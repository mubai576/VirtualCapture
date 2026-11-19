"""Four-way Raw L2 interop smoke: C++/Rust TX and C++/Rust RX over private Loopback.

Phase 1 runs `vc-sender.exe` (C++ TX) while `vc-receiver.exe` and
`vc-stream.exe receive` (Rust RX) capture the same stream concurrently.
Phase 2 runs `vc-stream.exe send` (Rust TX) while the same two receivers
capture a second stream.  Both phases use motion, packed 640x640 NV12,
MTU 9000, 60 fps, 120 frames and batch 1 unless overridden.  A default run
is a documented short functional send, not a capacity or latency claim:
the physical batch-69 burst has not been proven on Npcap Loopback, so
batch 1 is the default.  An explicit --batch value is passed verbatim,
with no retry or fallback.

The gate is functional, not a capacity or latency claim:
  * each hashlog has exactly frame ids 0..N-1, with no duplicate rows;
  * every sender/receiver hashlog from both phases agrees on every frame;
  * a selected subset of frames matches the independent
    `ctrl/pattern_ref.py::gen_native` FNV-1a64 reference.  When fewer than
    all frames are selected the report says so; the remaining frames are
    covered only by full actual-stream agreement, never claimed as
    reference-checked;
  * each process echoes the requested session id; Rust roles also echo the
    adapter in --ready/stats, while the legacy C++ adapter selection is read
    from its exact native stdout (`[receiver] adapter ...` and
    `[sender] adapter match ...`);
  * receiver stats report complete=N with 0 dropped, lost, CRC and hash
    mismatch counters; Rust RX also reports gaps=0, hash_rows=N and a SHM
    publish count; the C++ receiver runs with SHM off (shm_published=0);
    sender stats report N frames, all packets sent and 0 send errors;
    every process exits 0;
  * when Rust SHM is enabled, the private v3 mapping is opened while the
    Rust receiver is still alive (after its --ready marker), held across the
    run, and its layout plus latest published frame are validated after the
    writer exits.  The mapping handle is acquired before the sender starts,
    so the check does not depend on receiver-exit scheduling.  The Rust
    receiver publishes --ready only after capture, SHM and hash output open;
    the harness verifies the mapping and hashlog are visible at that point.

Only the exact `\\Device\\NPF_Loopback` adapter is used.  Each stream gets a
unique locally administered unicast source MAC and (per Rust receiver) a
unique `Local\\...` SHM name.  No DShow registration, agent/service, physical
NIC, PC2 or deployed 5143 runtime is touched.  C++ tool paths default to
`build/Release`; the Rust tool defaults to
`rust/target/release/vc-stream.exe`.

Evidence (stdout/stderr logs, stats and complete hashlogs for every process)
is kept under a unique gitignored `run/rust-raw-stream-smoke-<id>/` directory.
On success one JSON PASS record is printed and written to `report.json`;
on failure the evidence is preserved and one JSON FAIL record is printed and
written to `failure.json`.

Run from the repository root:
  python -I rust/tests/raw_stream_smoke.py
  python -I rust/tests/raw_stream_smoke.py --self-check
  python -I rust/tests/raw_stream_smoke.py --ref-samples 24 --no-shm
"""
from __future__ import annotations

import argparse
import ctypes
import ctypes.wintypes as wintypes
import json
import math
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import time
import traceback
import uuid

ROOT = Path(__file__).resolve().parents[2]
CTRL_DIR = ROOT / "ctrl"
if str(CTRL_DIR) not in sys.path:
    sys.path.insert(0, str(CTRL_DIR))
import pattern_ref  # noqa: E402  (independent Python mirror of sender/pattern.h)

LOOPBACK = r"\Device\NPF_Loopback"
CPP_RX_ADAPTER_MARKER = "[receiver] adapter "
CPP_TX_ADAPTER_MARKER = "[sender] adapter match "
DEST_MAC = "ff:ff:ff:ff:ff:ff"
ETHERNET_HEADER = 14
WIRE_HEADER = 28
PACKET_OVERHEAD = ETHERNET_HEADER + WIRE_HEADER
DEFAULT_CPP_DIR = ROOT / "build" / "Release"
DEFAULT_RUST_EXE = ROOT / "rust" / "target" / "release" / "vc-stream.exe"

SHM_MAGIC = 0x33414356  # "VCA3"
SHM_VERSION = 3
SHM_SLOTS = 12
SHM_DATA_OFF = 4096
SHM_HDR_FMT = "<IIIIIiiIIII"  # magic..fps, 44 B; 20 B reserved follow
SHM_HDR_SIZE = struct.calcsize(SHM_HDR_FMT)
SHM_SLOT_FMT = "<iIQQQI"  # seq..hash_fid, 36 B; 28 B reserved follow
SHM_SLOT_SIZE = struct.calcsize(SHM_SLOT_FMT)
FILE_MAP_READ = 0x0004

if sys.platform == "win32":
    K32 = ctypes.WinDLL("kernel32", use_last_error=True)
    K32.OpenFileMappingW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
    K32.OpenFileMappingW.restype = wintypes.HANDLE
    K32.MapViewOfFile.argtypes = [
        wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD,
        wintypes.DWORD, ctypes.c_size_t,
    ]
    K32.MapViewOfFile.restype = ctypes.c_void_p
    K32.UnmapViewOfFile.argtypes = [ctypes.c_void_p]
    K32.UnmapViewOfFile.restype = wintypes.BOOL
    K32.CloseHandle.argtypes = [wintypes.HANDLE]
    K32.CloseHandle.restype = wintypes.BOOL
else:
    K32 = None

RUN: Path | None = None
CHILDREN: list["Child"] = []
EVIDENCE: dict = {}


class SmokeError(RuntimeError):
    """A gate failure with evidence already on disk."""


def require(condition, message):
    if not condition:
        raise SmokeError(message)


def is_int(value):
    return type(value) is int


# ---------------------------------------------------------------------------
# Configuration and independent reference


def build_config(frames, fps, width, height, mtu, batch):
    require(is_int(frames) and frames >= 1, f"frames must be a positive integer, got {frames!r}")
    require(is_int(fps) and 1 <= fps <= 1000, f"fps must be 1..1000, got {fps!r}")
    require(is_int(batch) and 1 <= batch <= 10000, f"batch must be 1..10000, got {batch!r}")
    require(is_int(width) and 2 <= width <= 8192 and width % 2 == 0,
            f"width must be an even integer in 2..8192, got {width!r}")
    require(is_int(height) and 2 <= height <= 8192 and height % 2 == 0,
            f"height must be an even integer in 2..8192, got {height!r}")
    require(is_int(mtu) and PACKET_OVERHEAD < mtu <= 9042,
            f"mtu must be in {PACKET_OVERHEAD + 1}..9042, got {mtu!r}")
    frame_bytes = width * height * 3 // 2
    payload = mtu - PACKET_OVERHEAD
    total_packets = (frame_bytes + payload - 1) // payload
    require(total_packets <= 65535, f"{total_packets} packets/frame exceeds the frozen 16-bit total field")
    receiver_timeout = int(max(20.0, math.ceil(frames / fps) + 15.0))
    return {
        "frames": frames,
        "fps": fps,
        "width": width,
        "height": height,
        "mtu": mtu,
        "batch": batch,
        "frame_bytes": frame_bytes,
        "payload_bytes": payload,
        "total_packets": total_packets,
        "receiver_timeout_s": receiver_timeout,
        "phase_timeout_s": float(receiver_timeout + 15),
        "ready_timeout_s": 15.0,
    }


