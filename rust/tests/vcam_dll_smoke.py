"""Isolated smoke for the Rust DirectShow virtual camera DLL ``vc_vcam.dll``.

The real ``tools/vcam_verify/vcam_verify.cpp`` verifier is used unchanged in
its ``--dll`` mode: it calls ``DllGetClassObject`` in-process (no COM
registration), builds a private filter graph, connects the filter to its own
``IMemInputPin`` sink, hashes every delivered sample and checks the
``VIDEOINFOHEADER`` timestamp, then calls Stop/Disconnect, ``DllCanUnloadNow``
and ``FreeLibrary``.  This script only supplies input data and judges the
evidence.

Input data is a private SHM v3 mapping created by Python (ctypes) with a unique
``Local\\VirtualCaptureVcamSmoke.<token>`` name and passed to the child through
``--shm`` (which sets ``VCAM_FRAME_MAP`` for that process only).  The stable
``Local\\VirtualCaptureVcamFrame`` mapping, the registry, a registered filter,
the agent on :5143 and any Npcap adapter are never touched.

Scenarios
---------
* ``no-data``: mapping absent for the whole run; the filter must emit nothing.
* ``empty-ring``: a valid v3 mapping exists but ``latest_slot == -1``; the
  filter must still emit nothing (receiver up, no verified frame yet).
* ``delivery-repeat``: publish distinct NV12 frames after a delay, then stop
  publishing; the first delivered hash must be the first published frame (no
  fabricated pixels), every published frame must appear in order, and every
  later sample must repeat the last published frame.  The filter's own
  ``<map>.vcam`` feedback ring must agree (n invariants, REPEAT flags,
  monotonic ``deliver_ns``) and, at 240 fps, the repeat cadence must show the
  1 ms worker timer period (p90 repeat gap < 12 ms instead of the 15.6 ms
  default tick).
* ``geometry-*``: the same delivery gate at a second session geometry.
* ``stop-blocked`` / ``stop-held-samples`` / ``disconnect-running``: the
  verifier's sink blocks ``Receive``, holds allocator samples, or disconnects
  while running; Stop must still finish (the verifier gates < 2000 ms), all
  samples must stay valid, and the DLL must report ``DllCanUnloadNow()==S_OK``
  before ``FreeLibrary``.

Evidence is written to a fresh ``run/rust-vcam-dll-smoke-<token>/`` directory:
per-scenario stdout/stderr/hashlogs, ``report.json`` on success and
``failure.json`` on failure.  Exit code 0 means every gate passed.

Usage (repository root)::

    python -I rust/tests/vcam_dll_smoke.py
    python -I rust/tests/vcam_dll_smoke.py --dll rust/target/release/vc_vcam.dll \
        --verify build/rust-package-native/Release/vc-vcam-verify.exe
    python -I rust/tests/vcam_dll_smoke.py --self-check

This is an isolated, same-machine DirectShow-filter smoke.  It is not a
two-machine, real-game, stability or image-quality acceptance.
"""
from __future__ import annotations

import argparse
import ctypes
import ctypes.wintypes as wintypes
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import threading
import time
import traceback
import uuid

REPO = Path(__file__).resolve().parents[2]
RUN_ROOT = REPO / "run"
DEFAULT_DLL = REPO / "rust" / "target" / "release" / "vc_vcam.dll"
DEFAULT_VERIFY = REPO / "build" / "rust-package-native" / "Release" / "vc-vcam-verify.exe"
SHM_PREFIX = "Local\\VirtualCaptureVcamSmoke."

SHM_MAGIC = 0x33414356  # "VCA3"
SHM_VERSION = 3
SHM_SLOTS = 12
SHM_DATA_OFF = 4096
SHM_HDR_FMT = "<IIIIIiiIIII"   # 44 B, 20 B reserved follow
SHM_SLOT_FMT = "<iIQQQI"       # 36 B, 28 B reserved follow
SHM_HDR_SIZE = 64
SHM_SLOT_SIZE = 64
FILE_MAP_ALL_ACCESS = 0x000F001F

VSTAT_MAGIC = 0x54535656  # "VVST"
VSTAT_CAP = 1024
VSTAT_HDR_SIZE = 16
VSTAT_ENTRY_SIZE = 32
VSTAT_SIZE = VSTAT_HDR_SIZE + VSTAT_CAP * VSTAT_ENTRY_SIZE  # 32784
VSTAT_FLAG_REPEAT = 1
VSTAT_HDR_FMT = "<IiII"      # magic, widx, cap, reserved
VSTAT_ENTRY_FMT = "<IIQQII"  # n, frame_id, pub_ns, deliver_ns, flags, reserved

RESULT_RE = re.compile(
    r"DLL stream (\d+)x(\d+)@(\d+) samples=(\d+) invalid=(\d+) hr=0x([0-9a-fA-F]+) (PASS|FAIL)")
