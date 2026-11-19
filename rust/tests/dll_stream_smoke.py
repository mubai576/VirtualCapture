"""Same-process `vc_stream.dll` ABI smoke and same-host latency probe.

The script loads the real `vc_stream.dll` into *this* Python process (ctypes,
cdecl C ABI v1), starts an in-process receiver session, waits for its ready
marker, opens its private SHM v3 mapping read-only, then starts an in-process
sender session.  Both sessions run on the exact Npcap Loopback adapter
(`\\Device\\NPF_Loopback`) with a unique locally administered source MAC and a
unique `Local\\...` SHM name.  Raw is the default; MJPEG and PyroWave are
explicit opt-in encodings with explicit configuration.

There are no subprocesses: no `vc-sender.exe`, no `vc-receiver.exe`, no
`vc-stream.exe`, no `vc-vcam-verify.exe`.  No DirectShow filter is registered
and no registry, service, agent, physical NIC, PC2 or deployed runtime is
touched.  The only kernel setup is the in-process Npcap Loopback handle and the
receiver-created session-private SHM mapping.

Gates
-----
* ABI: `vc_module_get_api(1)` returns a v1 struct with the exact frozen layout;
  unknown versions return NULL.  Failed `start_session` calls return a negative
  code, clear the output handle, and report the full error length even when the
  caller's error buffer is tiny.  NULL handles are rejected.
* Lifecycle: `start_session` is asynchronous; readiness is polled with a
  bounded deadline; `stop_session`/`destroy_session` return 0 when done, 1 while
  the worker is still unwinding, and never a negative code for valid handles.
  A session destroyed while pending is retried until it is freed, and the DLL
  stays loaded for the whole process.
* Coverage: every TX and RX hash row for frame ids 0..N-1 is required, all TX
  and RX rows must agree, and the configured motion pattern is checked against
  the independent Python reference in `ctrl/pattern_ref.py` (all frames by
  default; `--ref-samples` selects a spread and the report says so).
* Raw: the final published SHM NV12 frame is read through the v3 seqlock
  layout and must hash to the receiver hashlog row for the same frame id, with
  `hash_fid == frame_id`.
* Encoded: every sender `transport_hash` must equal the receiver
  `.transport.jsonl` row and the receiver hashlog `transport_hash`; the final
  SHM NV12 frame must hash to the receiver `decoded_hash` for that frame.  The
  source hash is never compared to the decoded hash.
* Errors: missing frames, session errors, Raw hash mismatches and missing
  codec/shared-library dependencies are failures, never skips.

Performance numbers are same-host QPC deltas taken from the real hash rows.
They are never a two-machine, physical-link or capacity claim.  Default smoke
only checks that the configured frame cadence is plausible for a finite run;
`--performance` additionally applies the documented cadence/latency gate
(mean TX frame-start and send-start intervals within 1% of the target period,
with p99 send-to-RX and source-start-to-RX both under 10 ms).  This stricter
local diagnostic gate does not replace the frozen test-matrix/D1 gates.

Evidence is written to a fresh `run/rust-dll-stream-smoke-<id>/` directory and
is never overwritten.  `report.json` records the DLL SHA-256, configuration,
requests, ready/final status snapshots, stats, hash summaries, SHM summary,
metrics, and cleanup results.  Failures are written to `failure.json`.

Usage (from the repository root)::

    python -I rust/tests/dll_stream_smoke.py --package path/to/extracted
    python -I rust/tests/dll_stream_smoke.py --package path/to/extracted --codec mjpeg --jpeg-quality 85
    python -I rust/tests/dll_stream_smoke.py --dll rust/target/release/vc_stream.dll --performance
    python -I rust/tests/dll_stream_smoke.py --self-check

`--self-check` only exercises pure ctypes/layout/hash helpers; it never loads
the DLL and is never a substitute for the live run.
"""
from __future__ import annotations

import argparse
import copy
import ctypes
import ctypes.wintypes as wintypes
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import struct
import sys
import tempfile
import time
import traceback
import uuid

# The documented `-I` invocation excludes the script directory from sys.path.
HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))
import raw_stream_smoke as smoke  # noqa: E402  (shared config/reference/evidence helpers)

ABI_VERSION = 1
ROLE_SENDER = 1
ROLE_RECEIVER = 2
DLL_NAME = "vc_stream.dll"
ERROR_CAPACITY = 4096
TEXT_LIMIT = 4 * 1024 * 1024
LIMIT_NS = 10_000_000
DEFAULT_FRAMES = 120
DEFAULT_FPS = 60
DEFAULT_WIDTH = 640
DEFAULT_HEIGHT = 640
DEFAULT_MTU = 9000
DEFAULT_REF_SAMPLES = 0  # 0 = every frame
DEST_MAC = [0xFF] * 6
# One-second send budgets from the encoded runbooks: MJPEG diagnostics use
# 800 Mbps and PyroWave 400 Mbps, both above a full intra frame at 60 fps, so
# the rate window is exercised without silently throttling the smoke cadence.
DEFAULT_BITRATE_BPS = {"raw": 80_000_000, "mjpeg": 800_000_000, "pyrowave": 400_000_000}
SHM_PREFIX = "Local\\VirtualCaptureDllSmoke."
SHM_HEADER_BYTES = 64
SHM_SLOT_BYTES = 64
POLL_SECONDS = 0.02


class DllError(RuntimeError):
    """A data-plane ABI/lifecycle failure."""


class PendingError(RuntimeError):
    """A session could not be stopped/freed before the bounded deadline."""


class UntrackedSession(RuntimeError):
    """A probe unexpectedly started a session that could not be freed."""


def _require(condition, message):
    # Keep the shared SmokeError type so callers see one validation family.
    smoke.require(condition, message)


def _is_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


# ---------------------------------------------------------------------------
# Frozen ABI v1


class VcStreamApiV1(ctypes.Structure):
    """Exact `#[repr(C)] VcStreamApiV1` layout from vc-stream/src/abi.rs."""

    _fields_ = [
        ("abi_version", ctypes.c_uint32),
        ("struct_size", ctypes.c_uint32),
        ("list_adapters", ctypes.c_void_p),
        ("start_session", ctypes.c_void_p),
        ("session_status", ctypes.c_void_p),
        ("stop_session", ctypes.c_void_p),
        ("destroy_session", ctypes.c_void_p),
        ("capabilities", ctypes.c_void_p),
    ]


TEXT_QUERY = ctypes.CFUNCTYPE(
    ctypes.c_int,
    ctypes.POINTER(ctypes.c_char), ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32),
    ctypes.POINTER(ctypes.c_char), ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32),
)
START_SESSION = ctypes.CFUNCTYPE(
    ctypes.c_int,
    ctypes.c_uint32, ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p),
    ctypes.POINTER(ctypes.c_char), ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32),
)
SESSION_STATUS = ctypes.CFUNCTYPE(
    ctypes.c_int,
    ctypes.c_void_p, ctypes.POINTER(ctypes.c_char), ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint32), ctypes.POINTER(ctypes.c_char), ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint32),
)
STOP_SESSION = ctypes.CFUNCTYPE(
    ctypes.c_int,
    ctypes.c_void_p, ctypes.POINTER(ctypes.c_char), ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint32),
)
DESTROY_SESSION = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p)


def _error_text(buffer, written, capacity):
    raw = bytes(buffer.raw[:capacity])
    head = raw.split(b"\0", 1)[0]
    if not head and written:
        head = raw[:min(int(written), capacity)].rstrip(b"\0")
    return head.decode("utf-8", "replace")


def _invoke_text(call, label):
    """Two-call text query with caller-owned buffers and bounded retries."""
    error = ctypes.create_string_buffer(ERROR_CAPACITY)
    error_written = ctypes.c_uint32(0)
    required = ctypes.c_uint32(0)
    code = call(None, 0, ctypes.pointer(required), error, ERROR_CAPACITY,
                ctypes.pointer(error_written))
    if code < 0:
        raise DllError(f"{label}: query failed ({code}): {_error_text(error, error_written.value, ERROR_CAPACITY)}")
    if code != 1:
        raise DllError(f"{label}: buffer-too-small discovery returned {code}, expected 1")
    for _ in range(8):
        need = int(required.value)
        if not 0 < need <= TEXT_LIMIT:
            raise DllError(f"{label}: invalid required text length {need}")
        output = ctypes.create_string_buffer(need)
        written = ctypes.c_uint32(0)
        error_written = ctypes.c_uint32(0)
        code = call(output, need, ctypes.pointer(written), error, ERROR_CAPACITY,
                    ctypes.pointer(error_written))
        if code == 1:
            required.value = written.value
            continue
        if code < 0:
            raise DllError(f"{label}: query failed ({code}): {_error_text(error, error_written.value, ERROR_CAPACITY)}")
        if code != 0:
            raise DllError(f"{label}: unexpected query result code {code}")
        size = int(written.value)
        if size == 0 or size > need:
            raise DllError(f"{label}: invalid written length {size} for capacity {need}")
        data = bytes(output.raw[:size])
        if not data.endswith(b"\0"):
            raise DllError(f"{label}: result is not NUL-terminated")
        try:
            text = data[:-1].decode("utf-8")
        except UnicodeDecodeError as exc:
            raise DllError(f"{label}: result is not UTF-8: {exc}") from exc
        try:
            return json.loads(text), text
        except json.JSONDecodeError as exc:
            raise DllError(f"{label}: result is not JSON: {exc}") from exc
    raise DllError(f"{label}: text result changed too quickly to read")


def _invoke_start(call, role, request, error_capacity):
    """Call the raw v1 start entry point with a sentinel output handle."""
    handle = ctypes.c_void_p(0x5A5A5A5A)
    error = ctypes.create_string_buffer(error_capacity)
    error_written = ctypes.c_uint32(0)
    code = call(role, request, ctypes.pointer(handle), error, error_capacity,
                ctypes.pointer(error_written))
    return {
        "code": code,
        "handle": handle.value,
        "error": _error_text(error, error_written.value, error_capacity),
        "error_written": int(error_written.value),
        "error_capacity": error_capacity,
    }