def sample_frames(frames, requested):
    """0/oversized request = all frames; otherwise an even spread incl. first and last."""
    require(is_int(requested) and requested >= 0, f"ref-samples must be >= 0, got {requested!r}")
    if requested == 0 or requested >= frames:
        return list(range(frames))
    if requested == 1:
        return [0]
    ids = {(index * (frames - 1)) // (requested - 1) for index in range(requested)}
    return sorted(ids)


def reference_hashes(frame_ids, width, height):
    """FNV-1a64 of packed Y+UV from the independent Python native-pattern mirror."""
    hashes = {}
    for fid in frame_ids:
        y_plane, uv_plane = pattern_ref.gen_native("motion", fid, width, height)
        hashes[fid] = pattern_ref.frame_hash_yuv(y_plane, uv_plane)
    return hashes


def make_local_mac():
    """Locally administered (0x02) unicast source MAC, unique per stream."""
    return "02:" + ":".join(f"{byte:02x}" for byte in os.urandom(5))


# ---------------------------------------------------------------------------
# Evidence validators


def read_json(path, label, timeout=3.0):
    deadline = time.monotonic() + timeout
    last_error = None
    while True:
        try:
            text = path.read_text(encoding="utf-8")
            return json.loads(text)
        except (OSError, json.JSONDecodeError) as error:
            last_error = error
            if time.monotonic() >= deadline:
                raise SmokeError(f"{label}: cannot read JSON {path}: {last_error}") from error
            time.sleep(0.05)


def parse_hashlog(path, frames, label):
    return {fid: row["hash"] for fid, row in parse_hash_rows(path, frames, label).items()}


def parse_hash_rows(path, frames, label):
    """Return rows by frame ID after validating coverage and normalizing FNV hashes."""
    require(path.is_file(), f"{label}: missing hashlog {path}")
    seen = {}
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as error:
            raise SmokeError(f"{label}: hashlog line {line_number} is not JSON: {error}") from error
        require(isinstance(row, dict), f"{label}: hashlog line {line_number} is not a JSON object")
        fid = row.get("frame_id")
        digest = row.get("hash")
        require(is_int(fid) and 0 <= fid < frames,
                f"{label}: hashlog line {line_number} has invalid frame_id {fid!r}")
        require(isinstance(digest, str) and len(digest) == 16
                and all(char in "0123456789abcdefABCDEF" for char in digest),
                f"{label}: hashlog line {line_number} has invalid FNV hash {digest!r}")
        require(fid not in seen, f"{label}: duplicate hashlog row for frame_id {fid}")
        row["hash"] = digest.lower()
        seen[fid] = row
    missing = sorted(set(range(frames)) - set(seen))
    extra = sorted(set(seen) - set(range(frames)))
    require(len(seen) == frames and not missing and not extra,
            f"{label}: expected exactly frame ids 0..{frames - 1}, "
            f"got {len(seen)} rows; missing={missing[:8]} extra={extra[:8]}")
    return seen


def require_session(value, session_id, label):
    require(value == session_id,
            f"{label}: session_id {value!r} != requested {session_id!r}")


def require_adapter(value, label):
    require(value == LOOPBACK, f"{label}: adapter {value!r} != {LOOPBACK!r}")


def validate_ready(ready, session_id, label):
    """Rust receiver --ready: created only after capture, SHM and hash output open."""
    require(isinstance(ready, dict), f"{label}: ready marker is not a JSON object")
    require_session(ready.get("session_id"), session_id, label)
    require_adapter(ready.get("adapter"), label)
    return {"session_id": ready["session_id"], "adapter": ready["adapter"]}


def validate_tx_stats(stats, rust, frames, total_packets, session_id, label):
    require(isinstance(stats, dict), f"{label}: stats is not a JSON object")
    require_session(stats.get("session_id"), session_id, label)
    errors = stats.get("send_errors")
    require(is_int(errors) and errors == 0, f"{label}: send_errors must be 0, got {errors!r}")
    sent = stats.get("sent_frames")
    require(is_int(sent) and sent == frames, f"{label}: sent_frames must be {frames}, got {sent!r}")
    adapter = None
    if rust:
        require_adapter(stats.get("adapter"), label)
        adapter = stats["adapter"]
        alias = stats.get("frames")
        require(is_int(alias) and alias == frames,
                f"{label}: frames must be {frames}, got {alias!r}")
        require(alias == sent, f"{label}: frames {alias} != sent_frames {sent}")
    packets = stats.get("sent_packets")
    expected_packets = frames * total_packets
    require(is_int(packets) and packets == expected_packets,
            f"{label}: sent_packets must be {expected_packets}, got {packets!r}")
    return {
        "session_id": stats.get("session_id"),
        "adapter": adapter,
        "frames": sent,
        "sent_frames": sent,
        "sent_packets": packets,
        "send_errors": errors,
    }


def validate_rx_stats(stats, rust, frames, session_id, shm_enabled, label):
    require(isinstance(stats, dict), f"{label}: stats is not a JSON object")
    require_session(stats.get("session_id"), session_id, label)
    complete = stats.get("complete")
    require(is_int(complete) and complete == frames,
            f"{label}: complete must be {frames}, got {complete!r}")
    for key in ("dropped", "packets_lost", "hash_mismatch"):
        value = stats.get(key)
        require(is_int(value) and value == 0, f"{label}: {key} must be 0, got {value!r}")
    adapter = None
    if rust:
        require_adapter(stats.get("adapter"), label)
        adapter = stats["adapter"]
        value = stats.get("crc_errors")
        require(is_int(value) and value == 0, f"{label}: crc_errors must be 0, got {value!r}")
        gaps = stats.get("gaps")
        require(is_int(gaps) and gaps == 0, f"{label}: gaps must be 0, got {gaps!r}")
        hash_rows = stats.get("hash_rows")
        require(is_int(hash_rows) and hash_rows == frames,
                f"{label}: hash_rows must equal complete={frames}, got {hash_rows!r}")
        published = stats.get("shm_published")
        expected_published = frames if shm_enabled else 0
        require(is_int(published) and published == expected_published,
                f"{label}: shm_published must be {expected_published} "
                f"({'SHM enabled' if shm_enabled else 'SHM disabled'}), got {published!r}")
    else:
        if "crc_errors" in stats:
            require(stats["crc_errors"] == 0,
                    f"{label}: crc_errors must be 0, got {stats['crc_errors']!r}")
        if "gaps" in stats:
            require(is_int(stats["gaps"]) and stats["gaps"] == 0,
                    f"{label}: gaps must be 0, got {stats['gaps']!r}")
        published = stats.get("shm_published", 0)
        require(is_int(published) and published == 0,
                f"{label}: C++ receiver must run with SHM off, shm_published={published!r}")
    return {
        "session_id": stats.get("session_id"),
        "adapter": adapter,
        "complete": complete,
        "dropped": stats["dropped"],
        "packets_lost": stats["packets_lost"],
        "hash_mismatch": stats["hash_mismatch"],
        "crc_errors": stats.get("crc_errors", 0),
        "gaps": stats.get("gaps"),
        "hash_rows": stats.get("hash_rows") if rust else None,
        "shm_published": published,
    }


def first_hash_mismatch(left, right):
    for fid in sorted(set(left) & set(right)):
        if left[fid] != right[fid]:
            return fid, left[fid], right[fid]
    return None


def require_no_mismatch(left, right, left_label, right_label, context):
    mismatch = first_hash_mismatch(left, right)
    if mismatch is not None:
        frame_id, left_hash, right_hash = mismatch
        raise SmokeError(
            f"{context}: {left_label} vs {right_label} FNV mismatch at frame "
            f"{frame_id}: {left_hash} != {right_hash}")


def validate_stream_agreement(streams, frames, context):
    """Every actual stream must cover all frames and agree on every hash."""
    require(streams, f"{context}: no streams to compare")
    for name, hashes in streams:
        require(len(hashes) == frames,
                f"{context}: {name} has {len(hashes)} frames, expected {frames}")
    base_name, base_hashes = streams[0]
    for name, hashes in streams[1:]:
        require_no_mismatch(base_hashes, hashes, base_name, name, context)
    return [name for name, _ in streams]


# ---------------------------------------------------------------------------
# Private Rust SHM v3 ABI probe


class ShmView:
    def __init__(self, handle, view):
        self.handle = handle
        self.view = view

    def read(self, offset, size):
        return ctypes.string_at(self.view + offset, size)

    def close(self):
        if self.view:
            K32.UnmapViewOfFile(self.view)
            self.view = None
        if self.handle:
            K32.CloseHandle(self.handle)
            self.handle = None


def open_shm_view(name, child, timeout, label):
    """Open the receiver-created mapping read-only; caller holds it across the run."""
    require(K32 is not None, "SHM probe requires Windows")
    deadline = time.monotonic() + timeout
    last_error = None
    while True:
        if child.proc.poll() is not None:
            child.close_logs()
            raise SmokeError(
                f"{label}: Rust receiver exited {child.proc.returncode} before its SHM mapping "
                f"could be opened; stderr tail:\n{tail(child.stderr_path)}")
        handle = K32.OpenFileMappingW(FILE_MAP_READ, False, name)
        if handle:
            view = K32.MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0)
            if not view:
                error = ctypes.get_last_error()
                K32.CloseHandle(handle)
                raise SmokeError(f"{label}: MapViewOfFile({name}) failed, win32 error {error}")
            return ShmView(handle, view)
        last_error = ctypes.get_last_error()
        if time.monotonic() >= deadline:
            raise SmokeError(
                f"{label}: private SHM mapping {name!r} did not appear within {timeout}s "
                f"(win32 error {last_error}); stderr tail:\n{tail(child.stderr_path)}")
        time.sleep(0.05)