STOP_RE = re.compile(r"Stop/Disconnect ([0-9.]+) ms")


class SmokeError(RuntimeError):
    """A verifier, SHM or evidence gate failed."""


def require(condition, message):
    if not condition:
        raise SmokeError(message)


def fnv1a64(data):
    value = 0xCBF29CE484222325
    for byte in data:
        value = ((value ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return value


def fnv1a64_hex(data):
    return f"{fnv1a64(data):016x}"


if os.name == "nt":
    _k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _k32.CreateFileMappingW.argtypes = [
        ctypes.c_void_p, ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD,
        wintypes.DWORD, wintypes.LPCWSTR]
    _k32.CreateFileMappingW.restype = ctypes.c_void_p
    _k32.MapViewOfFile.argtypes = [
        ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
        ctypes.c_size_t]
    _k32.MapViewOfFile.restype = ctypes.c_void_p
    _k32.UnmapViewOfFile.argtypes = [ctypes.c_void_p]
    _k32.UnmapViewOfFile.restype = wintypes.BOOL
    _k32.CloseHandle.argtypes = [ctypes.c_void_p]
    _k32.CloseHandle.restype = wintypes.BOOL
    _k32.QueryPerformanceCounter.argtypes = [ctypes.POINTER(ctypes.c_int64)]
    _k32.QueryPerformanceCounter.restype = wintypes.BOOL
    _k32.QueryPerformanceFrequency.argtypes = [ctypes.POINTER(ctypes.c_int64)]
    _k32.QueryPerformanceFrequency.restype = wintypes.BOOL
    _k32.FlushProcessWriteBuffers.argtypes = []
    _k32.FlushProcessWriteBuffers.restype = None
else:  # pragma: no cover - diagnostic only
    _k32 = None


def qpc_ns():
    counter = ctypes.c_int64()
    frequency = ctypes.c_int64()
    if not _k32.QueryPerformanceCounter(ctypes.byref(counter)):
        raise SmokeError("QueryPerformanceCounter failed")
    if not _k32.QueryPerformanceFrequency(ctypes.byref(frequency)) or frequency.value <= 0:
        raise SmokeError("QueryPerformanceFrequency failed")
    freq = frequency.value
    return (counter.value // freq) * 1_000_000_000 + (counter.value % freq) * 1_000_000_000 // freq


# ---------------------------------------------------------------------------
# Private SHM v3 writer (mirrors vcam/vcam_shm.h / scripts/vc_shm.py)


class ShmWriter:
    def __init__(self, name, width, height, fps):
        require(_k32 is not None, "SHM writer requires Windows")
        require(name.startswith("Local\\") and name != "Local\\VirtualCaptureVcamFrame",
                f"refusing non-private mapping name {name!r}")
        require(width >= 2 and height >= 2 and width % 2 == 0 and height % 2 == 0
                and width <= 8192 and height <= 8192 and 1 <= fps <= 1000,
                f"invalid geometry {width}x{height}@{fps}")
        self.name = name
        self.width, self.height, self.fps = width, height, fps
        self.frame_bytes = width * height * 3 // 2
        self.size = SHM_DATA_OFF + SHM_SLOTS * self.frame_bytes
        self.handle = _k32.CreateFileMappingW(
            ctypes.c_void_p(-1), None, 0x04, 0, self.size, name)
        if not self.handle:
            raise SmokeError(f"CreateFileMappingW({name!r}) failed: {ctypes.get_last_error()}")
        self.view = _k32.MapViewOfFile(
            self.handle, FILE_MAP_ALL_ACCESS, 0, 0, self.size)
        if not self.view:
            error = ctypes.get_last_error()
            _k32.CloseHandle(self.handle)
            raise SmokeError(f"MapViewOfFile({name!r}) failed: {error}")
        ctypes.memset(self.view, 0, self.size)
        self.counter = 0
        self._write_header(-1, 0)

    def _write_header(self, latest_slot, latest_seq):
        header = struct.pack(
            SHM_HDR_FMT, SHM_MAGIC, SHM_VERSION, SHM_SLOTS, self.frame_bytes,
            SHM_DATA_OFF, latest_slot, latest_seq, os.getpid(),
            self.width, self.height, self.fps)
        header += b"\0" * (SHM_HDR_SIZE - len(header))
        ctypes.memmove(self.view, header, SHM_HDR_SIZE)

    def _slot_addr(self, slot):
        return self.view + SHM_HDR_SIZE + SHM_SLOT_SIZE * slot

    def _data_addr(self, slot):
        return self.view + SHM_DATA_OFF + slot * self.frame_bytes

    def publish(self, frame, frame_id):
        require(len(frame) == self.frame_bytes,
                f"frame is {len(frame)} bytes, expected {self.frame_bytes}")
        slot = self.counter % SHM_SLOTS
        seq_even = 2 * self.counter + 2
        addr = self._slot_addr(slot)
        # seqlock: odd seq (filling), payload, slot fields, even seq last.
        ctypes.memmove(addr, struct.pack("<i", seq_even - 1), 4)
        ctypes.memmove(self._data_addr(slot), bytes(frame), self.frame_bytes)
        digest = fnv1a64(frame)
        fields = struct.pack(
            "<IQQQI28x", frame_id, qpc_ns(), qpc_ns(), digest, frame_id)
        ctypes.memmove(addr + 4, fields, SHM_SLOT_SIZE - 4)
        ctypes.memmove(addr, struct.pack("<i", seq_even), 4)
        _k32.FlushProcessWriteBuffers()
        ctypes.memmove(self.view + 20, struct.pack("<i", slot), 4)
        ctypes.memmove(self.view + 24, struct.pack("<i", self.counter + 1), 4)
        _k32.FlushProcessWriteBuffers()
        self.counter += 1
        return digest

    def close(self):
        if self.view:
            _k32.UnmapViewOfFile(self.view)
            self.view = None
        if self.handle:
            _k32.CloseHandle(self.handle)
            self.handle = None


class VstatRing:
    """Local view of the filter's ``<map>.vcam`` delivery feedback ring.

    Created before the verifier starts, so the filter's worker attaches to
    this mapping and appends one entry per delivered sample (fresh and
    repeat).  Reading it after the verifier exits gives the filter's own
    delivery timeline (QPC ``deliver_ns``) and its REPEAT flags, which the
    hashlog alone cannot show.
    """

    def __init__(self, name):
        require(_k32 is not None, "vstat ring requires Windows")
        self.name = f"{name}.vcam"
        self.handle = _k32.CreateFileMappingW(
            ctypes.c_void_p(-1), None, 0x04, 0, VSTAT_SIZE, self.name)
        if not self.handle:
            raise SmokeError(
                f"CreateFileMappingW({self.name!r}) failed: {ctypes.get_last_error()}")
        self.view = _k32.MapViewOfFile(self.handle, FILE_MAP_ALL_ACCESS, 0, 0, VSTAT_SIZE)
        if not self.view:
            error = ctypes.get_last_error()
            _k32.CloseHandle(self.handle)
            self.handle = None
            raise SmokeError(f"MapViewOfFile({self.name!r}) failed: {error}")
        ctypes.memset(self.view, 0, VSTAT_SIZE)

    def read(self):
        magic, widx, cap, _reserved = struct.unpack_from(
            VSTAT_HDR_FMT, ctypes.string_at(self.view, VSTAT_HDR_SIZE))
        count = min(max(widx, 0), VSTAT_CAP)
        start = max(widx, 0) - count
        entries = []
        for index in range(count):
            fields = struct.unpack_from(
                VSTAT_ENTRY_FMT,
                ctypes.string_at(self.view + VSTAT_HDR_SIZE + index * VSTAT_ENTRY_SIZE,
                                 VSTAT_ENTRY_SIZE))
            entries.append({
                "n": fields[0],
                "frame_id": fields[1],
                "pub_ns": fields[2],
                "deliver_ns": fields[3],
                "flags": fields[4],
                "expected_n": start + index + 1,
            })
        return {"magic": magic, "widx": widx, "cap": cap, "entries": entries}

    def close(self):
        if self.view:
            _k32.UnmapViewOfFile(self.view)
            self.view = None
        if self.handle:
            _k32.CloseHandle(self.handle)
            self.handle = None


def make_frame(frame_bytes, frame_id):
    """Deterministic, cheap, distinct-per-frame NV12 content."""
    pattern = bytes(range(256))
    frame = bytearray((pattern * (frame_bytes // 256 + 1))[:frame_bytes])
    frame[0:4] = struct.pack("<I", frame_id)
    frame[4] = (frame_id * 73 + 11) & 0xFF
    return frame


# ---------------------------------------------------------------------------
# Publisher thread


class Publisher(threading.Thread):
    def __init__(self, writer, plan, stop_event):
        super().__init__(name="vcam-smoke-publisher", daemon=True)
        self.writer = writer
        self.plan = plan  # (initial_delay_s, interval_s, frame_ids)
        self.stop_event = stop_event
        self.expected = []
        self.error = None

    def run(self):
        delay, interval, frame_ids = self.plan
        if self.stop_event.wait(delay):
            return
        try:
            for frame_id in frame_ids:
                frame = make_frame(self.writer.frame_bytes, frame_id)
                self.expected.append((frame_id, fnv1a64_hex(frame)))
                self.writer.publish(frame, frame_id)
                if self.stop_event.wait(interval):
                    return
        except Exception as exc:  # noqa: BLE001 - surfaced in the report
            self.error = f"{type(exc).__name__}: {exc}"


# ---------------------------------------------------------------------------
# Verifier execution


def parse_stream_lines(text):
    parsed = None
    for match in RESULT_RE.finditer(text):
        parsed = {
            "width": int(match.group(1)),
            "height": int(match.group(2)),
            "fps": int(match.group(3)),
            "samples": int(match.group(4)),
            "invalid": int(match.group(5)),
            "hr": int(match.group(6), 16),
            "verdict": match.group(7),
        }
    stop_ms = None
    for match in STOP_RE.finditer(text):
        stop_ms = float(match.group(1))
    return parsed, stop_ms


def parse_hashlog(path, label):
    path = Path(path)
    require(path.is_file(), f"{label}: hashlog missing: {path}")
    rows = []
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as exc:
            raise SmokeError(f"{label}: hashlog line {number} is not JSON: {exc}") from exc
        require(isinstance(row, dict) and isinstance(row.get("hash"), str)
                and len(row["hash"]) == 16, f"{label}: bad hashlog row {number}: {row!r}")
        rows.append(row)
    return rows


def run_verifier(verify, dll, args, env_name, run_dir, label):
    cmd = [str(verify), "--dll", str(dll)] + args
    if env_name is not None:
        cmd += ["--shm", env_name]
    env = dict(os.environ)
    env.pop("VCAM_FRAME_MAP", None)
    if env_name is not None:
        env["VCAM_FRAME_MAP"] = env_name
    stdout_path = run_dir / f"{label}.stdout.txt"
    stderr_path = run_dir / f"{label}.stderr.txt"
    started = qpc_ns()
    with open(stdout_path, "wb") as out, open(stderr_path, "wb") as err:
        completed = subprocess.run(cmd, env=env, stdout=out, stderr=err,
                                   cwd=str(REPO), timeout=120)
    finished = qpc_ns()
    stdout = stdout_path.read_text(encoding="utf-8", errors="replace")
    stderr = stderr_path.read_text(encoding="utf-8", errors="replace")
    parsed, stop_ms = parse_stream_lines(stdout)
    return {
        "label": label,
        "command": cmd,
        "env": {"VCAM_FRAME_MAP": env_name},
        "exit_code": completed.returncode,
        "started_qpc_ns": started,
        "finished_qpc_ns": finished,
        "duration_ms": (finished - started) / 1e6,
        "stdout_path": str(stdout_path),
        "stderr_path": str(stderr_path),
        "stdout": stdout,
        "stderr": stderr,
        "stream": parsed,
        "stop_ms": stop_ms,
    }


def require_verifier_pass(record, expected_geometry, label):
    require(record["exit_code"] == 0,
            f"{label}: verifier exit {record['exit_code']}; stderr={record['stderr'].strip()!r} "
            f"stdout={record['stdout'].strip()!r}")
    stream = record["stream"]
    require(stream is not None, f"{label}: verifier did not report a stream result")
    require(stream["verdict"] == "PASS", f"{label}: verifier verdict {stream['verdict']}")
    require(stream["invalid"] == 0, f"{label}: verifier counted {stream['invalid']} invalid samples")
    require((stream["width"], stream["height"], stream["fps"]) == expected_geometry,
            f"{label}: verifier stream {stream['width']}x{stream['height']}@{stream['fps']} "
            f"!= {expected_geometry}")
    require(record["stop_ms"] is not None and record["stop_ms"] < 2000.0,
            f"{label}: Stop ms {record['stop_ms']!r}")
    return stream


def analyse_delivery(rows, expected, label):
    """Gate ordered delivery of every published frame and last-frame repeats."""
    hashes = [row["hash"].lower() for row in rows]
    require(hashes, f"{label}: hashlog is empty")
    expected_hashes = [digest for _, digest in expected]
    require(hashes[0] == expected_hashes[0],
            f"{label}: first delivered hash {hashes[0]} != first published "
            f"{expected_hashes[0]} (fabricated/foreign frame before data?)")
    foreign = sorted(set(hashes) - set(expected_hashes))
    require(not foreign, f"{label}: delivered hashes not published by the writer: {foreign[:4]}")
    index = 0
    seen = []
    first_new_index = []
    for position, digest in enumerate(hashes):
        if index < len(expected_hashes) and digest == expected_hashes[index]:
            seen.append(digest)
            first_new_index.append(position)
            index += 1
    require(len(seen) == len(expected_hashes),
            f"{label}: only {len(seen)}/{len(expected_hashes)} published frames delivered in order")
    tail = hashes[first_new_index[-1] + 1:]
    last_hash = expected_hashes[-1]
    require(tail and all(digest == last_hash for digest in tail),
            f"{label}: samples after the last published frame are not all repeats of "
            f"{last_hash}: {sorted(set(tail))[:4]}")
    require(len(tail) >= 10, f"{label}: only {len(tail)} repeat samples observed")
    starts = [int(row.get("start_100ns", -1)) for row in rows]
    require(all(starts[i] < starts[i + 1] for i in range(len(starts) - 1)),
            f"{label}: sample timestamps are not strictly increasing")
    return {
        "rows": len(rows),
        "distinct_delivered": len(set(hashes)),
        "first_hash": hashes[0],
        "last_published_hash": last_hash,
        "published_frames_seen_in_order": len(seen),
        "repeat_samples": len(tail),
        "all_hashes_published": True,
        "first_sample_start_100ns": starts[0],
        "last_sample_start_100ns": starts[-1],
        "timestamps_strictly_increasing": True,
    }


def analyse_vstat(ring, expected, fps, label):
    """Gate the worker's feedback ring and its repeat delivery cadence."""
    snapshot = ring.read()
    require(snapshot["magic"] == VSTAT_MAGIC,
            f"{label}: vstat magic 0x{snapshot['magic']:08x} != 0x{VSTAT_MAGIC:08x}")
    require(snapshot["cap"] == VSTAT_CAP,
            f"{label}: vstat cap {snapshot['cap']} != {VSTAT_CAP}")
    require(snapshot["widx"] > 0, f"{label}: vstat ring recorded no deliveries")
    entries = snapshot["entries"]
    broken = [entry for entry in entries if entry["n"] != entry["expected_n"]]
    require(not broken,
            f"{label}: {len(broken)} vstat entries with a broken n/order invariant, "
            f"first {broken[:2]}")
    require(all(entries[i]["deliver_ns"] <= entries[i + 1]["deliver_ns"]
                for i in range(len(entries) - 1)),
            f"{label}: vstat deliver_ns is not monotonic")
    fresh = [entry for entry in entries if entry["flags"] == 0]
    repeats = [entry for entry in entries if entry["flags"] & VSTAT_FLAG_REPEAT]
    require(bool(fresh), f"{label}: vstat recorded no fresh delivery")
    require(len(repeats) >= 10,
            f"{label}: only {len(repeats)} repeat deliveries in vstat")
    require(all(entry["frame_id"] == expected[-1][0] for entry in repeats[-10:]),
            f"{label}: last vstat repeats do not carry the last published frame_id "
            f"{expected[-1][0]}")
    result = {
        "magic": snapshot["magic"],
        "widx": snapshot["widx"],
        "cap": snapshot["cap"],
        "fresh": len(fresh),
        "repeats": len(repeats),
        "first_deliver_ns": entries[0]["deliver_ns"],
        "last_deliver_ns": entries[-1]["deliver_ns"],
    }
    if fps >= 120 and len(repeats) >= 50:
        # The worker must run at 1 ms timer resolution (`timeBeginPeriod(1)`,
        # like the C++ worker).  Without it the 240 fps repeat deadline rounds
        # to the 15.6 ms default tick: roughly one in four repeat intervals
        # stalls for ~15.6 ms (so p90 lands on the tick), while a 1 ms period
        # keeps the p90 repeat gap within a few ms of the 4.17 ms grid.
        gaps = sorted(
            repeats[i + 1]["deliver_ns"] - repeats[i]["deliver_ns"]
            for i in range(len(repeats) - 1)
            if repeats[i + 1]["deliver_ns"] >= repeats[i]["deliver_ns"])
        require(gaps, f"{label}: no usable vstat repeat intervals")
        p90 = gaps[min(len(gaps) - 1, (len(gaps) * 9) // 10)]
        require(p90 < 12_000_000,
                f"{label}: repeat delivery p90 {p90 / 1e6:.2f} ms >= 12 ms; the worker "
                "is not running at 1 ms timer resolution")
        result["repeat_gap_p90_ms"] = round(p90 / 1e6, 3)
    return result


# ---------------------------------------------------------------------------
# Scenarios


def scenario_no_data(ctx, run_dir):
    label = "no-data"
    record = run_verifier(
        ctx["verify"], ctx["dll"],
        ["--width", "640", "--height", "640", "--fps", "240",
         "--seconds", "2", "--allow-empty"],
        ctx["next_private_name"](), run_dir, label)
    stream = require_verifier_pass(record, (640, 640, 240), label)
    require(stream["samples"] == 0,
            f"{label}: filter emitted {stream['samples']} samples with no SHM data")
    record["gate"] = "no mapping -> zero samples"
    return record


def scenario_empty_ring(ctx, run_dir):
    label = "empty-ring"
    name = ctx["next_private_name"]()
    writer = ShmWriter(name, 640, 640, 240)  # valid header, latest_slot=-1
    try:
        record = run_verifier(
            ctx["verify"], ctx["dll"],
            ["--width", "640", "--height", "640", "--fps", "240",
             "--seconds", "2", "--allow-empty"],
            name, run_dir, label)
        stream = require_verifier_pass(record, (640, 640, 240), label)
        require(stream["samples"] == 0,
                f"{label}: filter emitted {stream['samples']} samples before any publication")
        record["gate"] = "valid v3 mapping with no publication -> zero samples"
        return record
    finally:
        writer.close()


def scenario_delivery(ctx, run_dir, label, width, height, fps, seconds, frame_ids, interval):
    name = ctx["next_private_name"]()
    writer = ShmWriter(name, width, height, fps)
    stop = threading.Event()
    publisher = Publisher(writer, (0.8, interval, frame_ids), stop)
    hashlog = run_dir / f"{label}.hashlog.jsonl"
    vstat = None
    record = None
    try:
        # Created before the verifier starts: the filter attaches to this
        # mapping and appends one entry per delivery to it.
        vstat = VstatRing(name)
        publisher.start()
        record = run_verifier(
            ctx["verify"], ctx["dll"],
            ["--width", str(width), "--height", str(height), "--fps", str(fps),
             "--seconds", str(seconds), "--hashlog", str(hashlog)],
            name, run_dir, label)
        stop.set()
        publisher.join(timeout=5.0)
        require(not publisher.is_alive(), f"{label}: publisher thread did not stop")
        require(publisher.error is None, f"{label}: publisher failed: {publisher.error}")
        stream = require_verifier_pass(record, (width, height, fps), label)
        require(stream["samples"] > 0, f"{label}: verifier received no samples")
        rows = parse_hashlog(hashlog, label)
        record["hashlog_path"] = str(hashlog)
        record["shm"] = {"name": name, "width": width, "height": height,
                         "fps": fps, "frame_bytes": writer.frame_bytes,
                         "published_frames": len(frame_ids)}
        record["delivery"] = analyse_delivery(rows, publisher.expected, label)
        record["vstat"] = analyse_vstat(vstat, publisher.expected, fps, label)
        record["gate"] = ("first sample is frame 0; all published frames in order; "
                          "tail repeats last; vstat ring and repeat cadence")
        return record
    finally:
        if vstat is not None:
            vstat.close()
        stop.set()
        writer.close()


def scenario_blocked(ctx, run_dir):
    label = "stop-blocked"
    name = ctx["next_private_name"]()
    writer = ShmWriter(name, 640, 640, 240)
    stop = threading.Event()
    publisher = Publisher(writer, (0.7, 0.1, [0, 1, 2]), stop)
    hashlog = run_dir / f"{label}.hashlog.jsonl"
    try:
        publisher.start()
        record = run_verifier(
            ctx["verify"], ctx["dll"],
            ["--width", "640", "--height", "640", "--fps", "240",
             "--seconds", "2", "--block-receive", "--hashlog", str(hashlog)],
            name, run_dir, label)
        stop.set()
        publisher.join(timeout=5.0)
        require(publisher.error is None, f"{label}: publisher failed: {publisher.error}")
        stream = require_verifier_pass(record, (640, 640, 240), label)
        require(stream["samples"] >= 1, f"{label}: expected at least one sample before blocking")
        record["gate"] = "sink blocked in Receive; Stop/BeginFlush/Decommit/join finished"
        record["hashlog_path"] = str(hashlog) if hashlog.is_file() else None
        return record
    finally:
        stop.set()
        writer.close()


def scenario_held(ctx, run_dir):
    label = "stop-held-samples"
    name = ctx["next_private_name"]()
    writer = ShmWriter(name, 640, 640, 240)
    stop = threading.Event()
    publisher = Publisher(writer, (0.7, 0.15, [0, 1]), stop)
    hashlog = run_dir / f"{label}.hashlog.jsonl"
    try:
        publisher.start()
        record = run_verifier(
            ctx["verify"], ctx["dll"],
            ["--width", "640", "--height", "640", "--fps", "240",
             "--seconds", "1", "--hold-samples", "--hashlog", str(hashlog)],
            name, run_dir, label)
        stop.set()
        publisher.join(timeout=5.0)
        require(publisher.error is None, f"{label}: publisher failed: {publisher.error}")
        stream = require_verifier_pass(record, (640, 640, 240), label)
        require(stream["samples"] >= 1, f"{label}: expected at least one held sample")
        record["gate"] = "sink held allocator samples; Stop releases them and joins the worker"
        record["hashlog_path"] = str(hashlog) if hashlog.is_file() else None
        return record
    finally:
        stop.set()
        writer.close()


def scenario_disconnect(ctx, run_dir):
    label = "disconnect-running"
    name = ctx["next_private_name"]()
    writer = ShmWriter(name, 640, 640, 240)
    stop = threading.Event()
    publisher = Publisher(writer, (0.7, 0.1, [0, 1, 2]), stop)
    hashlog = run_dir / f"{label}.hashlog.jsonl"
    try:
        publisher.start()
        record = run_verifier(
            ctx["verify"], ctx["dll"],
            ["--width", "640", "--height", "640", "--fps", "240",
             "--seconds", "2", "--disconnect-running", "--hashlog", str(hashlog)],
            name, run_dir, label)
        stop.set()
        publisher.join(timeout=5.0)
        require(publisher.error is None, f"{label}: publisher failed: {publisher.error}")
        stream = require_verifier_pass(record, (640, 640, 240), label)
        require(stream["samples"] >= 1, f"{label}: expected samples before Disconnect")
        record["gate"] = "IPin::Disconnect while RUNNING; then Stop and DllCanUnloadNow==S_OK"
        record["hashlog_path"] = str(hashlog) if hashlog.is_file() else None
        return record
    finally:
        stop.set()
        writer.close()


SCENARIOS = ("no-data", "empty-ring", "delivery-repeat", "geometry-416",
             "stop-blocked", "stop-held-samples", "disconnect-running")


def execute(args, dll, verify, run_dir, token):
    private_index = [0]

    def next_private_name():
        private_index[0] += 1
        return f"{SHM_PREFIX}{token}.{private_index[0]}"

    ctx = {"dll": dll, "verify": verify, "next_private_name": next_private_name}
    report = {
        "status": "RUNNING",
        "scope": "isolated in-process DShow source filter via vc-vcam-verify --dll; "
                 "private SHM v3 only; no registration, registry, agent, Npcap or stable mapping",
        "run_dir": str(run_dir),
        "token": token,
        "dll": file_identity(dll),
        "verify": file_identity(verify),
        "environment": {
            "python": sys.version,
            "isolated": bool(sys.flags.isolated),
            "pid": os.getpid(),
            "cwd": str(Path.cwd()),
        },
        "scenarios": {},
    }
    selected = args.scenario
    for name in SCENARIOS:
        if selected and name not in selected:
            continue
        if name == "no-data":
            report["scenarios"][name] = scenario_no_data(ctx, run_dir)
        elif name == "empty-ring":
            report["scenarios"][name] = scenario_empty_ring(ctx, run_dir)
        elif name == "delivery-repeat":
            report["scenarios"][name] = scenario_delivery(
                ctx, run_dir, name, 640, 640, 240, 3, list(range(8)), 0.05)
        elif name == "geometry-416":
            report["scenarios"][name] = scenario_delivery(
                ctx, run_dir, name, 416, 416, 60, 2, list(range(6)), 0.05)
        elif name == "stop-blocked":
            report["scenarios"][name] = scenario_blocked(ctx, run_dir)
        elif name == "stop-held-samples":
            report["scenarios"][name] = scenario_held(ctx, run_dir)
        elif name == "disconnect-running":
            report["scenarios"][name] = scenario_disconnect(ctx, run_dir)
        print(json.dumps({"scenario": name, "status": "PASS",
                          "detail": report["scenarios"][name].get("gate")},
                         ensure_ascii=False), flush=True)
    return report


def file_identity(path):
    path = Path(path).resolve()
    require(path.is_file(), f"missing file: {path}")
    with path.open("rb") as handle:
        digest = hashlib.file_digest(handle, "sha256").hexdigest()
    return {"path": str(path), "size": path.stat().st_size, "sha256": digest}


# ---------------------------------------------------------------------------
# Self-check (never loads the DLL)


def self_check():
    require(ctypes.sizeof(ctypes.c_void_p) == 8, "self-check expects 64-bit Python")
    require(SHM_HDR_SIZE == 64 and SHM_SLOT_SIZE == 64, "SHM header/slot sizes")
    require(SHM_MAGIC == 0x33414356 and SHM_VERSION == 3 and SHM_SLOTS == 12,
            "SHM v3 constants")
    require(VSTAT_MAGIC == 0x54535656 and VSTAT_CAP == 1024 and VSTAT_SIZE == 32_784
            and VSTAT_HDR_SIZE == 16 and VSTAT_ENTRY_SIZE == 32,
            "vstat ring constants")
    frame = make_frame(614_400, 0)
    require(len(frame) == 614_400 and fnv1a64_hex(frame) != fnv1a64_hex(make_frame(614_400, 1)),
            "frame generator")
    name = f"{SHM_PREFIX}self-check-{uuid.uuid4().hex[:8]}"
    writer = ShmWriter(name, 4, 2, 60)
    try:
        digest = writer.publish(make_frame(writer.frame_bytes, 5), 5)
        header = struct.unpack_from(SHM_HDR_FMT, ctypes.string_at(writer.view, 44))
        require(header[0] == SHM_MAGIC and header[1] == SHM_VERSION and header[5] == 0
                and header[6] == 1, f"SHM header after publish: {header}")
        slot = struct.unpack_from(SHM_SLOT_FMT, ctypes.string_at(writer._slot_addr(0), 36))
        payload = ctypes.string_at(writer._data_addr(0), writer.frame_bytes)
        require(slot[0] == 2 and slot[1] == 5 and slot[5] == 5, f"SHM slot: {slot}")
        require(fnv1a64(payload) == digest, "SHM payload hash")
    finally:
        writer.close()
    checks = {
        "python_isolated": bool(sys.flags.isolated),
        "shm_header_size": SHM_HDR_SIZE,
        "shm_slot_size": SHM_SLOT_SIZE,
        "shm_v3_constants": [SHM_MAGIC, SHM_VERSION, SHM_SLOTS, SHM_DATA_OFF],
        "vstat_constants": [VSTAT_MAGIC, VSTAT_CAP, VSTAT_SIZE],
        "fnv_frame0_640": fnv1a64_hex(make_frame(614_400, 0)),
        "dll_loaded": False,
        "note": "--self-check never loads vc_vcam.dll and is not a live substitute",
    }
    return checks


def parse_args(argv=None):
    parser = argparse.ArgumentParser(
        description="Isolated vc_vcam.dll DirectShow smoke using vc-vcam-verify --dll.")
    parser.add_argument("--dll", type=Path, default=DEFAULT_DLL,
                        help=f"vc_vcam.dll path (default {DEFAULT_DLL})")
    parser.add_argument("--verify", type=Path, default=DEFAULT_VERIFY,
                        help=f"vc-vcam-verify.exe path (default {DEFAULT_VERIFY})")
    parser.add_argument("--scenario", action="append", choices=SCENARIOS, default=None,
                        help="run only the named scenario (repeatable)")
    parser.add_argument("--self-check", action="store_true",
                        help="check pure layout/hash helpers; never loads the DLL")
    return parser.parse_args(argv)


def make_run_dir():
    RUN_ROOT.mkdir(parents=True, exist_ok=True)
    for _ in range(100):
        token = uuid.uuid4().hex[:12]
        path = RUN_ROOT / f"rust-vcam-dll-smoke-{token}"
        try:
            path.mkdir()
            return path, token
        except FileExistsError:
            continue
    raise SmokeError(f"cannot create a unique run directory under {RUN_ROOT}")


def write_json(path, value):
    with open(path, "x", encoding="utf-8") as handle:
        json.dump(value, handle, ensure_ascii=False, indent=2, sort_keys=True)
        handle.write("\n")


def main(argv=None):
    args = parse_args(argv)
    if args.self_check:
        try:
            checks = self_check()
        except Exception as exc:  # noqa: BLE001
            print(json.dumps({"self_check": "FAIL", "error": str(exc),
                              "traceback": traceback.format_exc()}))
            return 1
        print(json.dumps({"self_check": "PASS", **checks}, ensure_ascii=False, sort_keys=True))
        return 0

    try:
        dll = args.dll.resolve()
        verify = args.verify.resolve()
        file_identity(dll)
        file_identity(verify)
        run_dir, token = make_run_dir()
    except Exception as exc:  # noqa: BLE001
        print(json.dumps({"status": "FAIL", "phase": "arguments", "error": str(exc)}))
        return 2

    report = {
        "status": "RUNNING",
        "run_dir": str(run_dir),
        "token": token,
        "dll_path": str(dll),
        "verify_path": str(verify),
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    try:
        report.update(execute(args, dll, verify, run_dir, token))
        report["status"] = "PASS"
        report["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        write_json(run_dir / "report.json", report)
        print(json.dumps({
            "status": "PASS",
            "run_dir": str(run_dir),
            "run_json": str(run_dir / "report.json"),
            "dll_sha256": report["dll"]["sha256"],
            "verify_sha256": report["verify"]["sha256"],
            "scenarios": {
                name: {
                    "samples": entry.get("stream", {}).get("samples"),
                    "stop_ms": entry.get("stop_ms"),
                    "gate": entry.get("gate"),
                }
                for name, entry in report["scenarios"].items()
            },
        }, ensure_ascii=False, sort_keys=True))
        return 0
    except Exception as exc:  # noqa: BLE001 - always preserve failure evidence
        report["status"] = "FAIL"
        report["failure"] = {
            "error": str(exc),
            "type": type(exc).__name__,
            "traceback": traceback.format_exc(),
        }
        try:
            write_json(run_dir / "failure.json", report)
        except OSError as write_error:
            report["failure"]["evidence_error"] = str(write_error)
        print(json.dumps({
            "status": "FAIL",
            "run_dir": str(run_dir),
            "run_json": str(run_dir / "failure.json"),
            "error": str(exc),
        }, ensure_ascii=False))
        return 1


if __name__ == "__main__":
    sys.exit(main())