class DllApi:
    """Owns the loaded module and typed v1 entry points."""

    def __init__(self, library, struct_value):
        self._library = library  # kept for the whole process; never unloaded early
        self.struct = struct_value
        self.list_adapters_fn = TEXT_QUERY(struct_value.list_adapters)
        self.start_fn = START_SESSION(struct_value.start_session)
        self.status_fn = SESSION_STATUS(struct_value.session_status)
        self.stop_fn = STOP_SESSION(struct_value.stop_session)
        self.destroy_fn = DESTROY_SESSION(struct_value.destroy_session)
        self.capabilities_fn = TEXT_QUERY(struct_value.capabilities)

    def list_adapters(self):
        return _invoke_text(self.list_adapters_fn, "list_adapters")

    def capabilities(self):
        return _invoke_text(self.capabilities_fn, "capabilities")

    def start(self, role, request, label):
        payload = json.dumps(request, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        result = _invoke_start(self.start_fn, role, payload, ERROR_CAPACITY)
        if result["code"] != 0:
            if result["handle"] is not None:
                raise DllError(f"{label}: start_session failed but did not clear the output handle "
                               f"(0x{int(result['handle']):x})")
            raise DllError(f"{label}: start_session failed ({result['code']}): {result['error']}")
        handle = result["handle"]
        if not handle:
            raise DllError(f"{label}: start_session returned success with a null handle")
        return int(handle)

    def status(self, handle, label):
        def call(out, capacity, written, error, error_capacity, error_written):
            return self.status_fn(handle, out, capacity, written, error, error_capacity, error_written)
        value, _ = _invoke_text(call, label)
        return value

    def stop(self, handle, label):
        error = ctypes.create_string_buffer(ERROR_CAPACITY)
        error_written = ctypes.c_uint32(0)
        code = self.stop_fn(handle, error, ERROR_CAPACITY, ctypes.pointer(error_written))
        if code < 0:
            raise DllError(f"{label}: stop_session failed ({code}): "
                           f"{_error_text(error, error_written.value, ERROR_CAPACITY)}")
        if code not in (0, 1):
            raise DllError(f"{label}: stop_session returned unexpected code {code}")
        return int(code)

    def destroy(self, handle, label):
        code = self.destroy_fn(handle)
        if code < 0:
            raise DllError(f"{label}: destroy_session failed ({code}) for handle 0x{int(handle):x}")
        if code not in (0, 1):
            raise DllError(f"{label}: destroy_session returned unexpected code {code}")
        return int(code)


def _load_api(dll_path, report):
    _require(dll_path.is_file(), f"vc_stream.dll not found: {dll_path}")
    try:
        library = ctypes.CDLL(str(dll_path))
    except OSError as exc:
        raise DllError(f"cannot load data-plane DLL {dll_path}: {exc}") from exc
    try:
        get_api = library.vc_module_get_api
    except AttributeError as exc:
        raise DllError(f"{dll_path} does not export vc_module_get_api") from exc
    get_api.argtypes = [ctypes.c_uint32]
    get_api.restype = ctypes.c_void_p
    rejected = {}
    for version in (0, 2, 3, 0xFFFFFFFF):
        rejected[str(version)] = get_api(version) is None
    if not all(rejected.values()):
        raise DllError(f"vc_module_get_api accepted an unknown ABI version: {rejected}")
    address = get_api(ABI_VERSION)
    if not address:
        raise DllError(f"{dll_path} does not implement data-plane ABI v{ABI_VERSION}")
    prefix = ctypes.cast(address, ctypes.POINTER(ctypes.c_uint32))
    version, struct_size = int(prefix[0]), int(prefix[1])
    if version != ABI_VERSION:
        raise DllError(f"{dll_path} reported ABI version {version}, expected {ABI_VERSION}")
    native_size = ctypes.sizeof(VcStreamApiV1)
    if struct_size < native_size:
        raise DllError(f"{dll_path} reported ABI struct_size {struct_size} < {native_size}")
    struct_value = VcStreamApiV1.from_address(address)
    for name, _ in VcStreamApiV1._fields_:
        if name in ("abi_version", "struct_size"):
            continue
        if not getattr(struct_value, name):
            raise DllError(f"{dll_path} returned a null ABI function pointer for {name}")
    api = DllApi(library, struct_value)
    report["abi"] = {
        "requested_version": ABI_VERSION,
        "abi_version": version,
        "struct_size": struct_size,
        "native_struct_size": native_size,
        "pointer_size": ctypes.sizeof(ctypes.c_void_p),
        "unknown_versions_rejected": rejected,
        "module": DLL_NAME,
    }
    return api


# ---------------------------------------------------------------------------
# Session status / lifecycle


def _check_status(status, session_id, label):
    _require(isinstance(status, dict), f"{label}: status is not a JSON object")
    _require(status.get("session_id") == session_id,
             f"{label}: session_id {status.get('session_id')!r} != {session_id!r}")
    _require(status.get("pid") == os.getpid(),
             f"{label}: status pid {status.get('pid')!r} != live host pid {os.getpid()}")
    _require(status.get("module") == DLL_NAME,
             f"{label}: status module {status.get('module')!r} != {DLL_NAME!r}")
    _require(status.get("abi_version") == ABI_VERSION,
             f"{label}: status abi_version {status.get('abi_version')!r} != {ABI_VERSION}")
    return status


def _brief(status):
    keys = ("ready", "running", "stopping", "exit_code", "frames", "hash_rows",
            "transport_frames", "shm_published", "error")
    return {key: status.get(key) for key in keys if key in status}


def _wait_ready(api, handle, session_id, label, timeout):
    deadline = time.monotonic() + timeout
    last = None
    while True:
        status = _check_status(api.status(handle, label), session_id, label)
        last = status
        if status.get("ready") is True:
            return status
        if status.get("running") is False:
            raise DllError(f"{label}: worker ended before ready: {_brief(status)}")
        if time.monotonic() >= deadline:
            raise DllError(f"{label}: ready not observed within {timeout:.1f}s: {_brief(last)}")
        time.sleep(POLL_SECONDS)


def _wait_done(api, handle, session_id, label, deadline):
    while True:
        status = _check_status(api.status(handle, label), session_id, label)
        if status.get("running") is False:
            if status.get("exit_code") != 0:
                raise DllError(f"{label}: worker failed (exit_code={status.get('exit_code')!r}): "
                               f"{status.get('error')!r}")
            return status
        if time.monotonic() >= deadline:
            raise DllError(f"{label}: session did not finish before the deadline: {_brief(status)}")
        time.sleep(POLL_SECONDS)


def _stop_then_destroy(api, entry, label, timeout=10.0):
    handle = entry["handle"]
    deadline = time.monotonic() + timeout
    stop_codes = []
    while True:
        code = api.stop(handle, f"{label} stop")
        stop_codes.append(code)
        if code == 0:
            break
        if time.monotonic() >= deadline:
            raise PendingError(f"{label}: stop_session stayed pending for {timeout:.1f}s")
        time.sleep(0.01)
    destroy_codes = []
    while True:
        code = api.destroy(handle, f"{label} destroy")
        destroy_codes.append(code)
        if code == 0:
            break
        if time.monotonic() >= deadline:
            raise PendingError(f"{label}: destroy_session stayed pending for {timeout:.1f}s")
        time.sleep(0.01)
    entry["destroyed"] = True
    return {"stop_codes": stop_codes, "destroy_codes": destroy_codes}


def _module_loaded():
    if os.name != "nt":
        return None
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
    kernel32.GetModuleHandleW.restype = ctypes.c_void_p
    return bool(kernel32.GetModuleHandleW(DLL_NAME))


# ---------------------------------------------------------------------------
# Configuration and requests


def _codec_config(codec, jpeg_quality, max_au_bytes, max_bitrate_bps, decode_in_flight):
    if codec not in ("raw", "mjpeg", "pyrowave"):
        raise smoke.SmokeError(f"unsupported codec {codec!r}; only raw/mjpeg/pyrowave")
    jpeg = 80 if jpeg_quality is None else int(jpeg_quality)
    max_au = 8_388_608 if max_au_bytes is None else int(max_au_bytes)
    bitrate = DEFAULT_BITRATE_BPS[codec] if max_bitrate_bps is None else int(max_bitrate_bps)
    in_flight = 2 if decode_in_flight is None else int(decode_in_flight)
    if codec == "raw":
        experimental = False
        encoder = ""
        decoder = ""
    elif codec == "mjpeg":
        experimental = True
        encoder = decoder = "turbojpeg:cpu"
    else:
        experimental = True
        encoder = decoder = "pyrowave:vulkan"
    return {
        "codec": codec,
        "experimental": experimental,
        "encoder_backend": encoder,
        "decoder_backend": decoder,
        "jpeg_quality": jpeg,
        "max_au_bytes": max_au,
        "max_bitrate_bps": bitrate,
        "decode_in_flight": in_flight,
    }


def _validate_codec_config(config, width, height, fps):
    """Local mirror of vc_media::validate; the DLL remains authoritative."""
    codec = config["codec"]
    _require(codec in ("raw", "mjpeg", "pyrowave"), f"unsupported codec {codec!r}")
    _require(1 <= fps <= 1000, f"fps must be 1..1000, got {fps}")
    _require(1 <= config["jpeg_quality"] <= 100,
             f"jpeg_quality must be 1..100, got {config['jpeg_quality']}")
    _require(1024 <= config["max_au_bytes"] <= 16_777_216,
             f"max_au_bytes must be 1024..16777216, got {config['max_au_bytes']}")
    _require(1 <= config["decode_in_flight"] <= 8,
             f"decode_in_flight must be 1..8, got {config['decode_in_flight']}")
    if codec == "raw":
        _require(config["experimental"] is False and config["encoder_backend"] == ""
                 and config["decoder_backend"] == "", "raw must not carry a codec backend")
        return
    identity = {"mjpeg": "turbojpeg:cpu", "pyrowave": "pyrowave:vulkan"}[codec]
    _require(config["experimental"] is True,
             f"{codec} is an explicit development codec and requires experimental=true")
    _require(config["encoder_backend"] == identity and config["decoder_backend"] == identity,
             f"{codec} requires backend identity {identity!r}")
    _require(100_000 <= config["max_bitrate_bps"] <= 2_000_000_000,
             f"max_bitrate_bps must be 0.1..2000 Mbps, got {config['max_bitrate_bps']}")
    if codec == "pyrowave":
        frame_bytes = width * height * 3 // 2
        budget = min(config["max_bitrate_bps"] // 8 // fps,
                     config["max_au_bytes"] - 48, frame_bytes)
        _require(budget >= 1024, f"PyroWave per-frame budget {budget} is below 1024 bytes")


def _resolve_codec_options(args):
    if args.jpeg_quality is not None and args.codec != "mjpeg":
        raise smoke.SmokeError("--jpeg-quality is only valid with --codec mjpeg")
    if args.codec == "raw":
        for flag, value in (("--max-au-bytes", args.max_au_bytes),
                            ("--max-bitrate-bps", args.max_bitrate_bps),
                            ("--decode-in-flight", args.decode_in_flight)):
            if value is not None:
                raise smoke.SmokeError(f"{flag} requires --codec mjpeg|pyrowave; Raw has no encoding options")
    config = _codec_config(args.codec, args.jpeg_quality, args.max_au_bytes,
                           args.max_bitrate_bps, args.decode_in_flight)
    _validate_codec_config(config, args.width, args.height, args.fps)
    return config


def _source_config():
    return {
        "mode": "pattern",
        "pattern": "motion",
        "output": 0,
        "window": "",
        "transform": "crop",
        "gpu_convert": False,
    }


def _random_source_mac():
    raw = bytearray(os.urandom(6))
    raw[0] = (raw[0] & 0xFC) | 0x02  # locally administered unicast
    return list(raw)


def _mac_text(values):
    return ":".join(f"{value:02x}" for value in values)


def _sender_request(cfg, session, codec, source, source_mac, stats, hashlog):
    stats = Path(stats)
    hashlog = Path(hashlog)
    _require(stats.is_absolute() and hashlog.is_absolute(), "sender evidence paths must be absolute")
    return {
        "adapter": smoke.LOOPBACK,
        "width": cfg["width"],
        "height": cfg["height"],
        "fps": cfg["fps"],
        "mtu": cfg["mtu"],
        "session": session,
        "frames": cfg["frames"],
        "pacing_us": 0,
        "source": dict(source),
        "codec": dict(codec),
        "source_mac": list(source_mac),
        "dst_mac": list(DEST_MAC),
        "stats": str(stats),
        "hashlog": str(hashlog),
    }


def _receiver_request(cfg, session, codec, source_mac, shm_name, stats, hashlog):
    stats = Path(stats)
    hashlog = Path(hashlog)
    _require(stats.is_absolute() and hashlog.is_absolute(), "receiver evidence paths must be absolute")
    return {
        "adapter": smoke.LOOPBACK,
        "width": cfg["width"],
        "height": cfg["height"],
        "fps": cfg["fps"],
        "mtu": cfg["mtu"],
        "session": session,
        "codec": dict(codec),
        "source_mac": list(source_mac),
        "expect_frames": cfg["frames"],
        "timeout_s": cfg["receiver_timeout_s"],
        "shm": shm_name,
        "stats": str(stats),
        "hashlog": str(hashlog),
    }


# ---------------------------------------------------------------------------
# Ready markers


def _check_echo(record, key, expected, label):
    """Require each negotiated field and compare its exact value."""
    _require(key in record, f"{label}: missing negotiated field {key!r}")
    _require(record[key] == expected, f"{label}: {key} {record[key]!r} != {expected!r}")


def _check_geometry_echo(ready, cfg, label):
    geometry = ready.get("geometry")
    if geometry is not None:
        _require(isinstance(geometry, dict), f"{label}: geometry is not an object")
        _check_echo(geometry, "width", cfg["width"], f"{label} geometry")
        _check_echo(geometry, "height", cfg["height"], f"{label} geometry")
        _check_echo(geometry, "fps", cfg["fps"], f"{label} geometry")
    for key in ("width", "height", "fps", "mtu"):
        _check_echo(ready, key, cfg[key], label)


def _validate_receiver_ready(ready, cfg, session, codec):
    label = "RX ready"
    _require(isinstance(ready, dict), f"{label}: not a JSON object")
    _require(ready.get("ready") is True, f"{label}: ready is not true")
    _check_echo(ready, "role", "receiver", label)
    smoke.require_session(ready.get("session_id"), session, label)
    smoke.require_adapter(ready.get("adapter"), label)
    _check_echo(ready, "running", True, label)
    _check_geometry_echo(ready, cfg, label)
    _check_echo(ready, "codec", codec["codec"], label)
    _check_echo(ready, "ethertype", "0x88b5", label)
    if "datalink" in ready:
        _require(smoke.is_int(ready["datalink"]) and ready["datalink"] >= 0,
                 f"{label}: datalink {ready['datalink']!r}")
    if "decode_in_flight" in ready:
        _require(smoke.is_int(ready["decode_in_flight"])
                 and 1 <= ready["decode_in_flight"] <= 8,
                 f"{label}: decode_in_flight {ready['decode_in_flight']!r}")
    # SHM was requested: a silent SHM-off receiver must fail closed.
    _require(ready.get("shm_enabled") is True, f"{label}: SHM is not enabled")
    _require(ready.get("shm_version") == 3, f"{label}: SHM version {ready.get('shm_version')!r}")


def _validate_sender_ready(ready, cfg, session, codec, raw, total_packets):
    label = "TX ready"
    _require(isinstance(ready, dict), f"{label}: not a JSON object")
    _require(ready.get("ready") is True, f"{label}: ready is not true")
    _check_echo(ready, "role", "sender", label)
    smoke.require_session(ready.get("session_id"), session, label)
    smoke.require_adapter(ready.get("adapter"), label)
    _require((ready.get("width"), ready.get("height"), ready.get("fps"), ready.get("mtu"))
             == (cfg["width"], cfg["height"], cfg["fps"], cfg["mtu"]),
             f"{label}: geometry/mtu do not match the request")
    _check_echo(ready, "codec", codec["codec"], label)
    _require(ready.get("source") == "pattern", f"{label}: source {ready.get('source')!r}")
    _require(ready.get("source_info") is None, f"{label}: pattern source must not report desktop info")
    if raw:
        _require(ready.get("packets_per_frame") == total_packets,
                 f"{label}: packets_per_frame {ready.get('packets_per_frame')!r} != {total_packets}")
        _require(ready.get("batch") == 1,
                 f"{label}: Loopback diagnostics must use batch 1, got {ready.get('batch')!r}")
    else:
        _require(ready.get("packets_per_frame") is None,
                 f"{label}: encoded packets_per_frame must be null")
        _require(ready.get("batch") is None, f"{label}: encoded batch must be null")


# ---------------------------------------------------------------------------
# Evidence parsers


HEX_DIGITS = set("0123456789abcdefABCDEF")


def _normalise_hash(value, label):
    _require(isinstance(value, str) and len(value) == 16 and set(value) <= HEX_DIGITS,
             f"{label}: invalid FNV-1a64 hash {value!r}")
    return value.lower()


def _parse_rows(path, frames, label, fields):
    path = Path(path)
    _require(path.is_file(), f"{label}: missing {path}")
    _require(path.stat().st_size > 0, f"{label}: evidence file is empty: {path}")
    rows = {}
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        raise smoke.SmokeError(f"{label}: cannot read {path}: {exc}") from exc
    for line_number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as exc:
            raise smoke.SmokeError(f"{label}: line {line_number} is not JSON: {exc}") from exc
        _require(isinstance(row, dict), f"{label}: line {line_number} is not a JSON object")
        frame_id = row.get("frame_id")
        _require(smoke.is_int(frame_id) and 0 <= frame_id < frames,
                 f"{label}: line {line_number} has invalid frame_id {frame_id!r}")
        _require(frame_id not in rows, f"{label}: duplicate row for frame_id {frame_id}")
        row = dict(row)
        for field in fields:
            row[field] = _normalise_hash(row.get(field), f"{label} frame {frame_id} {field}")
        rows[frame_id] = row
    missing = sorted(set(range(frames)) - set(rows))
    extra = sorted(set(rows) - set(range(frames)))
    _require(len(rows) == frames and not missing and not extra,
             f"{label}: expected exactly frame ids 0..{frames - 1}, got {len(rows)} rows; "
             f"missing={missing[:8]} extra={extra[:8]}")
    return rows


def _validate_raw_rows(tx_rows, rx_rows, frames):
    for fid in range(frames):
        for role, row, field in (("TX", tx_rows[fid], "send_qpc_ns"),
                                 ("RX", rx_rows[fid], "first_packet_qpc_ns")):
            _require(smoke.is_int(row.get("qpc_ns")) and row["qpc_ns"] > 0,
                     f"{role} frame {fid}: qpc_ns {row.get('qpc_ns')!r}")
            _require(smoke.is_int(row.get(field)) and row[field] > 0,
                     f"{role} frame {fid}: {field} {row.get(field)!r}")


def _validate_encoded_tx_rows(rows, frames, max_au_bytes):
    for fid in range(frames):
        row = rows[fid]
        _require(smoke.is_int(row.get("qpc_ns")) and row["qpc_ns"] > 0,
                 f"TX frame {fid}: qpc_ns {row.get('qpc_ns')!r}")
        _require(smoke.is_int(row.get("send_qpc_ns")) and row["send_qpc_ns"] > 0,
                 f"TX frame {fid}: send_qpc_ns {row.get('send_qpc_ns')!r}")
        _require(smoke.is_int(row.get("encoded_bytes"))
                 and 0 < row["encoded_bytes"] <= max_au_bytes,
                 f"TX frame {fid}: encoded_bytes {row.get('encoded_bytes')!r}")
        _require(isinstance(row.get("keyframe"), bool), f"TX frame {fid}: keyframe is not a bool")


def _validate_encoded_rx_rows(rows, frames):
    for fid in range(frames):
        row = rows[fid]
        _require(smoke.is_int(row.get("qpc_ns")) and row["qpc_ns"] > 0,
                 f"RX frame {fid}: qpc_ns {row.get('qpc_ns')!r}")
        if "decode_pipeline_ms" in row:
            pipeline = row["decode_pipeline_ms"]
            _require(_is_number(pipeline) and pipeline >= 0,
                     f"RX frame {fid}: decode_pipeline_ms {pipeline!r}")


def _validate_transport_rows(rows, tx_rows, frames):
    for fid in range(frames):
        row = rows[fid]
        if "encoded_bytes" in row:
            _require(smoke.is_int(row["encoded_bytes"]) and row["encoded_bytes"] > 0,
                     f"transport frame {fid}: encoded_bytes {row['encoded_bytes']!r}")
            _require(row["encoded_bytes"] == tx_rows[fid]["encoded_bytes"],
                     f"transport frame {fid}: encoded_bytes {row['encoded_bytes']} != "
                     f"TX {tx_rows[fid]['encoded_bytes']}")
        if "keyframe" in row:
            _require(isinstance(row["keyframe"], bool),
                     f"transport frame {fid}: keyframe is not a bool")
            _require(row["keyframe"] == tx_rows[fid]["keyframe"],
                     f"transport frame {fid}: keyframe {row['keyframe']} != TX {tx_rows[fid]['keyframe']}")


# ---------------------------------------------------------------------------
# SHM v3 reader


def _open_shm_view(name, label, timeout):
    if smoke.K32 is None:
        raise DllError(f"{label}: SHM probing requires Windows")
    deadline = time.monotonic() + timeout
    last_error = None
    while True:
        handle = smoke.K32.OpenFileMappingW(smoke.FILE_MAP_READ, False, name)
        if handle:
            view = smoke.K32.MapViewOfFile(handle, smoke.FILE_MAP_READ, 0, 0, 0)
            if not view:
                error = ctypes.get_last_error()
                smoke.K32.CloseHandle(handle)
                raise DllError(f"{label}: MapViewOfFile({name!r}) failed, win32 error {error}")
            return smoke.ShmView(handle, view)
        last_error = ctypes.get_last_error()
        if time.monotonic() >= deadline:
            raise DllError(f"{label}: SHM mapping {name!r} did not appear within {timeout:.1f}s "
                           f"(win32 error {last_error})")
        time.sleep(POLL_SECONDS)


def _validate_shm(view, name, cfg, frames, rx_rows, hash_field, label):
    """Validate the v3 static layout and the latest published NV12 frame."""
    header = struct.unpack(smoke.SHM_HDR_FMT, view.read(0, smoke.SHM_HDR_SIZE))
    (magic, version, slots, frame_bytes, data_off, latest_slot, latest_seq,
     writer_pid, width, height, fps) = header
    _require(magic == smoke.SHM_MAGIC, f"{label}: SHM magic 0x{magic:08x} != 0x{smoke.SHM_MAGIC:08x}")
    _require(version == smoke.SHM_VERSION, f"{label}: SHM version {version} != {smoke.SHM_VERSION}")
    _require(slots == smoke.SHM_SLOTS, f"{label}: SHM slots {slots} != {smoke.SHM_SLOTS}")
    _require(data_off == smoke.SHM_DATA_OFF, f"{label}: SHM data_off {data_off} != {smoke.SHM_DATA_OFF}")
    _require(frame_bytes == cfg["frame_bytes"],
             f"{label}: SHM frame_bytes {frame_bytes} != {cfg['frame_bytes']}")
    _require((width, height, fps) == (cfg["width"], cfg["height"], cfg["fps"]),
             f"{label}: SHM geometry {width}x{height}@{fps} != "
             f"{cfg['width']}x{cfg['height']}@{cfg['fps']}")
    _require(writer_pid == os.getpid(),
             f"{label}: SHM writer_pid {writer_pid} != live host pid {os.getpid()}")
    _require(0 <= latest_slot < smoke.SHM_SLOTS,
             f"{label}: SHM latest_slot {latest_slot} is outside 0..{smoke.SHM_SLOTS - 1}")
    _require(latest_seq == frames,
             f"{label}: SHM latest_seq {latest_seq} != {frames} published frames")

    slot_offset = SHM_HEADER_BYTES + SHM_SLOT_BYTES * latest_slot
    seq1, frame_id, qpc_first, qpc_pub, slot_hash, hash_fid = struct.unpack(
        smoke.SHM_SLOT_FMT, view.read(slot_offset, smoke.SHM_SLOT_SIZE))
    _require(seq1 % 2 == 0, f"{label}: latest SHM slot seq {seq1} is odd (still filling)")
    _require(0 <= frame_id < frames,
             f"{label}: latest SHM frame_id {frame_id} is outside 0..{frames - 1}")
    _require(qpc_first > 0 and qpc_pub >= qpc_first,
             f"{label}: invalid SHM QPC fields first={qpc_first} pub={qpc_pub}")
    payload = view.read(data_off + latest_slot * frame_bytes, frame_bytes)
    seq2 = struct.unpack("<i", view.read(slot_offset, 4))[0]
    _require(seq1 == seq2, f"{label}: SHM slot seq changed during read ({seq1} != {seq2})")

    actual = smoke.pattern_ref.fnv1a64_hex(payload)
    expected = rx_rows[frame_id][hash_field]
    _require(actual == expected,
             f"{label}: SHM frame {frame_id} FNV {actual} != RX {hash_field} {expected}")
    _require(hash_fid == frame_id,
             f"{label}: SHM hash_fid {hash_fid} != frame_id {frame_id} (frame was not hash-marked)")
    _require(slot_hash == int(actual, 16),
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
        "hash_field": hash_field,
        "hash_marked": True,
        "qpc_pub_ns": qpc_pub,
    }


# ---------------------------------------------------------------------------
# Metrics


def _summarize_ns(samples):
    values = sorted(int(value) for value in samples)
    count = len(values)
    _require(count > 0, "cannot summarize an empty latency sample")
    return {
        "samples": count,
        "mean_ms": sum(values) / count / 1e6,
        "p50_ms": values[(50 * count + 99) // 100 - 1] / 1e6,
        "p99_ms": values[(99 * count + 99) // 100 - 1] / 1e6,
        "max_ms": values[-1] / 1e6,
        "gte_10ms": sum(value >= LIMIT_NS for value in values),
    }


def _interval_fps(times_ns, role):
    _require(len(times_ns) >= 2, f"{role}: at least two frames are needed for cadence")
    intervals = [right - left for left, right in zip(times_ns, times_ns[1:])]
    _require(min(intervals) > 0, f"{role}: QPC frame times are not strictly increasing")
    summary = _summarize_ns(intervals)
    fps = len(intervals) * 1e9 / sum(intervals)
    return intervals, summary, fps


def _compute_metrics(tx_rows, rx_rows, frames, codec_name, target_fps, performance):
    latencies = []
    start_to_done = []
    generation = []
    send_to_first = []
    reassembly = []
    decode_pipeline = []
    have_decode_pipeline = True
    for fid in range(frames):
        tx = tx_rows[fid]
        rx = rx_rows[fid]
        start = tx.get("qpc_ns")
        sent = tx.get("send_qpc_ns")
        done = rx.get("qpc_ns")
        _require(smoke.is_int(start) and smoke.is_int(sent) and smoke.is_int(done)
                 and start > 0 and sent > 0 and done > 0,
                 f"frame {fid}: missing QPC timestamps")
        _require(start <= sent <= done,
                 f"frame {fid}: invalid same-host QPC order: {start}, {sent}, {done}")
        if codec_name == "raw":
            first = rx.get("first_packet_qpc_ns")
            _require(smoke.is_int(first) and sent <= first <= done,
                     f"frame {fid}: invalid receive QPC order: {sent}, {first!r}, {done}")
            send_to_first.append(first - sent)
            reassembly.append(done - first)
        elif "decode_pipeline_ms" in rx:
            pipeline = rx["decode_pipeline_ms"]
            _require(_is_number(pipeline) and pipeline >= 0,
                     f"frame {fid}: decode_pipeline_ms {pipeline!r}")
            decode_pipeline.append(int(round(float(pipeline) * 1e6)))
        else:
            have_decode_pipeline = False
        latencies.append(done - sent)
        start_to_done.append(done - start)
        generation.append(sent - start)

    latency = _summarize_ns(latencies)
    over_10ms = [{"frame_id": fid, "ms": round(latencies[fid] / 1e6, 6)}
                 for fid in range(frames) if latencies[fid] >= LIMIT_NS]
    tx_times = [tx_rows[fid]["qpc_ns"] for fid in range(frames)]
    send_times = [tx_rows[fid]["send_qpc_ns"] for fid in range(frames)]
    rx_times = [rx_rows[fid]["qpc_ns"] for fid in range(frames)]
    tx_intervals = tx_interval_summary = tx_fps = None
    send_interval_summary = send_fps = None
    rx_intervals = rx_interval_summary = rx_fps = None
    if frames >= 2:
        tx_intervals, tx_interval_summary, tx_fps = _interval_fps(tx_times, "TX frame start")
        _, send_interval_summary, send_fps = _interval_fps(send_times, "TX send start")
        rx_intervals, rx_interval_summary, rx_fps = _interval_fps(rx_times, "RX completion")

    metrics = {
        "scope": "same-process vc_stream.dll on Npcap Loopback; same-host QPC only, "
                 "not two-machine, physical-link or capacity acceptance",
        "latency_basis": "TX send_qpc_ns -> RX completion qpc_ns from the real hash rows",
        "codec": codec_name,
        "target_fps": target_fps,
        "period_ms": 1000.0 / target_fps,
        "send_to_rx_ms": latency,
        "start_to_rx_ms": _summarize_ns(start_to_done),
        "generation_ms": _summarize_ns(generation),
        "frames_ge_10ms": over_10ms,
        "frames_ge_10ms_count": len(over_10ms),
        "actual_fps": {"tx_steady": tx_fps, "tx_send_steady": send_fps, "rx_steady": rx_fps},
        "tx_interval_ms": tx_interval_summary,
        "tx_send_interval_ms": send_interval_summary,
        "rx_interval_ms": rx_interval_summary,
        "worst_frame_id": max(range(frames), key=latencies.__getitem__),
        "all_frames_lt_10ms": latency["gte_10ms"] == 0,
        "latency_p99_lt_10ms": latency["p99_ms"] < 10.0,
        "performance_requested": bool(performance),
    }
    if codec_name == "raw":
        metrics["send_to_first_packet_ms"] = _summarize_ns(send_to_first)
        metrics["reassembly_ms"] = _summarize_ns(reassembly)
    else:
        metrics["decode_pipeline_ms"] = (
            _summarize_ns(decode_pipeline) if have_decode_pipeline else None)

    metrics["performance_pass"] = False if performance else None
    return metrics


def _validate_metrics(metrics, frames):
    """Apply gates after the caller saves the measurements, including failures."""
    target_fps = metrics["target_fps"]
    if frames >= 2:
        low, high = 0.75 * target_fps, 1.25 * target_fps
        for role, fps in metrics["actual_fps"].items():
            if role not in ("tx_steady", "tx_send_steady", "rx_steady"):
                continue
            _require(low <= fps <= high,
                     f"{role} FPS {fps:.3f} is outside {low:.3f}..{high:.3f} "
                     f"(configured {target_fps} fps, finite-run gate)")

    if metrics["performance_requested"]:
        cadence_tolerance_ms = metrics["period_ms"] * 0.01
        for key in ("tx_interval_ms", "tx_send_interval_ms"):
            intervals = metrics[key]
            _require(intervals is not None, f"--performance needs {key} cadence")
            _require(abs(intervals["mean_ms"] - metrics["period_ms"]) <= cadence_tolerance_ms,
                     f"--performance: {key} mean interval {intervals['mean_ms']:.3f} ms "
                     f"differs from {metrics['period_ms']:.3f} ms by more than "
                     f"1% ({cadence_tolerance_ms:.3f} ms)")
        for key in ("send_to_rx_ms", "start_to_rx_ms"):
            latency = metrics[key]
            _require(latency["p99_ms"] < 10.0,
                     f"--performance: {key} p99 {latency['p99_ms']:.3f} ms is not below 10 ms")
        metrics["performance_pass"] = True


# ---------------------------------------------------------------------------
# ABI negative probes


def _expect_start_failure(api, role, request, label, error_capacity=ERROR_CAPACITY):
    result = _invoke_start(api.start_fn, role, request, error_capacity)
    if result["code"] == 0:
        # A rejected probe was accepted. Free the unexpected session before the
        # failure propagates so interpreter teardown never unloads a live DLL.
        if result["handle"]:
            entry = {"handle": int(result["handle"]), "destroyed": False}
            try:
                _stop_then_destroy(api, entry, f"{label} unexpected session")
            except PendingError as exc:
                raise UntrackedSession(f"{label}: probe unexpectedly started a session that "
                                       f"cannot be stopped: {exc}") from exc
        raise smoke.SmokeError(f"{label}: expected a negative start code, got success")
    _require(result["code"] < 0,
             f"{label}: expected a negative start code, got {result['code']}")
    _require(result["handle"] is None,
             f"{label}: failed start_session did not clear the output handle "
             f"({result['handle']!r})")
    _require(result["error_written"] > 0, f"{label}: failure did not report an error length")
    if error_capacity > 1:
        _require(result["error"] != "", f"{label}: failure left the error buffer empty")
        _require(len(result["error"].encode("utf-8")) <= error_capacity,
                 f"{label}: error text does not fit the caller's buffer")
    return {
        "label": label,
        "role": role,
        "code": result["code"],
        "handle_cleared": True,
        "error_written": result["error_written"],
        "error_capacity": error_capacity,
        "error": result["error"],
    }


def _expect_negative(call, label):
    try:
        call()
    except DllError as exc:
        return {"label": label, "error": str(exc)}
    raise smoke.SmokeError(f"{label}: expected a negative result, but the call succeeded")


def _abi_probes(api, receiver_request, sender_request):
    probes = []
    probes.append(_expect_start_failure(api, 99, b"{}", "unsupported stream role"))
    probes.append(_expect_start_failure(api, ROLE_SENDER, b"{", "invalid JSON request"))
    probes.append(_expect_start_failure(api, ROLE_SENDER, None, "null request pointer"))

    tiny = _expect_start_failure(api, ROLE_SENDER, b"{", "tiny error buffer", error_capacity=4)
    _require(tiny["error_written"] > 4,
             f"tiny error buffer: full error length {tiny['error_written']} was not reported")
    _require(len(tiny["error"].encode("utf-8")) <= 3,
             f"tiny error buffer: truncated error text is {tiny['error']!r}")
    probes.append(tiny)

    unsupported = copy.deepcopy(sender_request)
    unsupported["codec"] = _codec_config("mjpeg", None, None, None, None)
    unsupported["codec"]["experimental"] = False
    unsupported["stats"] = str(Path(sender_request["stats"]).with_name("probe-unsupported-stats.json"))
    unsupported["hashlog"] = str(Path(sender_request["hashlog"]).with_name("probe-unsupported-hash.jsonl"))
    probes.append(_expect_start_failure(api, ROLE_SENDER, json.dumps(unsupported).encode("utf-8"),
                                        "encoded codec without experimental"))

    empty_shm = copy.deepcopy(receiver_request)
    empty_shm["shm"] = ""
    empty_shm["stats"] = str(Path(receiver_request["stats"]).with_name("probe-empty-shm-stats.json"))
    empty_shm["hashlog"] = str(Path(receiver_request["hashlog"]).with_name("probe-empty-shm-hash.jsonl"))
    probes.append(_expect_start_failure(api, ROLE_RECEIVER, json.dumps(empty_shm).encode("utf-8"),
                                        "empty SHM name"))

    error = ctypes.create_string_buffer(ERROR_CAPACITY)
    error_written = ctypes.c_uint32(0)
    code = api.start_fn(ROLE_SENDER, b"{}", None, error, ERROR_CAPACITY,
                        ctypes.pointer(error_written))
    _require(code < 0 and error_written.value > 0,
             f"null output handle: expected a negative code and an error, got {code}/{error_written.value}")
    probes.append({
        "label": "null output handle",
        "code": int(code),
        "error_written": int(error_written.value),
        "error": _error_text(error, error_written.value, ERROR_CAPACITY),
    })

    probes.append(_expect_negative(lambda: api.status(0, "null handle status"), "null status handle"))
    probes.append(_expect_negative(lambda: api.stop(0, "null handle stop"), "null stop handle"))
    probes.append(_expect_negative(lambda: api.destroy(0, "null handle destroy"), "null destroy handle"))
    return probes


# ---------------------------------------------------------------------------
# Evidence and evidence paths


def _make_run_dir():
    base = smoke.ROOT / "run"
    base.mkdir(parents=True, exist_ok=True)
    for _ in range(100):
        token = uuid.uuid4().hex[:12]
        path = base / f"rust-dll-stream-smoke-{token}"
        try:
            path.mkdir()
        except FileExistsError:
            continue
        return path, token
    raise smoke.SmokeError(f"cannot create a unique run directory under {base}")


def _write_json_exclusive(path, value):
    with open(path, "x", encoding="utf-8") as handle:
        json.dump(value, handle, ensure_ascii=False, indent=2, sort_keys=True)
        handle.write("\n")


def _print_record(value):
    print(json.dumps(value, ensure_ascii=False, sort_keys=True))
    sys.stdout.flush()


def _hard_exit_pending(run_dir, report, message):
    """Never let interpreter teardown unload a DLL whose worker is still pending."""
    report["status"] = "FAIL"
    report["failure"] = {"error": message, "reason": "pending session cleanup"}
    try:
        _write_json_exclusive(run_dir / "failure.json", report)
    except OSError:
        pass
    _print_record({"status": "FAIL", "run_dir": str(run_dir), "error": message,
                   "reason": "pending session; process exited without unloading the DLL"})
    sys.stderr.flush()
    os._exit(1)


# ---------------------------------------------------------------------------
# Live execution


def _preflight_dependencies(codec_name, dll_dir, report):
    candidates = {
        "raw": (),
        "mjpeg": ("turbojpeg.dll", "libturbojpeg.dll"),
        "pyrowave": ("libpyrowave-shared-1.dll",),
    }[codec_name]
    if not candidates:
        report["dependencies"] = {"codec": codec_name, "required": [], "found": []}
        return
    found = [name for name in candidates if (dll_dir / name).is_file()]
    _require(found,
             f"{codec_name} requires one of {list(candidates)} beside {dll_dir}; "
             f"missing dependencies must fail closed")
    report["dependencies"] = {
        "codec": codec_name,
        "required": list(candidates),
        "found": found,
        "dir": str(dll_dir),
    }


def _validate_reference(ref_ids, ref_hashes, streams, frames, label):
    checked = []
    for fid in ref_ids:
        for name, rows, field in streams:
            _require(rows[fid][field] == ref_hashes[fid],
                     f"{label}: {name} frame {fid} FNV {rows[fid][field]} != motion reference "
                     f"{ref_hashes[fid]}")
        checked.append(fid)
    return {
        "frames": checked,
        "all_frames": len(checked) == frames,
        "requested_samples": len(ref_ids),
    }


def _run_pending_probe(api, cfg, run_dir, token, report):
    """Prove stop/destroy semantics on a live session and never unload while pending."""
    session = "dll-smoke-probe-" + token
    shm_name = SHM_PREFIX + "Probe." + token
    stats_path = run_dir / "probe-rx-stats.json"
    hashlog_path = run_dir / "probe-rx-hash.jsonl"
    request = _receiver_request(
        {**cfg, "frames": 0, "receiver_timeout_s": 30},
        session,
        _codec_config("raw", None, None, None, None),
        _random_source_mac(),
        shm_name,
        stats_path,
        hashlog_path,
    )
    # expect_frames=0 and a positive timeout keep the receiver waiting until destroyed.
    _require(request["expect_frames"] == 0 and request["timeout_s"] > 0,
             "probe RX request does not describe a waiting receiver")
    handle = api.start(ROLE_RECEIVER, request, "probe RX start")
    entry = {"handle": handle, "destroyed": False}
    record = {"session_id": session, "shm": shm_name}
    try:
        ready = _wait_ready(api, handle, session, "probe RX", cfg["ready_timeout_s"])
        record["ready"] = _brief(ready)
        _require(ready.get("running") is True, "probe RX: ready session is not running")
        first = api.destroy(handle, "probe RX destroy")
        record["first_destroy_code"] = first
        record["observed_pending"] = first == 1
        deadline = time.monotonic() + 10.0
        codes = [first]
        while codes[-1] != 0:
            if time.monotonic() >= deadline:
                raise PendingError("probe RX: destroy_session stayed pending for 10s")
            codes.append(api.destroy(handle, "probe RX destroy retry"))
            if codes[-1] == 1:
                time.sleep(0.01)
        entry["destroyed"] = True
        record["destroy_codes"] = codes
        stats = smoke.read_json(stats_path, "probe RX stats")
        smoke.require_session(stats.get("session_id"), session, "probe RX stats")
        smoke.require_adapter(stats.get("adapter"), "probe RX stats")
        _require(stats.get("complete") == 0, f"probe RX stats: complete {stats.get('complete')!r}")
        _require(stats.get("hash_rows") == 0, f"probe RX stats: hash_rows {stats.get('hash_rows')!r}")
        record["stats"] = {"running": stats.get("running"), "complete": stats.get("complete"),
                           "hash_rows": stats.get("hash_rows")}
    finally:
        if not entry["destroyed"]:
            try:
                _stop_then_destroy(api, entry, "probe RX cleanup")
            except PendingError as exc:
                _hard_exit_pending(run_dir, report, str(exc))
    return record


def execute(args, dll_path, package, run_dir, token, report):
    cfg = smoke.build_config(args.frames, args.fps, args.width, args.height, args.mtu, 1)
    codec = _resolve_codec_options(args)
    source = _source_config()
    raw = codec["codec"] == "raw"
    ref_ids = smoke.sample_frames(cfg["frames"], args.ref_samples)

    tx_session = "dll-smoke-tx-" + token
    rx_session = "dll-smoke-rx-" + token
    shm_name = SHM_PREFIX + token
    source_mac = _random_source_mac()

    tx_stats_path = run_dir / "tx-stats.json"
    tx_hashlog_path = run_dir / "tx-hash.jsonl"
    rx_stats_path = run_dir / "rx-stats.json"
    rx_hashlog_path = run_dir / "rx-hash.jsonl"
    transport_path = rx_hashlog_path.with_name(rx_hashlog_path.name + ".transport.jsonl")

    sender_request = _sender_request(cfg, tx_session, codec, source, source_mac,
                                     tx_stats_path, tx_hashlog_path)
    receiver_request = _receiver_request(cfg, rx_session, codec, source_mac, shm_name,
                                         rx_stats_path, rx_hashlog_path)

    report.update({
        "scope": "same-process vc_stream.dll ABI v1; same-host Npcap Loopback only; "
                 "not two-machine, physical-link or capacity acceptance",
        "run_dir": str(run_dir),
        "token": token,
        "config": {
            **cfg,
            "codec": codec,
            "source_mac": _mac_text(source_mac),
            "dst_mac": _mac_text(DEST_MAC),
            "ref_samples_requested": args.ref_samples,
            "reference_frame_ids": ref_ids,
            "performance": bool(args.performance),
            "raw": raw,
        },
        "requests": {"sender": sender_request, "receiver": receiver_request},
        "sessions": {
            "sender": {"session_id": tx_session},
            "receiver": {"session_id": rx_session},
        },
        "subprocesses": [],
        "dshow_registration": False,
    })

    dll_path = Path(dll_path).resolve()
    report["dll"] = {"path": str(dll_path), "size": dll_path.stat().st_size}
    with dll_path.open("rb") as binary:
        report["dll"]["sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
    report["package"] = str(package) if package else None
    report["environment"] = {
        "platform": platform.platform(),
        "python": sys.version,
        "executable": sys.executable,
        "isolated": bool(sys.flags.isolated),
        "pid": os.getpid(),
        "cwd": str(Path.cwd()),
    }
    _preflight_dependencies(codec["codec"], dll_path.parent, report)

    api = _load_api(dll_path, report)
    capabilities, capabilities_text = api.capabilities()
    _require(isinstance(capabilities, dict), "capabilities: not a JSON object")
    _require(codec["codec"] in capabilities.get("codecs", []),
             f"capabilities do not list requested codec {codec['codec']!r}: {capabilities.get('codecs')!r}")
    _require(capabilities.get("shm_version") == 3,
             f"capabilities report SHM version {capabilities.get('shm_version')!r}, need 3")
    _require(capabilities.get("pixel_format") == "nv12",
             f"capabilities pixel format {capabilities.get('pixel_format')!r}, need nv12")
    report["capabilities"] = capabilities
    report["capabilities_text"] = capabilities_text

    adapters, adapters_text = api.list_adapters()
    _require(isinstance(adapters, list) and adapters, "adapter list is empty or not an array")
    adapter_names = []
    for entry in adapters:
        if isinstance(entry, (list, tuple)) and entry:
            adapter_names.append(entry[0])
    _require(smoke.LOOPBACK in adapter_names,
             f"Npcap adapter list lacks {smoke.LOOPBACK}; install Npcap with Loopback support")
    report["adapters"] = adapter_names
    report["adapters_text"] = adapters_text

    report["abi"]["probes"] = _abi_probes(api, receiver_request, sender_request)

    sessions = {}
    view = None
    try:
        rx_handle = api.start(ROLE_RECEIVER, receiver_request, "RX start")
        sessions["rx"] = {"handle": rx_handle, "destroyed": False}
        rx_ready = _wait_ready(api, rx_handle, rx_session, "RX", cfg["ready_timeout_s"])
        _validate_receiver_ready(rx_ready, cfg, rx_session, codec)
        report["sessions"]["receiver"].update({
            "handle": f"0x{rx_handle:x}",
            "ready": rx_ready,
            "module_loaded_after_start": _module_loaded(),
        })

        view = _open_shm_view(shm_name, "RX", cfg["ready_timeout_s"])
        report["sessions"]["receiver"]["shm_opened_while_running"] = True

        tx_handle = api.start(ROLE_SENDER, sender_request, "TX start")
        sessions["tx"] = {"handle": tx_handle, "destroyed": False}
        tx_ready = _wait_ready(api, tx_handle, tx_session, "TX", cfg["ready_timeout_s"])
        _validate_sender_ready(tx_ready, cfg, tx_session, codec, raw, cfg["total_packets"])
        report["sessions"]["sender"].update({
            "handle": f"0x{tx_handle:x}",
            "ready": tx_ready,
        })

        deadline = time.monotonic() + cfg["phase_timeout_s"]
        tx_final = _wait_done(api, tx_handle, tx_session, "TX", deadline)
        rx_final = _wait_done(api, rx_handle, rx_session, "RX", deadline)
        report["sessions"]["sender"]["final"] = tx_final
        report["sessions"]["receiver"]["final"] = rx_final

        tx_stats = smoke.read_json(tx_stats_path, "TX stats")
        rx_stats = smoke.read_json(rx_stats_path, "RX stats")
        report["stats"] = {"tx": tx_stats, "rx": rx_stats}

        if raw:
            smoke.validate_tx_stats(tx_stats, True, cfg["frames"], cfg["total_packets"],
                                    tx_session, "TX stats")
        else:
            _require(tx_stats.get("sent_frames") == cfg["frames"] and tx_stats.get("frames") == cfg["frames"],
                     f"TX stats: sent_frames/frames {tx_stats.get('sent_frames')!r}/"
                     f"{tx_stats.get('frames')!r} != {cfg['frames']}")
            _require(tx_stats.get("send_errors") == 0, f"TX stats: send_errors {tx_stats.get('send_errors')!r}")
            _require(smoke.is_int(tx_stats.get("sent_packets")) and tx_stats["sent_packets"] > 0,
                     f"TX stats: sent_packets {tx_stats.get('sent_packets')!r}")
            _require(smoke.is_int(tx_stats.get("encoded_bytes")) and tx_stats["encoded_bytes"] > 0,
                     f"TX stats: encoded_bytes {tx_stats.get('encoded_bytes')!r}")
            _require(tx_stats.get("batch") is None, f"TX stats: encoded batch {tx_stats.get('batch')!r}")
        smoke.validate_rx_stats(rx_stats, True, cfg["frames"], rx_session, True, "RX stats")

        for role, stats in (("TX", tx_stats), ("RX", rx_stats)):
            smoke.require_adapter(stats.get("adapter"), f"{role} stats")
            _check_geometry_echo(stats, cfg, f"{role} stats")
            _check_echo(stats, "codec", codec["codec"], f"{role} stats")
            _check_echo(stats, "ethertype", "0x88b5", f"{role} stats")
        _require(tx_stats.get("pattern") == "motion", f"TX stats: pattern {tx_stats.get('pattern')!r}")
        _require(tx_stats.get("transform") == "crop", f"TX stats: transform {tx_stats.get('transform')!r}")
        _require(tx_stats.get("source") == "pattern", f"TX stats: source {tx_stats.get('source')!r}")
        _require(tx_stats.get("source_info") is None, "TX stats: pattern source_info is not null")
        _require(tx_stats.get("fps_target") == cfg["fps"],
                 f"TX stats: fps_target {tx_stats.get('fps_target')!r}")
        _require(tx_stats.get("pacing_us") == 0, f"TX stats: pacing_us {tx_stats.get('pacing_us')!r}")
        if raw:
            _require(tx_stats.get("batch") == 1, f"TX stats: Loopback batch {tx_stats.get('batch')!r}")
        _require(_is_number(tx_stats.get("fps_actual")) and tx_stats["fps_actual"] > 0,
                 f"TX stats: fps_actual {tx_stats.get('fps_actual')!r}")
        _require(_is_number(rx_stats.get("fps_actual")) and rx_stats["fps_actual"] > 0,
                 f"RX stats: fps_actual {rx_stats.get('fps_actual')!r}")
        _require(rx_stats.get("last_frame_id") == cfg["frames"] - 1,
                 f"RX stats: last_frame_id {rx_stats.get('last_frame_id')!r}")
        _require(smoke.is_int(rx_stats.get("first_completed_qpc_ns"))
                 and rx_stats["first_completed_qpc_ns"] > 0,
                 "RX stats: first_completed_qpc_ns")
        _require(smoke.is_int(rx_stats.get("last_completed_qpc_ns"))
                 and rx_stats["last_completed_qpc_ns"] >= rx_stats["first_completed_qpc_ns"],
                 "RX stats: last_completed_qpc_ns")

        hash_summary = {}
        if raw:
            tx_rows = smoke.parse_hash_rows(tx_hashlog_path, cfg["frames"], "TX hashlog")
            rx_rows = smoke.parse_hash_rows(rx_hashlog_path, cfg["frames"], "RX hashlog")
            _validate_raw_rows(tx_rows, rx_rows, cfg["frames"])
            tx_hashes = {fid: row["hash"] for fid, row in tx_rows.items()}
            rx_hashes = {fid: row["hash"] for fid, row in rx_rows.items()}
            smoke.require_no_mismatch(tx_hashes, rx_hashes, "TX hashlog", "RX hashlog",
                                      "Raw hash agreement")
            reference = smoke.reference_hashes(ref_ids, cfg["width"], cfg["height"])
            hash_summary["reference"] = _validate_reference(
                ref_ids, reference,
                [("TX hashlog", tx_rows, "hash"), ("RX hashlog", rx_rows, "hash")],
                cfg["frames"], "Raw motion reference")
            hash_summary["tx_rows"] = len(tx_rows)
            hash_summary["rx_rows"] = len(rx_rows)
            hash_summary["all_tx_rx_frames_agree"] = True
            hash_field = "hash"
            transport_summary = None
        else:
            tx_rows = _parse_rows(tx_hashlog_path, cfg["frames"], "TX hashlog",
                                  ("source_hash", "transport_hash"))
            rx_rows = _parse_rows(rx_hashlog_path, cfg["frames"], "RX hashlog",
                                  ("decoded_hash", "transport_hash"))
            _validate_encoded_tx_rows(tx_rows, cfg["frames"], codec["max_au_bytes"])
            _validate_encoded_rx_rows(rx_rows, cfg["frames"])
            _require(tx_hashlog_path.is_file() and rx_hashlog_path.is_file(),
                     "encoded hashlogs are missing")
            transport_rows = _parse_rows(transport_path, cfg["frames"], "receiver transport log",
                                         ("transport_hash",))
            _validate_transport_rows(transport_rows, tx_rows, cfg["frames"])

            tx_transport = {fid: row["transport_hash"] for fid, row in tx_rows.items()}
            rx_transport = {fid: row["transport_hash"] for fid, row in rx_rows.items()}
            transport_map = {fid: row["transport_hash"] for fid, row in transport_rows.items()}
            smoke.require_no_mismatch(tx_transport, transport_map, "TX transport_hash",
                                      "receiver .transport.jsonl", "Encoded transport agreement")
            smoke.require_no_mismatch(transport_map, rx_transport, "receiver .transport.jsonl",
                                      "RX hashlog transport_hash", "Encoded transport agreement")
            # The decoded hash is intentionally never compared with the source hash.
            reference = smoke.reference_hashes(ref_ids, cfg["width"], cfg["height"])
            hash_summary["reference"] = _validate_reference(
                ref_ids, reference, [("TX source_hash", tx_rows, "source_hash")],
                cfg["frames"], "Encoded source motion reference")
            hash_summary.update({
                "tx_rows": len(tx_rows),
                "rx_rows": len(rx_rows),
                "transport_rows": len(transport_rows),
                "transport_agrees": True,
            })
            _require(smoke.is_int(tx_stats.get("encoded_bytes"))
                     and tx_stats["encoded_bytes"] == sum(row["encoded_bytes"] for row in tx_rows.values()),
                     "TX stats: encoded_bytes does not equal the hashlog byte sum")
            hash_field = "decoded_hash"
            transport_summary = {
                "path": str(transport_path),
                "rows": len(transport_rows),
                "tx_transport_equals_receiver_log": True,
                "receiver_log_equals_rx_hashlog": True,
                "source_hash_equals_decoded_hash_gate": False,
            }
        report["hashlog"] = hash_summary
        report["transport"] = transport_summary

        shm_summary = _validate_shm(view, shm_name, cfg, cfg["frames"], rx_rows, hash_field, "RX SHM")
        report["shm"] = shm_summary

        metrics = _compute_metrics(tx_rows, rx_rows, cfg["frames"], codec["codec"],
                                   cfg["fps"], args.performance)
        metrics["actual_fps"]["tx_stats_fps_actual"] = tx_stats.get("fps_actual")
        metrics["actual_fps"]["rx_stats_fps_actual"] = rx_stats.get("fps_actual")
        report["metrics"] = metrics
        _validate_metrics(metrics, cfg["frames"])

        # Per-session isolation: destroy RX, then the TX handle must still be readable.
        rx_cleanup = _stop_then_destroy(api, sessions["rx"], "RX")
        sessions["rx"]["cleanup"] = rx_cleanup
        tx_status = _check_status(api.status(tx_handle, "TX after RX destroy"), tx_session,
                                  "TX after RX destroy")
        report["sessions"]["sender"]["status_after_other_session_destroyed"] = _brief(tx_status)
        report["sessions"]["receiver"]["cleanup"] = rx_cleanup
        report["module_loaded_after_other_session_destroyed"] = _module_loaded()

        tx_cleanup = _stop_then_destroy(api, sessions["tx"], "TX")
        sessions["tx"]["cleanup"] = tx_cleanup
        report["sessions"]["sender"]["cleanup"] = tx_cleanup
        report["destroy_session_null_rejected"] = True
        _require(_module_loaded() is not False,
                 "vc_stream.dll was unloaded while session handles still existed")

        report["pending_probe"] = _run_pending_probe(api, cfg, run_dir, token, report)
    finally:
        if view is not None:
            view.close()
        pending = []
        for role in ("tx", "rx"):
            entry = sessions.get(role)
            if entry is None or entry.get("destroyed"):
                continue
            try:
                entry["cleanup"] = _stop_then_destroy(api, entry, f"{role.upper()} cleanup")
            except Exception as exc:  # noqa: BLE001 - keep the original failure visible
                pending.append(f"{role.upper()} cleanup: {exc}")
                entry["cleanup"] = {"error": str(exc)}
        report["cleanup"] = {role: sessions[role].get("cleanup") for role in sessions}
        report["cleanup_errors"] = pending
        if pending:
            _hard_exit_pending(run_dir, report, "; ".join(pending))


# ---------------------------------------------------------------------------
# Self-check (never loads the DLL)


class _FakeView:
    def __init__(self, data):
        self._data = bytes(data)

    def read(self, offset, size):
        return self._data[offset:offset + size]


def self_check(args):
    checks = {}

    expected_offsets = {
        "abi_version": 0, "struct_size": 4, "list_adapters": 8, "start_session": 16,
        "session_status": 24, "stop_session": 32, "destroy_session": 40, "capabilities": 48,
    }
    _require(ctypes.sizeof(ctypes.c_void_p) == 8, "self-check expects 64-bit Python")
    _require(ctypes.sizeof(VcStreamApiV1) == 56,
             f"VcStreamApiV1 size {ctypes.sizeof(VcStreamApiV1)} != 56")
    for name, offset in expected_offsets.items():
        actual = getattr(VcStreamApiV1, name).offset
        _require(actual == offset, f"VcStreamApiV1.{name} offset {actual} != {offset}")
    checks["abi_layout"] = {"size": 56, "offsets": expected_offsets}

    golden = smoke.reference_hashes([0], 640, 640)
    _require(golden[0] == "f6f53d2aeac3c9dc",
             f"motion 640x640 frame 0 reference {golden[0]} != f6f53d2aeac3c9dc")
    checks["motion_golden_frame0"] = golden[0]

    cfg = smoke.build_config(DEFAULT_FRAMES, DEFAULT_FPS, DEFAULT_WIDTH, DEFAULT_HEIGHT,
                             DEFAULT_MTU, 1)
    _require(cfg["frame_bytes"] == 614400 and cfg["total_packets"] == 69,
             f"unexpected Loopback config: {cfg}")
    checks["loopback_config"] = {"frame_bytes": cfg["frame_bytes"],
                                 "total_packets": cfg["total_packets"],
                                 "receiver_timeout_s": cfg["receiver_timeout_s"]}

    codec = _resolve_codec_options(args)
    checks["codec_config"] = codec
    _validate_codec_config(codec, DEFAULT_WIDTH, DEFAULT_HEIGHT, DEFAULT_FPS)
    bad = _codec_config("mjpeg", None, None, None, None)
    bad["experimental"] = False
    try:
        _validate_codec_config(bad, DEFAULT_WIDTH, DEFAULT_HEIGHT, DEFAULT_FPS)
    except smoke.SmokeError as exc:
        checks["experimental_rejection"] = str(exc)
    else:
        raise smoke.SmokeError("self-check: mjpeg without experimental was accepted")

    source = _source_config()
    mac = _random_source_mac()
    _require((mac[0] & 0x03) == 0x02, f"source MAC {mac} is not locally administered unicast")
    sender = _sender_request(cfg, "self-tx", codec, source, mac,
                             Path.cwd() / "self-tx-stats.json", Path.cwd() / "self-tx-hash.jsonl")
    receiver = _receiver_request(cfg, "self-rx", codec, mac,
                                 SHM_PREFIX + "self-check",
                                 Path.cwd() / "self-rx-stats.json", Path.cwd() / "self-rx-hash.jsonl")
    _require(set(sender) == {"adapter", "width", "height", "fps", "mtu", "session", "frames",
                             "pacing_us", "source", "codec", "source_mac", "dst_mac", "stats", "hashlog"},
             f"self-check: sender request keys {sorted(sender)}")
    _require(set(receiver) == {"adapter", "width", "height", "fps", "mtu", "session", "codec",
                               "source_mac", "expect_frames", "timeout_s", "shm", "stats", "hashlog"},
             f"self-check: receiver request keys {sorted(receiver)}")
    _require(set(source) == {"mode", "pattern", "output", "window", "transform", "gpu_convert"},
             "self-check: source config keys")
    _require(set(codec) == {"codec", "experimental", "encoder_backend", "decoder_backend",
                            "jpeg_quality", "max_au_bytes", "max_bitrate_bps", "decode_in_flight"},
             "self-check: codec config keys")
    _require(sender["adapter"] == smoke.LOOPBACK and sender["source_mac"] == mac
             and sender["dst_mac"] == DEST_MAC and sender["pacing_us"] == 0,
             "self-check: sender request values")
    _require(receiver["expect_frames"] == DEFAULT_FRAMES
             and receiver["timeout_s"] == cfg["receiver_timeout_s"],
             "self-check: receiver request values")
    checks["requests"] = {"sender_keys": sorted(sender), "receiver_keys": sorted(receiver)}

    _require(smoke.SHM_MAGIC == 0x33414356 and smoke.SHM_VERSION == 3
             and smoke.SHM_SLOTS == 12 and smoke.SHM_DATA_OFF == 4096
             and smoke.SHM_HDR_SIZE == 44 and smoke.SHM_SLOT_SIZE == 36,
             "self-check: SHM v3 constants/layout changed")

    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / "rows.jsonl"
        path.write_text('{"frame_id":0,"hash":"0000000000000000"}\n'
                        '{"frame_id":1,"hash":"aBcDeF0123456789"}\n', encoding="utf-8")
        rows = _parse_rows(path, 2, "self-check rows", ("hash",))
        _require(rows[1]["hash"] == "abcdef0123456789", "self-check: hash normalisation")
        path.write_text('{"frame_id":0,"hash":"0000000000000000"}\n'
                        '{"frame_id":0,"hash":"1111111111111111"}\n', encoding="utf-8")
        try:
            _parse_rows(path, 1, "self-check duplicate", ("hash",))
        except smoke.SmokeError:
            pass
        else:
            raise smoke.SmokeError("self-check: duplicate hash rows were accepted")
    checks["row_parser"] = "duplicate and normalisation checks passed"

    summary = _summarize_ns([1_000_000, 2_000_000, 3_000_000, 10_000_000])
    _require(summary["samples"] == 4 and summary["p50_ms"] == 2.0
             and summary["p99_ms"] == 10.0 and summary["max_ms"] == 10.0
             and summary["gte_10ms"] == 1, f"self-check: percentile summary {summary}")
    checks["percentiles"] = summary

    # The extra local performance gate must reject a slow sender even when its
    # frame-generation timeline looks correct, and reject the 10 ms boundary.
    tx = {fid: {"qpc_ns": 1_000_000_000 + fid * 4_166_667,
                "send_qpc_ns": 1_001_000_000 + fid * 4_166_667}
          for fid in range(4)}
    rx = {fid: {"qpc_ns": row["send_qpc_ns"] + 2_000_000} for fid, row in tx.items()}
    valid_metrics = _compute_metrics(tx, rx, 4, "mjpeg", 240, True)
    _validate_metrics(valid_metrics, 4)
    _require(valid_metrics["performance_pass"], "self-check: valid performance gate failed")
    rejected = []
    for key, field, value in (("tx_interval_ms", "mean_ms", 4.3),
                              ("tx_send_interval_ms", "mean_ms", 4.3),
                              ("send_to_rx_ms", "p99_ms", 10.0),
                              ("start_to_rx_ms", "p99_ms", 10.0)):
        candidate = copy.deepcopy(valid_metrics)
        candidate[key][field] = value
        candidate["performance_pass"] = False
        try:
            _validate_metrics(candidate, 4)
        except smoke.SmokeError:
            rejected.append(key)
        else:
            raise smoke.SmokeError(f"self-check: performance gate accepted invalid {key}")
    checks["performance_gate"] = {"valid_trace": "passed", "rejected": rejected}

    # Exercise the text-query and start-failure callers against Python callables.
    def fake_text(out, capacity, written, error, error_capacity, error_written):
        payload = b'{"ok":true}\0'
        written[0] = len(payload)
        if out is None or capacity < len(payload):
            return 1
        ctypes.memmove(out, payload, len(payload))
        return 0

    value, text = _invoke_text(fake_text, "self-check fake text")
    _require(value == {"ok": True} and text == '{"ok":true}', "self-check: fake text query")

    def fake_start(role, request, handle, error, error_capacity, error_written):
        handle[0] = None
        message = b"invalid fake request"
        error_written[0] = len(message) + 1
        count = min(len(message), error_capacity - 1)
        ctypes.memmove(error, message[:count], count)
        error[count] = b"\0"
        return -1

    result = _invoke_start(fake_start, 1, b"{}", 4)
    _require(result["code"] == -1 and result["handle"] is None
             and result["error_written"] == len(b"invalid fake request") + 1
             and result["error"] == "inv",
             f"self-check: fake start failure semantics {result}")
    checks["caller_buffers"] = {"fake_text_ok": True, "tiny_error": result}

    # Validate the real SHM parser against a synthetic v3 mapping.
    width, height = 4, 2
    frame_bytes = width * height * 3 // 2
    frame_id = 4
    frames = 5
    y_plane, uv_plane = smoke.pattern_ref.gen_native("motion", frame_id, width, height)
    payload = y_plane + uv_plane
    digest = smoke.pattern_ref.frame_hash_yuv(y_plane, uv_plane)
    header = struct.pack(smoke.SHM_HDR_FMT, smoke.SHM_MAGIC, smoke.SHM_VERSION, smoke.SHM_SLOTS,
                         frame_bytes, smoke.SHM_DATA_OFF, 0, frames, os.getpid(),
                         width, height, 60)
    header += b"\0" * (SHM_HEADER_BYTES - len(header))
    slot = struct.pack(smoke.SHM_SLOT_FMT, 2, frame_id, 111, 222, int(digest, 16), frame_id)
    slot += b"\0" * (SHM_SLOT_BYTES - len(slot))
    mapping = bytearray(smoke.SHM_DATA_OFF + smoke.SHM_SLOTS * frame_bytes)
    mapping[0:SHM_HEADER_BYTES] = header
    mapping[SHM_HEADER_BYTES:SHM_HEADER_BYTES + SHM_SLOT_BYTES] = slot
    start = smoke.SHM_DATA_OFF + 0 * frame_bytes
    mapping[start:start + frame_bytes] = payload
    shm_cfg = {"frame_bytes": frame_bytes, "width": width, "height": height, "fps": 60}
    shm = _validate_shm(_FakeView(mapping), "Local\\self-check", shm_cfg, frames,
                        {frame_id: {"hash": digest}}, "hash", "self-check SHM")
    _require(shm["latest_frame_id"] == frame_id and shm["latest_seq"] == frames
             and shm["latest_hash"] == digest, f"self-check: SHM parse {shm}")
    checks["shm_layout"] = shm

    first_name = SHM_PREFIX + uuid.uuid4().hex[:12]
    second_name = SHM_PREFIX + uuid.uuid4().hex[:12]
    _require(first_name != second_name and first_name.startswith("Local\\"),
             "self-check: SHM names are not unique Local\\ names")
    checks["shm_names"] = [first_name, second_name]

    checks["dll_loaded"] = False
    checks["subprocesses"] = []
    checks["note"] = "--self-check never loads vc_stream.dll and is not a live substitute"
    return checks


# ---------------------------------------------------------------------------
# Main


def parse_args(argv=None):
    parser = argparse.ArgumentParser(
        description="Same-process vc_stream.dll ABI v1 smoke/performance probe on Npcap Loopback.")
    parser.add_argument("--package", type=Path,
                        help="extracted release directory containing vc_stream.dll")
    parser.add_argument("--dll", type=Path,
                        help="direct path to vc_stream.dll (overrides --package for the module)")
    parser.add_argument("--frames", type=int, default=DEFAULT_FRAMES,
                        help=f"finite frame count (default {DEFAULT_FRAMES})")
    parser.add_argument("--fps", type=int, default=DEFAULT_FPS,
                        help=f"paced frame rate (default {DEFAULT_FPS})")
    parser.add_argument("--width", type=int, default=DEFAULT_WIDTH,
                        help=f"NV12 width (default {DEFAULT_WIDTH})")
    parser.add_argument("--height", type=int, default=DEFAULT_HEIGHT,
                        help=f"NV12 height (default {DEFAULT_HEIGHT})")
    parser.add_argument("--mtu", type=int, default=DEFAULT_MTU,
                        help=f"L2 MTU (default {DEFAULT_MTU})")
    parser.add_argument("--codec", choices=("raw", "mjpeg", "pyrowave"), default="raw",
                        help="data-plane codec (default raw; encoded modes are explicit)")
    parser.add_argument("--jpeg-quality", type=int, default=None,
                        help="MJPEG quality 1..100 (only with --codec mjpeg)")
    parser.add_argument("--max-au-bytes", type=int, default=None,
                        help="encoded access-unit cap 1024..16777216 (encoded modes)")
    parser.add_argument("--max-bitrate-bps", type=int, default=None,
                        help="one-second send budget in bits/s (encoded modes; default 800 Mbps "
                             "for mjpeg, 400 Mbps for pyrowave)")
    parser.add_argument("--decode-in-flight", type=int, default=None,
                        help="configured decoder in-flight limit 1..8 (encoded modes; default 2)")
    parser.add_argument("--ref-samples", type=int, default=DEFAULT_REF_SAMPLES,
                        help="motion reference frames: 0 = all (default), otherwise a spread")
    parser.add_argument("--performance", action="store_true",
                        help="also gate mean TX frame/send cadence within 1% and both send/start-to-RX p99 < 10 ms")
    parser.add_argument("--self-check", action="store_true",
                        help="check pure ctypes/layout/hash helpers only; never loads the DLL")
    return parser.parse_args(argv)


def _resolve_module(args):
    if args.dll is not None:
        dll = args.dll.resolve()
        package = args.package.resolve() if args.package is not None else None
        if package is not None and not package.is_dir():
            raise smoke.SmokeError(f"--package is not a directory: {package}")
        return dll, package
    if args.package is None:
        raise smoke.SmokeError("--package DIR is required for the live DLL smoke (or pass --dll PATH)")
    package = args.package.resolve()
    if not package.is_dir():
        raise smoke.SmokeError(f"--package is not a directory: {package}")
    return package / DLL_NAME, package


def main(argv=None):
    args = parse_args(argv)

    if args.self_check:
        try:
            checks = self_check(args)
        except Exception as exc:  # noqa: BLE001 - self-check reports any failure as JSON
            _print_record({"self_check": "FAIL", "error": str(exc),
                           "traceback": traceback.format_exc()})
            return 1
        _print_record({"self_check": "PASS", **checks})
        return 0

    try:
        dll_path, package = _resolve_module(args)
    except Exception as exc:  # noqa: BLE001
        _print_record({"status": "FAIL", "phase": "arguments", "error": str(exc)})
        return 2

    try:
        run_dir, token = _make_run_dir()
    except Exception as exc:  # noqa: BLE001
        _print_record({"status": "FAIL", "phase": "evidence", "error": str(exc)})
        return 2
    report = {
        "status": "RUNNING",
        "run_dir": str(run_dir),
        "token": token,
        "dll_path": str(dll_path),
        "package": str(package) if package else None,
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    try:
        execute(args, dll_path, package, run_dir, token, report)
    except Exception as exc:  # noqa: BLE001 - preserve evidence and report a structured failure
        report["status"] = "FAIL"
        report["failure"] = {
            "error": str(exc),
            "type": type(exc).__name__,
            "traceback": traceback.format_exc(),
        }
        try:
            _write_json_exclusive(run_dir / "failure.json", report)
        except OSError as write_error:
            report["failure"]["evidence_error"] = str(write_error)
        _print_record({
            "status": "FAIL",
            "run_dir": str(run_dir),
            "error": str(exc),
            "run_json": str(run_dir / "failure.json"),
            "dll_sha256": report.get("dll", {}).get("sha256"),
            "config": report.get("config"),
        })
        if isinstance(exc, UntrackedSession):
            # The only handle we could not free came from a probe that should
            # never have started. Leave the DLL loaded by exiting without
            # interpreter teardown.
            sys.stderr.flush()
            os._exit(1)
        return 1

    report["status"] = "PASS"
    report["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    _write_json_exclusive(run_dir / "report.json", report)
    _print_record({
        "status": "PASS",
        "run_dir": str(run_dir),
        "run_json": str(run_dir / "report.json"),
        "dll_sha256": report.get("dll", {}).get("sha256"),
        "config": {
            "codec": report["config"]["codec"]["codec"],
            "frames": report["config"]["frames"],
            "fps": report["config"]["fps"],
            "width": report["config"]["width"],
            "height": report["config"]["height"],
            "mtu": report["config"]["mtu"],
            "ref_samples": report["config"]["ref_samples_requested"],
            "performance": report["config"]["performance"],
        },
        "hashlog": report.get("hashlog"),
        "shm": {key: report.get("shm", {}).get(key)
                for key in ("latest_frame_id", "latest_seq", "latest_hash", "writer_pid")},
        "metrics": {
                "actual_fps": report.get("metrics", {}).get("actual_fps"),
            "tx_interval_ms": report.get("metrics", {}).get("tx_interval_ms"),
            "tx_send_interval_ms": report.get("metrics", {}).get("tx_send_interval_ms"),
            "send_to_rx_ms": report.get("metrics", {}).get("send_to_rx_ms"),
            "start_to_rx_ms": report.get("metrics", {}).get("start_to_rx_ms"),
            "frames_ge_10ms_count": report.get("metrics", {}).get("frames_ge_10ms_count"),
            "performance_pass": report.get("metrics", {}).get("performance_pass"),
        },
        "pending_probe": report.get("pending_probe"),
        "subprocesses": [],
    })
    return 0


if __name__ == "__main__":
    sys.exit(main())