def validate_shm(view, name, child, cfg, rust_rx_hashes, label):
    """Validate static v3 layout and the latest published frame after the writer exited."""
    header = struct.unpack(SHM_HDR_FMT, view.read(0, SHM_HDR_SIZE))
    (magic, version, slots, frame_bytes, data_off, latest_slot, latest_seq,
     writer_pid, width, height, fps) = header
    require(magic == SHM_MAGIC, f"{label}: SHM magic 0x{magic:08x} != 0x{SHM_MAGIC:08x}")
    require(version == SHM_VERSION, f"{label}: SHM version {version} != {SHM_VERSION}")
    require(slots == SHM_SLOTS, f"{label}: SHM slots {slots} != {SHM_SLOTS}")
    require(data_off == SHM_DATA_OFF, f"{label}: SHM data_off {data_off} != {SHM_DATA_OFF}")
    require(frame_bytes == cfg["frame_bytes"],
            f"{label}: SHM frame_bytes {frame_bytes} != {cfg['frame_bytes']}")
    require((width, height, fps) == (cfg["width"], cfg["height"], cfg["fps"]),
            f"{label}: SHM geometry {width}x{height}@{fps} != "
            f"{cfg['width']}x{cfg['height']}@{cfg['fps']}")
    require(writer_pid == child.proc.pid,
            f"{label}: SHM writer_pid {writer_pid} != Rust receiver pid {child.proc.pid}")
    require(0 <= latest_slot < SHM_SLOTS,
            f"{label}: SHM latest_slot {latest_slot} is outside 0..{SHM_SLOTS - 1}")
    require(latest_seq == cfg["frames"],
            f"{label}: SHM latest_seq {latest_seq} != {cfg['frames']} published frames")

    slot_offset = 64 + 64 * latest_slot
    seq1, frame_id, qpc_first, qpc_pub, slot_hash, hash_fid = struct.unpack(
        SHM_SLOT_FMT, view.read(slot_offset, SHM_SLOT_SIZE))
    require(seq1 % 2 == 0, f"{label}: latest SHM slot seq {seq1} is odd (still filling)")
    require(0 <= frame_id < cfg["frames"],
            f"{label}: latest SHM frame_id {frame_id} is outside 0..{cfg['frames'] - 1}")
    require(qpc_first > 0 and qpc_pub >= qpc_first,
            f"{label}: invalid SHM QPC fields first={qpc_first} pub={qpc_pub}")
    payload = view.read(data_off + latest_slot * frame_bytes, frame_bytes)
    seq2 = struct.unpack("<i", view.read(slot_offset, 4))[0]
    require(seq1 == seq2, f"{label}: SHM slot seq changed during read ({seq1} != {seq2})")

    actual = pattern_ref.fnv1a64_hex(payload)
    require(actual == rust_rx_hashes[frame_id],
            f"{label}: SHM frame {frame_id} FNV {actual} != Rust RX hashlog "
            f"{rust_rx_hashes[frame_id]}")
    require(hash_fid == frame_id,
            f"{label}: SHM hash_fid {hash_fid} != frame_id {frame_id} (frame was not hash-marked)")
    require(slot_hash == int(actual, 16),
            f"{label}: SHM slot.hash 0x{slot_hash:016x} != computed FNV 0x{int(actual, 16):016x}")
    return {
        "name": name,
        "magic": f"0x{magic:08x}",
        "version": version,
        "slots": slots,
        "frame_bytes": frame_bytes,
        "geometry": [width, height, fps],
        "writer_pid": writer_pid,
        "latest_slot": latest_slot,
        "latest_seq": latest_seq,
        "latest_frame_id": frame_id,
        "latest_hash": actual,
        "hash_marked": True,
        "qpc_pub_ns": qpc_pub,
    }


# ---------------------------------------------------------------------------
# Child processes and evidence


def tail(path, limit=2400):
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return "<missing log>"
    return text[-limit:].replace("\r", "")


def read_native_adapter(path, marker, expected, label):
    """Read the adapter line legacy C++ writes to its own stdout, unchanged."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError as error:
        raise SmokeError(f"{label}: cannot read native stdout {path}: {error}") from error
    for line in text.splitlines():
        if line.startswith(marker):
            reported = line[len(marker):].split(" (", 1)[0].strip()
            require(reported == expected,
                    f"{label}: native adapter {reported!r} != {expected!r} (line: {line!r})")
            return reported
    raise SmokeError(
        f"{label}: native stdout {path} has no {marker!r} line; tail:\n{text[-2400:]}")


class Child:
    def __init__(self, role, argv, directory):
        self.role = role
        self.argv = [str(item) for item in argv]
        self.stdout_path = directory / f"{role}.stdout.log"
        self.stderr_path = directory / f"{role}.stderr.log"
        self.cleanup_errors = []
        self._stdout = open(self.stdout_path, "wb")
        self._stderr = open(self.stderr_path, "wb")
        self._closed = False
        flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
        try:
            self.proc = subprocess.Popen(
                self.argv,
                cwd=str(ROOT),
                stdin=subprocess.DEVNULL,
                stdout=self._stdout,
                stderr=self._stderr,
                creationflags=flags,
            )
        except OSError as error:
            self.close_logs()
            raise SmokeError(f"{role}: failed to start {self.argv[0]}: {error}") from error
        CHILDREN.append(self)

    def close_logs(self):
        if self._closed:
            return []
        self._closed = True
        problems = []
        for name, handle in (("stdout", self._stdout), ("stderr", self._stderr)):
            try:
                handle.flush()
                handle.close()
            except OSError as error:
                problems.append(f"{name} log close failed: {type(error).__name__}: {error}")
        self.cleanup_errors.extend(problems)
        return problems

    def stop(self):
        if self.proc.poll() is None:
            try:
                self.proc.terminate()
                self.proc.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired) as error:
                self.cleanup_errors.append(
                    f"terminate/wait failed: {type(error).__name__}: {error}")
                try:
                    self.proc.kill()
                except OSError as kill_error:
                    self.cleanup_errors.append(
                        f"kill failed: {type(kill_error).__name__}: {kill_error}")
                try:
                    self.proc.wait(timeout=5)
                except (OSError, subprocess.TimeoutExpired) as wait_error:
                    self.cleanup_errors.append(
                        f"wait after kill failed: {type(wait_error).__name__}: {wait_error}")
        self.close_logs()
        return list(self.cleanup_errors)


def stop_all():
    """Stop leftovers, but report rather than hide cleanup failures."""
    errors = []
    for child in reversed(CHILDREN):
        try:
            problems = child.stop()
        except Exception as error:  # noqa: BLE001 - cleanup must not mask the primary failure
            errors.append(f"{child.role}: stop raised {type(error).__name__}: {error}")
            continue
        errors.extend(f"{child.role}: {problem}" for problem in problems)
    return errors


def wait_for_stats_ready(child, path, timeout, label):
    """C++ receivers have no --ready marker; their first live stats write proves capture is open."""
    deadline = time.monotonic() + timeout
    last_error = None
    while True:
        returncode = child.proc.poll()
        if returncode is not None:
            child.close_logs()
            raise SmokeError(
                f"{label}: receiver exited {returncode} before its first stats write; "
                f"stderr tail:\n{tail(child.stderr_path)}")
        if path.is_file():
            try:
                stats = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as error:
                last_error = error
            else:
                require(isinstance(stats, dict), f"{label}: first stats write is not a JSON object")
                require(stats.get("running") is not False,
                        f"{label}: receiver wrote final stats before any capture: {stats}")
                return stats
        if time.monotonic() >= deadline:
            raise SmokeError(
                f"{label}: receiver stats {path} did not become readable within {timeout}s "
                f"({last_error}); stderr tail:\n{tail(child.stderr_path)}")
        time.sleep(0.05)


def wait_for_path(child, path, timeout, label):
    deadline = time.monotonic() + timeout
    while True:
        returncode = child.proc.poll()
        if returncode is not None:
            child.close_logs()
            raise SmokeError(
                f"{label}: process exited {returncode} before writing {path}; "
                f"stderr tail:\n{tail(child.stderr_path)}")
        if path.is_file():
            return
        if time.monotonic() >= deadline:
            raise SmokeError(
                f"{label}: ready marker {path} did not appear within {timeout}s; "
                f"stderr tail:\n{tail(child.stderr_path)}")
        time.sleep(0.05)


def wait_children(children, timeout, label):
    pending = list(children)
    deadline = time.monotonic() + timeout
    while pending:
        for child in pending[:]:
            returncode = child.proc.poll()
            if returncode is None:
                continue
            child.close_logs()
            pending.remove(child)
            require(returncode == 0,
                    f"{label}: {child.role} exited {returncode}; "
                    f"stderr tail:\n{tail(child.stderr_path)}")
        if not pending:
            return
        if time.monotonic() >= deadline:
            raise SmokeError(
                f"{label}: timed out after {timeout}s waiting for "
                f"{', '.join(child.role for child in pending)}; logs in "
                f"{children[0].stdout_path.parent}")
        time.sleep(0.05)


def record_command(evidence, role, argv, directory):
    """Record the exact Popen command before spawning, so failures keep it."""
    evidence["commands"][role] = [str(item) for item in argv]
    evidence["logs"][role] = {
        "stdout": str(directory / f"{role}.stdout.log"),
        "stderr": str(directory / f"{role}.stderr.log"),
    }


# ---------------------------------------------------------------------------
# One phase: one sender, two concurrent receivers


def run_phase(phase_name, sender_kind, cfg, ref_ids, expected_ref, args, tools, source_mac):
    phase_dir = RUN / phase_name
    phase_dir.mkdir(parents=True)
    session_id = "rsmoke-" + uuid.uuid4().hex[:12]
    shm_name = None
    if not args.no_shm:
        shm_name = "Local\\VirtualCaptureRawStreamSmoke." + uuid.uuid4().hex

    paths = {
        "cpp-rx.stats": phase_dir / "cpp-rx.stats.json",
        "cpp-rx.hashlog": phase_dir / "cpp-rx.hash.jsonl",
        "rust-rx.stats": phase_dir / "rust-rx.stats.json",
        "rust-rx.hashlog": phase_dir / "rust-rx.hash.jsonl",
        "rust-rx.ready": phase_dir / "rust-rx.ready.json",
        "sender.stats": phase_dir / "sender.stats.json",
        "sender.hashlog": phase_dir / "sender.hash.jsonl",
    }
    evidence = {
        "directory": str(phase_dir),
        "source_mac": source_mac,
        "dest_mac": DEST_MAC,
        "session_id": session_id,
        "shm_name": shm_name,
        "commands": {},
        "logs": {},
        "stats": {
            "cpp-rx": str(paths["cpp-rx.stats"]),
            "rust-rx": str(paths["rust-rx.stats"]),
            "sender": str(paths["sender.stats"]),
        },
        "hashlogs": {
            "cpp-rx": str(paths["cpp-rx.hashlog"]),
            "rust-rx": str(paths["rust-rx.hashlog"]),
            "sender": str(paths["sender.hashlog"]),
        },
    }
    EVIDENCE.setdefault("phases", {})[phase_name] = evidence

    width = str(cfg["width"])
    height = str(cfg["height"])
    fps = str(cfg["fps"])
    mtu = str(cfg["mtu"])
    frames = str(cfg["frames"])
    receiver_timeout = str(cfg["receiver_timeout_s"])

    cpp_rx_argv = [
        tools["cpp_receiver"], "--adapter", LOOPBACK,
        "--width", width, "--height", height, "--fps", fps, "--mtu", mtu,
        "--source-mac", source_mac, "--session", session_id,
        "--stats", paths["cpp-rx.stats"], "--hashlog", paths["cpp-rx.hashlog"],
        "--timeout-s", receiver_timeout, "--expect-frames", frames,
    ]
    rust_rx_argv = [
        tools["rust"], "receive", "--adapter", LOOPBACK,
        "--source-mac", source_mac, "--session", session_id,
        "--width", width, "--height", height, "--fps", fps, "--mtu", mtu,
        "--expect-frames", frames, "--timeout-s", receiver_timeout,
        "--stats", paths["rust-rx.stats"], "--hashlog", paths["rust-rx.hashlog"],
        "--ready", paths["rust-rx.ready"],
    ]
    if shm_name:
        rust_rx_argv += ["--shm", shm_name]

    if sender_kind == "cpp":
        sender_argv = [
            tools["cpp_sender"], "--adapter", LOOPBACK, "--mode", "pattern",
            "--pattern", "motion", "--width", width, "--height", height,
            "--fps", fps, "--mtu", mtu, "--batch", str(cfg["batch"]),
            "--frames", frames, "--source-mac", source_mac, "--dst-mac", DEST_MAC,
            "--session", session_id,
            "--stats", paths["sender.stats"], "--hashlog", paths["sender.hashlog"],
        ]
    else:
        sender_argv = [
            tools["rust"], "send", "--adapter", LOOPBACK,
            "--source-mac", source_mac, "--dst-mac", DEST_MAC,
            "--session", session_id,
            "--width", width, "--height", height, "--fps", fps, "--mtu", mtu,
            "--batch", str(cfg["batch"]), "--frames", frames,
            "--stats", paths["sender.stats"], "--hashlog", paths["sender.hashlog"],
            "--pattern", "motion",
        ]

    label = phase_name
    record_command(evidence, "cpp-rx", cpp_rx_argv, phase_dir)
    cpp_rx = Child("cpp-rx", cpp_rx_argv, phase_dir)
    cpp_initial = wait_for_stats_ready(
        cpp_rx, paths["cpp-rx.stats"], cfg["ready_timeout_s"], f"{label}/cpp-rx")
    evidence["cpp_rx_initial_stats"] = cpp_initial

    record_command(evidence, "rust-rx", rust_rx_argv, phase_dir)
    rust_rx = Child("rust-rx", rust_rx_argv, phase_dir)
    wait_for_path(rust_rx, paths["rust-rx.ready"], cfg["ready_timeout_s"], f"{label}/rust-rx")
    ready = read_json(paths["rust-rx.ready"], f"{label}/rust-rx.ready")
    ready_summary = validate_ready(ready, session_id, f"{label}/rust-rx.ready")
    evidence["rust_ready"] = ready
    require(paths["rust-rx.hashlog"].is_file(),
            f"{label}/rust-rx: hashlog was not open before --ready was written")

    shm_view = None
    try:
        if shm_name:
            # Acquire the read-only view before TX starts and hold it while the
            # writer exits, so the ABI check is independent of exit timing.
            shm_view = open_shm_view(shm_name, rust_rx, cfg["ready_timeout_s"], f"{label}/rust-rx")
        time.sleep(0.15)

        record_command(evidence, "sender", sender_argv, phase_dir)
        sender = Child("sender", sender_argv, phase_dir)

        wait_children([cpp_rx, rust_rx, sender], cfg["phase_timeout_s"], label)

        evidence["exit_codes"] = {
            "cpp-rx": cpp_rx.proc.returncode,
            "rust-rx": rust_rx.proc.returncode,
            "sender": sender.proc.returncode,
        }

        native_adapters = None
        if sender_kind == "cpp":
            native_adapters = {
                "cpp-rx": read_native_adapter(
                    cpp_rx.stdout_path, CPP_RX_ADAPTER_MARKER, LOOPBACK, f"{label}/cpp-rx"),
                "sender": read_native_adapter(
                    sender.stdout_path, CPP_TX_ADAPTER_MARKER, LOOPBACK, f"{label}/sender"),
            }
            evidence["native_adapters"] = native_adapters

        tx_stats = read_json(paths["sender.stats"], f"{label}/sender.stats")
        cpp_stats = read_json(paths["cpp-rx.stats"], f"{label}/cpp-rx.stats")
        rust_stats = read_json(paths["rust-rx.stats"], f"{label}/rust-rx.stats")

        tx_summary = validate_tx_stats(
            tx_stats, sender_kind == "rust", cfg["frames"], cfg["total_packets"],
            session_id, f"{label}/sender")
        cpp_summary = validate_rx_stats(
            cpp_stats, False, cfg["frames"], session_id, False, f"{label}/cpp-rx")
        rust_summary = validate_rx_stats(
            rust_stats, True, cfg["frames"], session_id, shm_name is not None,
            f"{label}/rust-rx")

        sender_hashes = parse_hashlog(paths["sender.hashlog"], cfg["frames"], f"{label}/sender")
        cpp_hashes = parse_hashlog(paths["cpp-rx.hashlog"], cfg["frames"], f"{label}/cpp-rx")
        rust_hashes = parse_hashlog(paths["rust-rx.hashlog"], cfg["frames"], f"{label}/rust-rx")

        require_no_mismatch(sender_hashes, cpp_hashes, "sender", "cpp-rx", label)
        require_no_mismatch(sender_hashes, rust_hashes, "sender", "rust-rx", label)
        for fid in ref_ids:
            expected = expected_ref[fid]
            require(sender_hashes[fid] == expected,
                    f"{label}: sender frame {fid} FNV {sender_hashes[fid]} != "
                    f"pattern_ref {expected}")
            require(cpp_hashes[fid] == expected,
                    f"{label}: cpp-rx frame {fid} FNV {cpp_hashes[fid]} != "
                    f"pattern_ref {expected}")
            require(rust_hashes[fid] == expected,
                    f"{label}: rust-rx frame {fid} FNV {rust_hashes[fid]} != "
                    f"pattern_ref {expected}")

        shm_result = None
        if shm_view is not None:
            # The writer has exited, but our read-only view keeps the pagefile
            # section alive; validate layout and the latest frame now.
            shm_result = validate_shm(
                shm_view, shm_name, rust_rx, cfg, rust_hashes, f"{label}/rust-rx")
    finally:
        if shm_view is not None:
            shm_view.close()

    result = {
        "name": phase_name,
        "sender": sender_kind,
        "source_mac": source_mac,
        "dest_mac": DEST_MAC,
        "session_id": session_id,
        "frame_ids": f"0..{cfg['frames'] - 1} (unique; no duplicate rows)",
        "stats_summary": {
            "sender": tx_summary,
            "cpp-rx": cpp_summary,
            "rust-rx": rust_summary,
        },
        "native_adapters": native_adapters,
        "rust_ready": ready_summary,
        "stats_files": evidence["stats"],
        "hashlog_files": evidence["hashlogs"],
        "hashlogs_equal": True,
        "reference_frames_checked": len(ref_ids),
        "reference_frames_total": cfg["frames"],
        "exit_codes": evidence["exit_codes"],
        "commands": evidence["commands"],
        "logs": evidence["logs"],
        "shm": shm_result,
    }
    streams = {
        "sender": sender_hashes,
        "cpp-rx": cpp_hashes,
        "rust-rx": rust_hashes,
    }
    return result, streams


# ---------------------------------------------------------------------------
# Entry points


def require_tools(args):
    cpp_dir = args.cpp_dir.expanduser().resolve()
    tools = {
        "cpp_dir": str(cpp_dir),
        "cpp_sender": str(cpp_dir / "vc-sender.exe"),
        "cpp_receiver": str(cpp_dir / "vc-receiver.exe"),
        "rust": str(args.rust_exe.expanduser().resolve()),
    }
    require(Path(tools["cpp_sender"]).is_file(), f"C++ sender not found: {tools['cpp_sender']}")
    require(Path(tools["cpp_receiver"]).is_file(), f"C++ receiver not found: {tools['cpp_receiver']}")
    require(Path(tools["rust"]).is_file(),
            f"Rust vc-stream not found: {tools['rust']} (build rust/target/release/vc-stream.exe first)")
    return tools


def make_run_dir():
    base = ROOT / "run"
    while True:
        candidate = base / ("rust-raw-stream-smoke-" + uuid.uuid4().hex[:10])
        if not candidate.exists():
            candidate.mkdir(parents=True)
            return candidate


def execute(args):
    cfg = build_config(args.frames, args.fps, args.width, args.height, args.mtu, args.batch)
    EVIDENCE["config"] = cfg
    tools = require_tools(args)
    EVIDENCE["binaries"] = tools
    EVIDENCE["adapter"] = LOOPBACK

    macs = []
    while len(macs) < 2:
        candidate = make_local_mac()
        if candidate not in macs:
            macs.append(candidate)

    ref_ids = sample_frames(cfg["frames"], args.ref_samples)
    EVIDENCE["reference"] = {
        "frames_checked": len(ref_ids),
        "frames_total": cfg["frames"],
        "frame_ids": ref_ids,
    }
    started = time.perf_counter()
    expected_ref = reference_hashes(ref_ids, cfg["width"], cfg["height"])
    reference_elapsed = time.perf_counter() - started

    phases = []
    streams = []
    for phase_name, sender_kind, source_mac in (
            ("phase-1-cpp-tx", "cpp", macs[0]),
            ("phase-2-rust-tx", "rust", macs[1])):
        result, phase_streams = run_phase(
            phase_name, sender_kind, cfg, ref_ids, expected_ref, args, tools, source_mac)
        phases.append(result)
        streams.extend(
            (f"{phase_name}/{role}", hashes) for role, hashes in phase_streams.items())
    agreement = validate_stream_agreement(streams, cfg["frames"], "cross-phase")

    all_reference = len(ref_ids) == cfg["frames"]
    if all_reference:
        reference_check = (
            f"all {cfg['frames']} frames match ctrl/pattern_ref.py::gen_native")
    else:
        reference_check = (
            f"{len(ref_ids)}/{cfg['frames']} sampled frames match "
            f"ctrl/pattern_ref.py::gen_native; the remaining "
            f"{cfg['frames'] - len(ref_ids)} frames are covered only by full "
            f"actual-stream agreement, not by the Python reference")

    report = {
        "ok": True,
        "result": "PASS",
        "scope": ("local Npcap Loopback functional interop only; not dual-machine, "
                  "throughput, latency, long-run, DShow, or quality acceptance"),
        "run": str(RUN),
        "adapter": LOOPBACK,
        "ethertype": "0x88B5",
        "pattern": "motion",
        "batch_policy": ("default batch 1 because the physical batch-69 burst is not proven "
                         "on Npcap Loopback; an explicit --batch is used verbatim with no "
                         "retry or fallback"),
        "geometry": {
            "width": cfg["width"],
            "height": cfg["height"],
            "frame_bytes": cfg["frame_bytes"],
            "fps": cfg["fps"],
            "mtu": cfg["mtu"],
            "payload_bytes": cfg["payload_bytes"],
            "packets_per_frame": cfg["total_packets"],
            "batch": cfg["batch"],
            "frames": cfg["frames"],
        },
        "binaries": tools,
        "reference": {
            "module": "ctrl/pattern_ref.py::gen_native (motion)",
            "frames_checked": len(ref_ids),
            "frames_total": cfg["frames"],
            "all_frames": all_reference,
            "coverage": "all" if all_reference else "sampled",
            "elapsed_s": round(reference_elapsed, 3),
        },
        "agreement": {
            "streams": agreement,
            "frames_per_stream": cfg["frames"],
            "all_frames_equal": True,
        },
        "phases": phases,
        "checks": [
            "exact frame ids 0..N-1 with no duplicate rows in every sender/receiver hashlog",
            "every sender/receiver hashlog from both phases agrees on every FNV-1a64 hash",
            reference_check,
            "every process echoed the requested session id; Rust adapter fields and C++ "
            "native stdout both report \\Device\\NPF_Loopback",
            "0 receiver drops / lost packets / CRC errors / hash mismatches; Rust gaps=0 "
            "and hash_rows=N",
            "C++ receiver ran with SHM off (shm_published=0); Rust shm_published matches "
            "complete or is 0 with --no-shm",
            "sender reports N frames, N*packets_per_frame packets and 0 send errors",
            "all six C++/Rust processes exited 0",
        ],
        "evidence": EVIDENCE,
    }
    if any(phase["shm"] for phase in phases):
        report["checks"].append(
            "Rust private SHM v3 geometry, latest published frame, FNV and hash marker validated")
    else:
        report["checks"].append("Rust private SHM ABI check skipped (--no-shm)")

    return report


def self_check():
    cfg = build_config(120, 60, 640, 640, 9000, 1)
    require(cfg["frame_bytes"] == 614400, cfg)
    require(cfg["total_packets"] == 69, cfg)
    require(cfg["payload_bytes"] == 8958, cfg)
    require(parse_args(["--self-check"]).batch == 1, "default sender batch must be 1")
    require(parse_args(["--batch", "69"]).batch == 69,
            "explicit --batch override must be preserved verbatim")

    mac = make_local_mac()
    require(len(mac) == 17 and mac.startswith("02:"), mac)
    require(int(mac[:2], 16) & 1 == 0, "source MAC must be unicast")

    sample = sample_frames(120, 12)
    require(sample[0] == 0 and sample[-1] == 119 and len(sample) == 12, sample)
    require(sample_frames(120, 0) == list(range(120)), "ref-samples=0 must select all")
    require(sample_frames(120, 1) == [0], "single reference sample must be frame 0")

    y_plane, uv_plane = pattern_ref.gen_native("motion", 0, 640, 640)
    digest = pattern_ref.frame_hash_yuv(y_plane, uv_plane)
    # Cross-checked with build/Release/vc-sender.exe --selftest "VEC full motion 0"
    # and build/pattern-rs-check pattern-fixture motion 0 640 640.
    require(digest == "f6f53d2aeac3c9dc", f"unexpected pattern_ref motion frame 0 hash {digest}")

    session = "rsmoke-self"
    with tempfile.TemporaryDirectory(prefix="vc-raw-smoke-self-") as temp:
        temp = Path(temp)
        log = temp / "hashlog.jsonl"
        log.write_text(
            "\n".join(json.dumps({"frame_id": fid, "hash": f"{fid:016x}"}) for fid in range(3)) + "\n",
            encoding="utf-8")
        parsed = parse_hashlog(log, 3, "self-check")
        require(parsed == {0: "0000000000000000", 1: "0000000000000001",
                           2: "0000000000000002"}, parsed)
        log.write_text(
            "\n".join(json.dumps({"frame_id": fid, "hash": f"{fid:016x}"}) for fid in (0, 0, 2)) + "\n",
            encoding="utf-8")
        expect_smoke_error(lambda: parse_hashlog(log, 3, "self-check duplicate"),
                           "duplicate frame rows")
        log.write_text(
            "\n".join(json.dumps({"frame_id": fid, "hash": f"{fid:016x}"}) for fid in (0, 1)) + "\n",
            encoding="utf-8")
        expect_smoke_error(lambda: parse_hashlog(log, 3, "self-check short"),
                           "missing frame rows")

        rx_native = temp / "cpp-rx.stdout.log"
        rx_native.write_text(
            "[receiver] adapter \\Device\\NPF_Loopback (Npcap Loopback Adapter)\n",
            encoding="utf-8")
        require(read_native_adapter(
                    rx_native, CPP_RX_ADAPTER_MARKER, LOOPBACK, "self-check cpp-rx") == LOOPBACK,
                "C++ receiver adapter marker was not parsed")
        tx_native = temp / "cpp-sender.stdout.log"
        tx_native.write_text(
            "[sender] adapter match \\Device\\NPF_Loopback (Npcap Loopback Adapter)\n",
            encoding="utf-8")
        require(read_native_adapter(
                    tx_native, CPP_TX_ADAPTER_MARKER, LOOPBACK, "self-check cpp-tx") == LOOPBACK,
                "C++ sender adapter marker was not parsed")
        tx_native.write_text(
            "[sender] adapter match \\Device\\NPF_Ethernet (Other)\n", encoding="utf-8")
        expect_smoke_error(
            lambda: read_native_adapter(
                tx_native, CPP_TX_ADAPTER_MARKER, LOOPBACK, "self-check wrong-adapter"),
            "wrong native adapter")
        rx_native.write_text("no adapter line\n", encoding="utf-8")
        expect_smoke_error(
            lambda: read_native_adapter(
                rx_native, CPP_RX_ADAPTER_MARKER, LOOPBACK, "self-check missing-adapter"),
            "missing native adapter marker")

    cpp_tx = {"session_id": session, "sent_frames": 120,
              "sent_packets": 8280, "send_errors": 0}
    rust_tx = {"session_id": session, "adapter": LOOPBACK, "frames": 120,
               "sent_frames": 120, "sent_packets": 8280, "send_errors": 0}
    cpp_rx = {"session_id": session, "complete": 120, "dropped": 0,
              "packets_lost": 0, "hash_mismatch": 0, "gaps": 0,
              "shm_published": 0}
    rust_rx = {"session_id": session, "adapter": LOOPBACK, "complete": 120,
               "dropped": 0, "packets_lost": 0, "crc_errors": 0,
               "hash_mismatch": 0, "gaps": 0, "hash_rows": 120,
               "shm_published": 120}
    validate_tx_stats(cpp_tx, False, 120, 69, session, "self-check cpp-tx")
    validate_tx_stats(rust_tx, True, 120, 69, session, "self-check rust-tx")
    validate_rx_stats(cpp_rx, False, 120, session, False, "self-check cpp-rx")
    validate_rx_stats(rust_rx, True, 120, session, True, "self-check rust-rx")
    validate_rx_stats({**rust_rx, "shm_published": 0}, True, 120, session, False,
                      "self-check rust-rx no-shm")

    ready = validate_ready(
        {"session_id": session, "adapter": LOOPBACK, "pid": 123}, session, "self-check ready")
    require(ready == {"session_id": session, "adapter": LOOPBACK}, ready)
    expect_smoke_error(
        lambda: validate_ready({"session_id": "other", "adapter": LOOPBACK}, session,
                               "self-check ready-session"),
        "ready session mismatch")
    expect_smoke_error(
        lambda: validate_ready({"session_id": session, "adapter": "\\Device\\NPF_Ethernet"},
                               session, "self-check ready-adapter"),
        "ready adapter mismatch")

    expect_smoke_error(
        lambda: validate_rx_stats({**rust_rx, "crc_errors": 1}, True, 120, session, True,
                                  "self-check bad-crc"),
        "nonzero CRC errors")
    expect_smoke_error(
        lambda: validate_rx_stats({**rust_rx, "gaps": 1}, True, 120, session, True,
                                  "self-check gaps"),
        "nonzero frame gaps")
    expect_smoke_error(
        lambda: validate_rx_stats({**rust_rx, "hash_rows": 119}, True, 120, session, True,
                                  "self-check hash-rows"),
        "wrong hash row count")
    expect_smoke_error(
        lambda: validate_rx_stats({**rust_rx, "shm_published": 119}, True, 120, session, True,
                                  "self-check shm-count"),
        "wrong SHM publish count")
    expect_smoke_error(
        lambda: validate_rx_stats({**rust_rx, "shm_published": 120}, True, 120, session, False,
                                  "self-check shm-disabled"),
        "SHM publish count without --shm")
    expect_smoke_error(
        lambda: validate_rx_stats({**cpp_rx, "shm_published": 1}, False, 120, session, False,
                                  "self-check cpp-shm"),
        "C++ receiver SHM enabled")
    expect_smoke_error(
        lambda: validate_tx_stats({**rust_tx, "sent_packets": 0}, True, 120, 69, session,
                                  "self-check bad-packets"),
        "wrong packet count")
    expect_smoke_error(
        lambda: validate_tx_stats({**rust_tx, "frames": 119}, True, 120, 69, session,
                                  "self-check frames-alias"),
        "frames/sent_frames disagreement")
    expect_smoke_error(
        lambda: validate_tx_stats({**rust_tx, "session_id": "other"}, True, 120, 69, session,
                                  "self-check tx-session"),
        "TX session mismatch")
    expect_smoke_error(
        lambda: validate_tx_stats({**rust_tx, "adapter": "\\Device\\NPF_Ethernet"}, True,
                                  120, 69, session, "self-check tx-adapter"),
        "TX adapter mismatch")

    equal = {fid: f"{fid:016x}" for fid in range(3)}
    validate_stream_agreement([("a", equal), ("b", dict(equal))], 3,
                              "self-check agreement")
    require_no_mismatch(equal, dict(equal), "left", "right", "self-check equal")
    expect_smoke_error(
        lambda: validate_stream_agreement(
            [("a", equal), ("b", {**equal, 1: "deadbeefdeadbeef"})], 3,
            "self-check disagreement"),
        "cross-stream hash disagreement")
    expect_smoke_error(
        lambda: require_no_mismatch({0: "aa"}, {0: "bb"}, "left", "right",
                                    "self-check mismatch"),
        "hash mismatch with a formatted message")
    expect_smoke_error(
        lambda: validate_stream_agreement([("a", {0: "00"}), ("b", {0: "00"})], 3,
                                          "self-check short-stream"),
        "stream with missing frames")

    require(SHM_HDR_SIZE == 44 and SHM_SLOT_SIZE == 36,
            f"unexpected SHM struct sizes {SHM_HDR_SIZE}/{SHM_SLOT_SIZE}")
    require(64 + 64 * SHM_SLOTS <= SHM_DATA_OFF, "SHM slot table overruns data offset")

    return {
        "ok": True,
        "result": "PASS",
        "mode": "self-check",
        "scope": "pure Python only; no binaries, adapters, SHM mappings, or child processes",
        "checks": [
            "640x640 NV12 is 614400 bytes and fragments into 69 MTU-9000 packets",
            "default sender batch is 1 and explicit --batch is preserved verbatim",
            "source MAC is locally administered unicast",
            "reference sampling covers first and last frame",
            "pattern_ref motion frame 0 matches the C++/Rust golden hash",
            "hashlog parser enforces exact ids, no duplicates and no missing rows",
            "C++ and Rust TX/RX stats schemas pass, including session/adapter echoes, "
            "Rust gaps/hash_rows/shm_published and C++ SHM-off",
            "invalid stats, ready markers, native adapter lines and stream agreement are rejected",
            "SHM v3 structure sizes and data offset match vcam/vcam_shm.h",
        ],
    }


def expect_smoke_error(function, description):
    try:
        function()
    except SmokeError:
        return
    raise SmokeError(f"self-check: expected rejection for {description}")


def parse_args(argv=None):
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--cpp-dir", type=Path, default=DEFAULT_CPP_DIR,
                        help="directory containing vc-sender.exe/vc-receiver.exe "
                             "(default: build/Release)")
    parser.add_argument("--rust-exe", type=Path, default=DEFAULT_RUST_EXE,
                        help="Rust vc-stream.exe (default: rust/target/release/vc-stream.exe)")
    parser.add_argument("--frames", type=int, default=120, help="frames per stream (default: 120)")
    parser.add_argument("--fps", type=int, default=60, help="sender pacing fps (default: 60)")
    parser.add_argument("--batch", type=int, default=1,
                        help="sender batch size (default: 1; batch 69 is a physical-link "
                             "setting not proven on Npcap Loopback; an explicit value is used "
                             "verbatim with no retry or fallback)")
    parser.add_argument("--width", type=int, default=640, help="NV12 width, even (default: 640)")
    parser.add_argument("--height", type=int, default=640, help="NV12 height, even (default: 640)")
    parser.add_argument("--mtu", type=int, default=9000, help="L2 MTU (default: 9000)")
    parser.add_argument("--ref-samples", type=int, default=0,
                        help="frames checked against pattern_ref.py; 0 = all (default: 0)")
    parser.add_argument("--no-shm", action="store_true",
                        help="do not pass --shm to the Rust receiver and skip the ABI probe")
    parser.add_argument("--self-check", action="store_true",
                        help="run pure Python parser/reference/stats checks and exit")
    return parser.parse_args(argv)


def print_json(record):
    print(json.dumps(record, ensure_ascii=False))


def main(argv=None):
    global RUN, EVIDENCE
    args = parse_args(argv)
    if args.self_check:
        try:
            report = self_check()
        except Exception as error:  # noqa: BLE001 - one JSON failure record
            print_json({
                "ok": False,
                "result": "FAIL",
                "mode": "self-check",
                "error": f"{type(error).__name__}: {error}",
                "traceback": traceback.format_exc(),
            })
            return 1
        print_json(report)
        return 0

    if sys.platform != "win32":
        print_json({
            "ok": False,
            "result": "FAIL",
            "error": "this harness requires Windows, Npcap and the exact \\Device\\NPF_Loopback adapter",
        })
        return 1
    if K32 is None:
        print_json({"ok": False, "result": "FAIL", "error": "kernel32 is unavailable"})
        return 1

    RUN = make_run_dir()
    CHILDREN.clear()
    EVIDENCE = {"phases": {}}
    failure = None
    failure_traceback = None
    try:
        report = execute(args)
    except Exception as error:  # noqa: BLE001 - preserve evidence and emit one JSON record
        failure = error
        failure_traceback = traceback.format_exc()
    finally:
        cleanup_errors = stop_all()

    EVIDENCE["cleanup_errors"] = cleanup_errors
    if failure is None and cleanup_errors:
        failure = SmokeError("process cleanup failed: " + "; ".join(cleanup_errors))
        failure_traceback = ""

    if failure is not None:
        report = {
            "ok": False,
            "result": "FAIL",
            "run": str(RUN),
            "error": f"{type(failure).__name__}: {failure}",
            "traceback": failure_traceback,
            "cleanup_errors": cleanup_errors,
            "evidence": EVIDENCE,
        }
        try:
            (RUN / "failure.json").write_text(
                json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        except OSError:
            pass
        print_json(report)
        return 1

    report["cleanup_errors"] = cleanup_errors
    try:
        (RUN / "report.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    except OSError as error:
        print_json({
            "ok": False,
            "result": "FAIL",
            "run": str(RUN),
            "error": f"{type(error).__name__}: cannot write report.json: {error}",
            "traceback": traceback.format_exc(),
            "evidence": EVIDENCE,
        })
        return 1
    print_json(report)
    return 0


if __name__ == "__main__":
    sys.exit(main())
