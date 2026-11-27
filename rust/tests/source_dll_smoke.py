"""Real DDA/WGC capture smoke through the release `vc_stream.dll` ABI.

The script loads a *copy* of the release `vc_stream.dll` from a private
`build/source-dll-smoke-<id>/` directory, starts an in-process Raw receiver and
sender on the exact Npcap Loopback adapter, and drives the real GPU desktop
capture backends:

* DDA GPU `stretch` and GPU `fill` (GPU integer BGRA->NV12 with centre crop)
  against the private animated fixture window on the desktop;
* WGC GPU `crop` against the private Tk window, including a live window resize
  that must make the frame pool recreate and the source dimensions update;
* WGC GPU `stretch` against the same private window (integer scaling);
* DDA and WGC unlimited-sender cooperative stop runs (`frames=0`): after real
  frames advance, stopping the exact sender session must return from native
  stop/destroy within the 2 s cooperative-stop grace while the DLL stays loaded;
* a WGC close lifecycle run (sender only) where closing the fixture window
  must surface the terminal `WGC capture item closed` error instead of
  hanging or silently repeating frames.

Desktop capture is GPU-only here: every combo requests `gpu_convert=true`.
There is no CPU conversion path, no backend substitution and no fallback; a
missing GPU conversion, source or DXGI output is an explicit failure (or an
explicit DDA BLOCKED record with --allow-dda-unavailable).

Every combo uses a unique session id, source MAC and `Local\\...` SHM v3
mapping.  No registry, DirectShow filter, agent/service, physical NIC, PC2 or
deployed :5143 runtime is touched.  The Tk fixture is a test-only child
process and is never part of the product package.

Gates (functional, not capacity):
* the ABI reports sources dda/wgc, Raw, SHM v3 and NV12;
* sender ready/stats report the requested source mode/transform/gpu_convert,
  a real GPU adapter name, source/output dimensions and no silent fallback;
* all requested frames are captured, sent and received; TX/RX Raw FNV hashes
  agree on every frame; the latest SHM v3 frame matches the receiver hash;
* captured NV12 is non-blank and changes across frames (the fixture animates);
* the resize run observes new WGC source dimensions and still completes;
* unlimited DDA/WGC senders advance real TX/RX frames before a cooperative
  stop; the exact sessions stop and destroy within 2 s and keep the DLL loaded;
* the private DDA fixture is made visible/topmost and its hwnd, rect,
  visibility and output-0/primary-proxy monitor evidence is recorded; only that
  fixture is restored/closed;
* the close run exits `running=false, exit_code=1` with a closed-item error;
* every session stops/destroys cleanly and the DLL stays loaded until then.

Evidence is written to a fresh `run/rust-source-dll-smoke-<id>/` directory.
Measured `fps_actual` values are reported verbatim; this is a same-host
Loopback functional smoke, never a 240 fps, physical-link, two-machine or
capacity claim.

Usage (from the repository root)::

    python -I rust/tests/source_dll_smoke.py
    python -I rust/tests/source_dll_smoke.py --allow-dda-unavailable
    python -I rust/tests/source_dll_smoke.py --dll rust/target/release/vc_stream.dll
    python -I rust/tests/source_dll_smoke.py --self-check
"""
from __future__ import annotations

import argparse
import ctypes
import ctypes.wintypes as wintypes
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import time
import traceback
import uuid

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))
import dll_stream_smoke as dll  # noqa: E402
import raw_stream_smoke as smoke  # noqa: E402

ROOT = smoke.ROOT
DEFAULT_DLL = ROOT / "rust" / "target" / "release" / "vc_stream.dll"
BUILD_PREFIX = "source-dll-smoke-"
RUN_PREFIX = "rust-source-dll-smoke-"
FIXTURE_TITLE_PREFIX = "VirtualCapture Source Smoke "
DEST_MAC = [0xFF] * 6
MTU = 9000
FPS = 30
DDA_WIDTH, DDA_HEIGHT = 320, 180
WGC_WIDTH, WGC_HEIGHT = 320, 240
DDA_FRAMES = 45
WGC_FRAMES = 45
RESIZE_FRAMES = 90
RESIZE_AFTER_FRAMES = 10
RESIZE_TO = (640, 480)
CLOSE_MIN_FRAMES = 5
COOP_MIN_FRAMES = 10
COOP_STOP_GRACE_S = 2.0
COOP_FRAME_TIMEOUT_S = 15.0

if sys.platform == "win32":
    class MONITORINFO(ctypes.Structure):
        _fields_ = [("cbSize", wintypes.DWORD),
                    ("rcMonitor", wintypes.RECT),
                    ("rcWork", wintypes.RECT),
                    ("dwFlags", wintypes.DWORD)]

    USER32 = ctypes.WinDLL("user32", use_last_error=True)
    USER32.EnumWindows.argtypes = [ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND,
                                                      wintypes.LPARAM), wintypes.LPARAM]
    USER32.EnumWindows.restype = wintypes.BOOL
    USER32.IsWindowVisible.argtypes = [wintypes.HWND]
    USER32.IsWindowVisible.restype = wintypes.BOOL
    USER32.IsIconic.argtypes = [wintypes.HWND]
    USER32.IsIconic.restype = wintypes.BOOL
    USER32.GetWindowTextLengthW.argtypes = [wintypes.HWND]
    USER32.GetWindowTextLengthW.restype = ctypes.c_int
    USER32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
    USER32.GetWindowTextW.restype = ctypes.c_int
    USER32.GetWindowThreadProcessId.argtypes = [wintypes.HWND,
                                                ctypes.POINTER(wintypes.DWORD)]
    USER32.GetWindowThreadProcessId.restype = wintypes.DWORD
    USER32.SetWindowPos.argtypes = [wintypes.HWND, wintypes.HWND, ctypes.c_int,
                                    ctypes.c_int, ctypes.c_int, ctypes.c_int,
                                    ctypes.c_uint]
    USER32.SetWindowPos.restype = wintypes.BOOL
    USER32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT,
                                    wintypes.WPARAM, wintypes.LPARAM]
    USER32.PostMessageW.restype = wintypes.BOOL
    USER32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
    USER32.GetWindowRect.restype = wintypes.BOOL
    USER32.MonitorFromWindow.argtypes = [wintypes.HWND, wintypes.DWORD]
    USER32.MonitorFromWindow.restype = ctypes.c_void_p
    USER32.GetMonitorInfoW.argtypes = [ctypes.c_void_p, ctypes.POINTER(MONITORINFO)]
    USER32.GetMonitorInfoW.restype = wintypes.BOOL
else:
    USER32 = None

WM_CLOSE = 0x0010
HWND_TOPMOST = -1
HWND_NOTOPMOST = -2
SWP_NOSIZE = 0x0001
SWP_NOMOVE = 0x0002
SWP_NOZORDER = 0x0004
SWP_NOACTIVATE = 0x0010
SWP_SHOWWINDOW = 0x0040
MONITOR_DEFAULTTOPRIMARY = 1
MONITOR_DEFAULTTONEAREST = 2
MONITORINFOF_PRIMARY = 0x1


class FixtureError(RuntimeError):
    """The private capture fixture could not be started/controlled."""


def _require(condition, message):
    smoke.require(condition, message)


def _is_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _sha256(path):
    with Path(path).open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


# ---------------------------------------------------------------------------
# Private window fixture (child process; never packaged)


def _fixture_main(args):
    import tkinter as tk

    root = tk.Tk()
    root.title(args.title)
    root.geometry(f"{args.width}x{args.height}+90+90")
    canvas = tk.Canvas(root, bg="#101018", highlightthickness=0)
    canvas.pack(fill="both", expand=True)
    state = {"frame": 0}

    def draw():
        index = state["frame"]
        state["frame"] = index + 1
        width = max(canvas.winfo_width(), 2)
        height = max(canvas.winfo_height(), 2)
        canvas.delete("all")
        # Colour bands change every frame so the DDA and WGC GPU captures
        # see new pixels even at a 30 fps sender cadence.
        for band in range(8):
            x0 = band * width // 8
            x1 = (band + 1) * width // 8
            red = (index * 17 + band * 31) & 0xFF
            green = (index * 29 + band * 47) & 0xFF
            blue = (index * 43 + band * 59) & 0xFF
            canvas.create_rectangle(x0, 0, x1, height,
                                    fill=f"#{red:02x}{green:02x}{blue:02x}",
                                    outline="")
        bar_h = max(height // 6, 2)
        bar_y = (index * 19) % max(height - bar_h, 1)
        canvas.create_rectangle(0, bar_y, width, bar_y + bar_h,
                                fill="#f0f0f0", outline="")
        radius = max(min(width, height) // 6, 4)
        cx = (index * 23) % max(width - radius, 1) + radius
        cy = (index * 31) % max(height - radius, 1) + radius
        canvas.create_oval(cx - radius, cy - radius, cx + radius, cy + radius,
                           fill="#ff5020", outline="#20ff80", width=3)
        canvas.create_text(8, 8, anchor="nw", text=f"frame={index}",
                           fill="white", font=("Consolas", 12))
        # Keep all 16 luma levels inside a centred crop after resizing.  The
        # moving bands alone can leave only 4 bands + 3 overlay colours in a
        # 320x240 crop; that is valid pixels but insufficient fixture coverage
        # for the unchanged >=8 distinct-Y non-blank gate.
        left = width // 2 - 32
        top = height // 2 - 32
        for row in range(4):
            for column in range(4):
                level = (row * 4 + column) * 17
                colour = f"#{level:02x}{level:02x}{level:02x}"
                canvas.create_rectangle(left + column * 16, top + row * 16,
                                        left + (column + 1) * 16,
                                        top + (row + 1) * 16,
                                        fill=colour, outline="")
        root.after(15, draw)

    draw()
    root.mainloop()
    return 0


def _find_window(substring, process_id=None):
    if USER32 is None:
        raise FixtureError("window discovery requires Windows")
    matches = []

    def callback(hwnd, _lparam):
        if not USER32.IsWindowVisible(hwnd) or USER32.IsIconic(hwnd):
            return True
        length = USER32.GetWindowTextLengthW(hwnd)
        if length <= 0:
            return True
        buffer = ctypes.create_unicode_buffer(length + 1)
        copied = USER32.GetWindowTextW(hwnd, buffer, length + 1)
        if copied <= 0 or substring.lower() not in buffer.value.lower():
            return True
        pid = wintypes.DWORD(0)
        USER32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
        if process_id is not None and int(pid.value) != int(process_id):
            return True
        matches.append((int(hwnd), int(pid.value), buffer.value))
        return False

    callback_ref = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND,
                                      wintypes.LPARAM)(callback)
    USER32.EnumWindows(callback_ref, 0)
    return matches[0] if matches else None


def _wait_window(process, substring, label, timeout=12.0):
    deadline = time.monotonic() + timeout
    while True:
        found = _find_window(substring, process.pid)
        if found is not None:
            return found
        code = process.poll()
        if code is not None:
            stderr = ""
            if process.stderr is not None:
                try:
                    stderr = process.stderr.read()
                except OSError:
                    pass
            raise FixtureError(f"{label}: process exited {code} before its window appeared; "
                               f"stderr={stderr[-800:]!r}")
        if time.monotonic() >= deadline:
            raise FixtureError(f"{label}: window containing {substring!r} did not appear "
                               f"within {timeout:.1f}s")
        time.sleep(0.05)


def _start_fixture(title, width, height):
    process = subprocess.Popen(
        [sys.executable, "-I", str(Path(__file__).resolve()),
         "--window-fixture", "--title", title,
         "--width", str(width), "--height", str(height)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        found = _wait_window(process, title, "Tk fixture")
    except Exception:
        _terminate_silently(process)
        raise
    return process, found[0]


def _close_window(hwnd):
    return bool(USER32.PostMessageW(hwnd, WM_CLOSE, 0, 0))


def _resize_window(hwnd, width, height):
    return bool(USER32.SetWindowPos(hwnd, None, 0, 0, width, height,
                                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE))


def _rect_values(rect):
    return [int(rect.left), int(rect.top), int(rect.right), int(rect.bottom)]


def _monitor_evidence(hwnd):
    monitor = USER32.MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
    primary = USER32.MonitorFromWindow(None, MONITOR_DEFAULTTOPRIMARY)
    info = MONITORINFO()
    info.cbSize = ctypes.sizeof(MONITORINFO)
    valid = bool(USER32.GetMonitorInfoW(monitor, ctypes.byref(info))) if monitor else False
    return {
        "monitor": int(monitor) if monitor else None,
        "primary_monitor": int(primary) if primary else None,
        "monitor_rect": _rect_values(info.rcMonitor) if valid else None,
        "monitor_work": _rect_values(info.rcWork) if valid else None,
        "primary_monitor_flag": bool(info.dwFlags & MONITORINFOF_PRIMARY) if valid else False,
        "on_primary": bool(monitor and primary and int(monitor) == int(primary)),
    }


def _fixture_evidence(hwnd, label):
    """Record only the private fixture window; never inspect/modify other windows."""
    if USER32 is None:
        raise FixtureError(f"{label}: fixture window control requires Windows")
    rect = wintypes.RECT()
    _require(bool(USER32.GetWindowRect(hwnd, ctypes.byref(rect))),
             f"{label}: GetWindowRect failed, win32 error {ctypes.get_last_error()}")
    monitor = _monitor_evidence(hwnd)
    return {
        "hwnd": int(hwnd),
        "rect": _rect_values(rect),
        "visible": bool(USER32.IsWindowVisible(hwnd)),
        "iconic": bool(USER32.IsIconic(hwnd)),
        # DXGI output ordinals are not in the ready marker; primary-monitor
        # membership is the verifiable proxy recorded for requested output 0.
        "chosen_output": 0,
        "chosen_output_proxy": "primary_monitor",
        "on_chosen_output": monitor["on_primary"],
        **monitor,
    }


def _set_fixture_topmost(hwnd, topmost):
    """Make only the private fixture visible/topmost, or restore its z-order."""
    if USER32 is None:
        raise FixtureError("fixture window control requires Windows")
    insert_after = HWND_TOPMOST if topmost else HWND_NOTOPMOST
    flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE
    if topmost:
        flags |= SWP_SHOWWINDOW
    action = "topmost" if topmost else "restore"
    _require(bool(USER32.SetWindowPos(hwnd, insert_after, 0, 0, 0, 0, flags)),
             f"fixture: SetWindowPos({action}) failed, win32 error "
             f"{ctypes.get_last_error()}")
    evidence = _fixture_evidence(hwnd, f"fixture {action}")
    if topmost:
        _require(evidence["visible"] and not evidence["iconic"],
                 f"fixture: window is not visible after topmost: {evidence}")
    return evidence


def _stop_process(process, hwnd=None, timeout=5.0):
    if process is None:
        return {"was_running": False}
    was_running = process.poll() is None
    if hwnd and was_running and USER32 is not None:
        # Post only while the handle still belongs to the private fixture;
        # never close a recycled or foreign window.
        pid = wintypes.DWORD(0)
        USER32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
        if int(pid.value) == int(process.pid):
            _close_window(hwnd)
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=timeout)
    stderr = ""
    if process.stderr is not None:
        try:
            stderr = process.stderr.read()
        except OSError:
            pass
    return {"was_running": was_running, "exit_code": process.returncode,
            "stderr_tail": stderr[-800:]}


def _terminate_silently(process):
    if process is None:
        return
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


# ---------------------------------------------------------------------------
# DLL staging and requests


def _stage_module(source_dll, token):
    source_dll = Path(source_dll).resolve()
    _require(source_dll.is_file(), f"release vc_stream.dll is missing: {source_dll}")
    stage = (ROOT / "build" / f"{BUILD_PREFIX}{token}").resolve()
    stage.mkdir(parents=True, exist_ok=False)
    copied = []
    for name in ("vc_stream.dll",):
        origin = source_dll.parent / name
        _require(origin.is_file(), f"release DLL is missing: {origin}")
        target = stage / name
        shutil.copy2(origin, target)
        copied.append({"name": name, "source": str(origin),
                       "staged": str(target), "sha256": _sha256(target)})
    return stage, stage / "vc_stream.dll", copied


def _source_config(mode, window, transform, gpu_convert):
    return {
        "mode": mode,
        "pattern": "motion",
        "output": 0,
        "window": window,
        "transform": transform,
        "gpu_convert": bool(gpu_convert),
    }


def _unlimited_config(width, height):
    """frames=0 keeps an unlimited sender/receiver alive until an exact stop."""
    frame_bytes = width * height * 3 // 2
    payload = MTU - smoke.PACKET_OVERHEAD
    return {
        "frames": 0,
        "fps": FPS,
        "width": width,
        "height": height,
        "mtu": MTU,
        "frame_bytes": frame_bytes,
        "total_packets": (frame_bytes + payload - 1) // payload,
        "ready_timeout_s": 15.0,
        "stream_timeout_s": 30.0,
        "receiver_timeout_s": 30,
    }


def _codec_raw():
    return dll._codec_config("raw", None, None, None)


def _sender_request(cfg, session, source, source_mac, stats, hashlog):
    return dll._sender_request(cfg, session, _codec_raw(), source, source_mac,
                               stats, hashlog)


def _receiver_request(cfg, session, source_mac, shm_name, stats, hashlog):
    return dll._receiver_request(cfg, session, _codec_raw(), source_mac, shm_name,
                                 stats, hashlog)


# ---------------------------------------------------------------------------
# Status helpers


def _wait_counter(api, handle, session_id, label, key, minimum, timeout):
    deadline = time.monotonic() + timeout
    last = None
    while True:
        status = dll._check_status(api.status(handle, label), session_id, label)
        last = status
        if status.get("running") is False:
            raise dll.DllError(f"{label}: worker ended before reaching {minimum} {key}: "
                               f"{dll._brief(status)}")
        value = status.get(key)
        if smoke.is_int(value) and value >= minimum:
            return status
        if time.monotonic() >= deadline:
            raise dll.DllError(f"{label}: only {value!r} {key} after {timeout:.1f}s "
                               f"(wanted >= {minimum}): {dll._brief(last)}")
        time.sleep(dll.POLL_SECONDS)


def _wait_frames(api, handle, session_id, label, minimum, timeout):
    return _wait_counter(api, handle, session_id, label, "frames", minimum, timeout)


def _wait_complete(api, handle, session_id, label, minimum, timeout):
    return _wait_counter(api, handle, session_id, label, "complete", minimum, timeout)


def _wait_stopped(api, handle, session_id, label, timeout):
    deadline = time.monotonic() + timeout
    last = None
    while True:
        status = dll._check_status(api.status(handle, label), session_id, label)
        last = status
        if status.get("running") is False:
            return status
        if time.monotonic() >= deadline:
            raise dll.DllError(f"{label}: worker did not stop within {timeout:.1f}s: "
                               f"{dll._brief(last)}")
        time.sleep(dll.POLL_SECONDS)


def _wait_done(api, handle, session_id, label, deadline):
    return dll._wait_done(api, handle, session_id, label, deadline)


# ---------------------------------------------------------------------------
# Source and evidence validators


def _validate_source_info(info, cfg, source, label):
    _require(isinstance(info, dict), f"{label}: source_info is not an object")
    for key in ("mode", "window", "transform"):
        _require(info.get(key) == source[key],
                 f"{label}: source_info[{key!r}] {info.get(key)!r} != {source[key]!r}")
    _require(info.get("output") == source["output"],
             f"{label}: source_info output {info.get('output')!r} != {source['output']!r}")
    _require(info.get("gpu_convert") is source["gpu_convert"],
             f"{label}: source_info gpu_convert {info.get('gpu_convert')!r} != "
             f"{source['gpu_convert']!r}")
    _require(isinstance(info.get("gpu"), str) and info["gpu"].strip(),
             f"{label}: source_info gpu adapter name is empty")
    source_w = info.get("source_width")
    source_h = info.get("source_height")
    _require(smoke.is_int(source_w) and 2 <= source_w <= 16384,
             f"{label}: source_width {source_w!r}")
    _require(smoke.is_int(source_h) and 2 <= source_h <= 16384,
             f"{label}: source_height {source_h!r}")
    _require(info.get("output_width") == cfg["width"]
             and info.get("output_height") == cfg["height"],
             f"{label}: source_info output geometry "
             f"{info.get('output_width')}x{info.get('output_height')} != "
             f"{cfg['width']}x{cfg['height']}")
    _require(info.get("frame_bytes") == cfg["frame_bytes"],
             f"{label}: source_info frame_bytes {info.get('frame_bytes')!r} != "
             f"{cfg['frame_bytes']}")
    skipped = info.get("source_skipped")
    _require(smoke.is_int(skipped) and skipped >= 0,
             f"{label}: source_skipped {skipped!r}")
    if source["transform"] == "crop":
        _require(source_w >= cfg["width"] and source_h >= cfg["height"],
                 f"{label}: crop source {source_w}x{source_h} is smaller than "
                 f"{cfg['width']}x{cfg['height']}")
    return {"source_width": source_w, "source_height": source_h,
            "gpu": info["gpu"], "source_skipped": skipped}


def _validate_sender_ready_source(ready, cfg, session, source):
    label = "TX ready"
    _require(isinstance(ready, dict), f"{label}: not a JSON object")
    _require(ready.get("ready") is True, f"{label}: ready is not true")
    dll._check_echo(ready, "role", "sender", label)
    smoke.require_session(ready.get("session_id"), session, label)
    smoke.require_adapter(ready.get("adapter"), label)
    dll._check_geometry_echo(ready, cfg, label)
    dll._check_echo(ready, "codec", "raw", label)
    _require(ready.get("source") == source["mode"],
             f"{label}: source {ready.get('source')!r} != {source['mode']!r}")
    _require(ready.get("pattern") is None,
             f"{label}: desktop source must not report a pattern name")
    _require(ready.get("batch") == 1,
             f"{label}: Loopback Raw must use batch 1, got {ready.get('batch')!r}")
    _require(ready.get("packets_per_frame") == cfg["total_packets"],
             f"{label}: packets_per_frame {ready.get('packets_per_frame')!r} != "
             f"{cfg['total_packets']}")
    _require(ready.get("fps_target") == cfg["fps"],
             f"{label}: fps_target {ready.get('fps_target')!r} != {cfg['fps']}")
    return _validate_source_info(ready.get("source_info"), cfg, source, label)


def _validate_sender_stats(tx_stats, cfg, session, source):
    smoke.validate_tx_stats(tx_stats, cfg["frames"], cfg["total_packets"],
                            session, "TX stats")
    _require(smoke.is_int(tx_stats.get("source_skipped"))
             and tx_stats["source_skipped"] >= 0,
             f"TX stats: source_skipped {tx_stats.get('source_skipped')!r}")
    actual = tx_stats.get("fps_actual")
    _require(_is_number(actual) and actual >= max(5.0, cfg["fps"] * 0.4),
             f"TX stats: measured fps_actual {actual!r} is below the liveness floor")
    return actual, _validate_source_info(tx_stats.get("source_info"), cfg, source,
                                         "TX stats")


def _nv12_stats(payload, width, height):
    y_bytes = width * height
    _require(len(payload) == y_bytes * 3 // 2,
             f"NV12 payload is {len(payload)} bytes, expected {y_bytes * 3 // 2}")
    y_plane = payload[:y_bytes]
    uv_plane = payload[y_bytes:]
    return {
        "y_mean": round(sum(y_plane) / len(y_plane), 3),
        "y_min": min(y_plane),
        "y_max": max(y_plane),
        "y_unique": len(set(y_plane)),
        "uv_unique": len(set(uv_plane)),
    }


def _validate_nv12_content(stats, label):
    _require(2.0 <= stats["y_mean"] <= 253.0,
             f"{label}: NV12 is blank (Y mean {stats['y_mean']})")
    _require(stats["y_unique"] >= 8,
             f"{label}: NV12 Y plane has only {stats['y_unique']} distinct values")
    _require(stats["uv_unique"] >= 4,
             f"{label}: NV12 UV plane has only {stats['uv_unique']} distinct values")


def _latest_payload(view, summary):
    header = struct.unpack(smoke.SHM_HDR_FMT, view.read(0, smoke.SHM_HDR_SIZE))
    data_off = header[4]
    frame_bytes = header[3]
    return view.read(data_off + summary["latest_slot"] * frame_bytes, frame_bytes)


def _validate_motion(tx_rows, frames, label):
    hashes = [tx_rows[fid]["hash"] for fid in range(frames)]
    unique = len(set(hashes))
    required = max(2, frames // 4)
    _require(unique >= required,
             f"{label}: only {unique} distinct source hashes across {frames} frames "
             f"(need >= {required}); capture did not see live content")
    _require(hashes[0] != hashes[-1],
             f"{label}: first and last source hashes are identical")
    return {"frames": frames, "unique_source_hashes": unique}


def _validate_stopped_sender_stats(stats, cfg, session, source, label, minimum):
    _require(isinstance(stats, dict), f"{label}: stats is not a JSON object")
    smoke.require_session(stats.get("session_id"), session, label)
    smoke.require_adapter(stats.get("adapter"), label)
    _require(stats.get("running") is False, f"{label}: running {stats.get('running')!r}")
    _require(stats.get("failed") is False, f"{label}: failed {stats.get('failed')!r}")
    _require(stats.get("exit_code") == 0, f"{label}: exit_code {stats.get('exit_code')!r}")
    _require(stats.get("cancelled") is True,
             f"{label}: cancelled {stats.get('cancelled')!r}; stop was not cooperative")
    _require(stats.get("send_errors") == 0,
             f"{label}: send_errors {stats.get('send_errors')!r}")
    sent = stats.get("sent_frames")
    frames = stats.get("frames")
    _require(smoke.is_int(sent) and sent >= minimum,
             f"{label}: sent_frames {sent!r} < {minimum}")
    _require(frames == sent, f"{label}: frames {frames!r} != sent_frames {sent!r}")
    actual = stats.get("fps_actual")
    _require(_is_number(actual) and actual > 0, f"{label}: fps_actual {actual!r}")
    source_info = _validate_source_info(stats.get("source_info"), cfg, source, label)
    return {"frames": sent, "fps_actual": actual, "source_info": source_info}


def _validate_stopped_receiver_stats(stats, cfg, session, label, minimum):
    _require(isinstance(stats, dict), f"{label}: stats is not a JSON object")
    smoke.require_session(stats.get("session_id"), session, label)
    smoke.require_adapter(stats.get("adapter"), label)
    _require(stats.get("running") is False, f"{label}: running {stats.get('running')!r}")
    _require(stats.get("failed") is False, f"{label}: failed {stats.get('failed')!r}")
    _require(stats.get("error") is None, f"{label}: error {stats.get('error')!r}")
    _require(stats.get("cancelled") is True,
             f"{label}: cancelled {stats.get('cancelled')!r}; stop was not cooperative")
    _require(stats.get("expect_frames") == 0,
             f"{label}: expect_frames {stats.get('expect_frames')!r} is not unlimited")
    _require(smoke.is_int(stats.get("timeout_s")) and stats["timeout_s"] > 0,
             f"{label}: timeout_s {stats.get('timeout_s')!r}")
    _require(smoke.is_int(stats.get("hash_mismatch")) and stats["hash_mismatch"] == 0,
             f"{label}: hash_mismatch {stats.get('hash_mismatch')!r}")
    complete = stats.get("complete")
    hash_rows = stats.get("hash_rows")
    _require(smoke.is_int(complete) and complete >= minimum,
             f"{label}: complete {complete!r} < {minimum}")
    _require(smoke.is_int(hash_rows) and hash_rows >= minimum,
             f"{label}: hash_rows {hash_rows!r} < {minimum}")
    _require(hash_rows == complete,
             f"{label}: hash_rows {hash_rows!r} != complete {complete!r}")
    return {"complete": complete, "hash_rows": hash_rows}


def _validate_unlimited_shm(view, shm_name, cfg, rx_rows, minimum_seq, label):
    """Validate the private SHM v3 header and latest published frame after stop."""
    header = struct.unpack(smoke.SHM_HDR_FMT, view.read(0, smoke.SHM_HDR_SIZE))
    (magic, version, slots, frame_bytes, data_off, latest_slot, latest_seq,
     writer_pid, width, height, fps) = header
    _require(magic == smoke.SHM_MAGIC, f"{label}: SHM magic 0x{magic:08x}")
    _require(version == smoke.SHM_VERSION, f"{label}: SHM version {version}")
    _require(slots == smoke.SHM_SLOTS, f"{label}: SHM slots {slots}")
    _require(data_off == smoke.SHM_DATA_OFF, f"{label}: SHM data_off {data_off}")
    _require(frame_bytes == cfg["frame_bytes"],
             f"{label}: SHM frame_bytes {frame_bytes} != {cfg['frame_bytes']}")
    _require((width, height, fps) == (cfg["width"], cfg["height"], cfg["fps"]),
             f"{label}: SHM geometry {width}x{height}@{fps} != "
             f"{cfg['width']}x{cfg['height']}@{cfg['fps']}")
    _require(writer_pid == os.getpid(),
             f"{label}: SHM writer_pid {writer_pid} != live host pid {os.getpid()}")
    _require(smoke.is_int(latest_seq) and latest_seq >= minimum_seq,
             f"{label}: SHM latest_seq {latest_seq!r} < {minimum_seq}")
    _require(0 <= latest_slot < smoke.SHM_SLOTS,
             f"{label}: SHM latest_slot {latest_slot} is outside 0..{smoke.SHM_SLOTS - 1}")
    slot_offset = dll.SHM_HEADER_BYTES + dll.SHM_SLOT_BYTES * latest_slot
    seq1, frame_id, qpc_first, qpc_pub, slot_hash, hash_fid = struct.unpack(
        smoke.SHM_SLOT_FMT, view.read(slot_offset, smoke.SHM_SLOT_SIZE))
    _require(seq1 % 2 == 0, f"{label}: latest SHM slot seq {seq1} is odd")
    _require(0 <= frame_id < latest_seq,
             f"{label}: latest SHM frame_id {frame_id} is outside 0..{latest_seq - 1}")
    _require(qpc_first > 0 and qpc_pub >= qpc_first,
             f"{label}: invalid SHM QPC fields first={qpc_first} pub={qpc_pub}")
    rows = {row["frame_id"]: row for row in rx_rows}
    _require(frame_id in rows,
             f"{label}: latest SHM frame {frame_id} is missing from the RX hashlog")
    payload = _latest_payload(view, {"latest_slot": latest_slot})
    actual = smoke.pattern_ref.fnv1a64_hex(payload)
    _require(actual == rows[frame_id]["hash"],
             f"{label}: SHM frame {frame_id} FNV {actual} != RX hash "
             f"{rows[frame_id]['hash']}")
    _require(hash_fid == frame_id,
             f"{label}: SHM hash_fid {hash_fid} != frame_id {frame_id}")
    _require(slot_hash == int(actual, 16),
             f"{label}: SHM slot.hash 0x{slot_hash:016x} != FNV 0x{int(actual, 16):016x}")
    return {
        "name": shm_name,
        "version": version,
        "geometry": [width, height, fps],
        "latest_slot": latest_slot,
        "latest_seq": latest_seq,
        "latest_frame_id": frame_id,
        "latest_hash": actual,
        "hash_marked": True,
    }


# ---------------------------------------------------------------------------
# Live combos


def _cleanup_entry(api, entry, label):
    return dll._stop_then_destroy(api, entry, label)


def _cooperative_stop_destroy(api, session_entry, session_id, label,
                              timeout=COOP_STOP_GRACE_S):
    """Stop and destroy one exact session inside one absolute native grace.

    The deadline is enforced even when the final stop/destroy calls return 0:
    a late successful return is still a failure of the 2 s cooperative gate.
    """
    handle = session_entry["handle"]
    started = time.monotonic()
    deadline = started + timeout
    stop_codes = []
    while True:
        code = api.stop(handle, f"{label} cooperative stop")
        stop_codes.append(code)
        if code == 0:
            break
        if time.monotonic() >= deadline:
            raise dll.PendingError(
                f"{label}: stop_session stayed pending beyond {timeout:.1f}s native "
                f"grace; codes={stop_codes[-8:]}")
        time.sleep(0.01)
    stopped_at = time.monotonic()
    status = dll._check_status(api.status(handle, f"{label} status after stop"),
                               session_id, label)
    loaded_after_stop = dll._module_loaded()
    _require(loaded_after_stop is True,
             f"{label}: vc_stream.dll was unloaded after stop before destroy")
    destroy_codes = []
    while True:
        code = api.destroy(handle, f"{label} destroy")
        destroy_codes.append(code)
        if code == 0:
            break
        if time.monotonic() >= deadline:
            raise dll.PendingError(
                f"{label}: destroy_session stayed pending beyond {timeout:.1f}s native "
                f"grace; codes={destroy_codes[-8:]}")
        time.sleep(0.01)
    destroyed_at = time.monotonic()
    # destroy_session returned 0, so the handle is invalid now.  Mark it before
    # any check that can raise, otherwise the caller would retry a dead handle.
    session_entry["destroyed"] = True
    loaded_after_destroy = dll._module_loaded()
    cleanup = {
        "stop_codes": stop_codes,
        "destroy_codes": destroy_codes,
        "native_stop_grace_s": timeout,
        "stop_elapsed_s": round(stopped_at - started, 6),
        "destroy_elapsed_s": round(destroyed_at - stopped_at, 6),
        "total_elapsed_s": round(destroyed_at - started, 6),
        "status_after_stop": status,
        "module_loaded_after_stop": loaded_after_stop,
        "module_loaded_after_destroy": loaded_after_destroy,
    }
    session_entry["cleanup"] = cleanup
    _require(loaded_after_destroy is True,
             f"{label}: vc_stream.dll was unloaded by destroy")
    total_elapsed = destroyed_at - started
    if total_elapsed > timeout:
        raise dll.DllError(
            f"{label}: stop+destroy completed after {total_elapsed:.3f}s, "
            f"exceeding the {timeout:.1f}s native stop grace; "
            f"stop_codes={stop_codes[-8:]}, destroy_codes={destroy_codes[-8:]}")
    return cleanup


def run_cooperative_stop(api, run_dir, token, name, source, width, height,
                         fixture_evidence=None, entries=None):
    """Unlimited TX/RX: advance real frames, then stop exact sessions within 2 s."""
    _require(source.get("gpu_convert") is True,
             f"{name}: desktop capture is GPU-only; gpu_convert=false is not allowed")
    cfg = _unlimited_config(width, height)
    tx_session = f"src-{token}-{name}-tx"
    rx_session = f"src-{token}-{name}-rx"
    shm_name = f"Local\\VirtualCaptureSourceSmoke.{token}.{name}"
    source_mac = dll._random_source_mac()
    tx_stats_path = run_dir / f"{name}-tx-stats.json"
    tx_hashlog_path = run_dir / f"{name}-tx-hash.jsonl"
    rx_stats_path = run_dir / f"{name}-rx-stats.json"
    rx_hashlog_path = run_dir / f"{name}-rx-hash.jsonl"

    tx_request = _sender_request(cfg, tx_session, source, source_mac,
                                 tx_stats_path, tx_hashlog_path)
    rx_request = _receiver_request(cfg, rx_session, source_mac, shm_name,
                                   rx_stats_path, rx_hashlog_path)
    entry = {
        "name": name,
        "kind": "cooperative-stop",
        "config": cfg,
        "source": source,
        "sessions_requested": {"sender": tx_session, "receiver": rx_session},
        "shm": shm_name,
        "requests": {"sender": tx_request, "receiver": rx_request},
        "evidence": {
            "tx_stats": str(tx_stats_path),
            "tx_hashlog": str(tx_hashlog_path),
            "rx_stats": str(rx_stats_path),
            "rx_hashlog": str(rx_hashlog_path),
        },
        "frames_floor": COOP_MIN_FRAMES,
        "native_stop_grace_s": COOP_STOP_GRACE_S,
        "sessions": {
            "sender": {"session_id": tx_session},
            "receiver": {"session_id": rx_session},
        },
    }
    if fixture_evidence is not None:
        entry["fixture_evidence"] = fixture_evidence
    if entries is not None:
        entries.append(entry)
    sessions = {}
    view = None
    try:
        rx_handle = api.start(dll.ROLE_RECEIVER, rx_request, f"{name} RX start")
        sessions["rx"] = {"handle": rx_handle, "destroyed": False}
        rx_ready = dll._wait_ready(api, rx_handle, rx_session, f"{name} RX",
                                   cfg["ready_timeout_s"])
        dll._validate_receiver_ready(rx_ready, cfg, rx_session, _codec_raw())
        entry["sessions"]["receiver"]["ready"] = rx_ready
        view = dll._open_shm_view(shm_name, f"{name} RX", cfg["ready_timeout_s"])

        tx_handle = api.start(dll.ROLE_SENDER, tx_request, f"{name} TX start")
        sessions["tx"] = {"handle": tx_handle, "destroyed": False}
        tx_ready = dll._wait_ready(api, tx_handle, tx_session, f"{name} TX",
                                   cfg["ready_timeout_s"])
        ready_source = _validate_sender_ready_source(tx_ready, cfg, tx_session, source)
        entry["sessions"]["sender"]["ready"] = tx_ready
        entry["sessions"]["sender"]["ready_source_info"] = ready_source

        tx_before = _wait_frames(api, tx_handle, tx_session, f"{name} TX",
                                 COOP_MIN_FRAMES, COOP_FRAME_TIMEOUT_S)
        rx_before = _wait_complete(api, rx_handle, rx_session, f"{name} RX",
                                   COOP_MIN_FRAMES, COOP_FRAME_TIMEOUT_S)
        entry["sessions"]["sender"]["before_stop"] = dll._brief(tx_before)
        entry["sessions"]["receiver"]["before_stop"] = dll._brief(rx_before)

        tx_cleanup = _cooperative_stop_destroy(
            api, sessions["tx"], tx_session, f"{name} TX")
        sessions["tx"]["cleanup"] = tx_cleanup
        entry["sessions"]["sender"]["cleanup"] = tx_cleanup
        entry["sessions"]["sender"]["final"] = tx_cleanup["status_after_stop"]

        rx_cleanup = _cooperative_stop_destroy(
            api, sessions["rx"], rx_session, f"{name} RX")
        sessions["rx"]["cleanup"] = rx_cleanup
        entry["sessions"]["receiver"]["cleanup"] = rx_cleanup
        entry["sessions"]["receiver"]["final"] = rx_cleanup["status_after_stop"]

        tx_status = tx_cleanup["status_after_stop"]
        _require(tx_status.get("running") is False,
                 f"{name} TX: running {tx_status.get('running')!r} after stop")
        _require(tx_status.get("cancelled") is True,
                 f"{name} TX: cancelled {tx_status.get('cancelled')!r} after stop")
        _require(tx_status.get("exit_code") == 0,
                 f"{name} TX: exit_code {tx_status.get('exit_code')!r} after stop")
        tx_frames_before = tx_before.get("frames")
        tx_frames_final = tx_status.get("frames")
        _require(smoke.is_int(tx_frames_final) and tx_frames_final >= COOP_MIN_FRAMES,
                 f"{name} TX: final frames {tx_frames_final!r} < {COOP_MIN_FRAMES}")
        _require(tx_frames_final >= tx_frames_before,
                 f"{name} TX: final frames {tx_frames_final!r} < before-stop "
                 f"{tx_frames_before!r}")

        rx_status = rx_cleanup["status_after_stop"]
        _require(rx_status.get("running") is False,
                 f"{name} RX: running {rx_status.get('running')!r} after stop")
        _require(rx_status.get("cancelled") is True,
                 f"{name} RX: cancelled {rx_status.get('cancelled')!r} after stop")
        _require(rx_status.get("exit_code") == 0,
                 f"{name} RX: exit_code {rx_status.get('exit_code')!r} after stop")
        rx_complete_final = rx_status.get("complete")
        _require(smoke.is_int(rx_complete_final) and rx_complete_final >= COOP_MIN_FRAMES,
                 f"{name} RX: complete {rx_complete_final!r} < {COOP_MIN_FRAMES}")

        tx_stats = smoke.read_json(tx_stats_path, f"{name} TX stats")
        rx_stats = smoke.read_json(rx_stats_path, f"{name} RX stats")
        entry["stats"] = {"tx": tx_stats, "rx": rx_stats}
        tx_final = _validate_stopped_sender_stats(
            tx_stats, cfg, tx_session, source, f"{name} TX stats", COOP_MIN_FRAMES)
        rx_final = _validate_stopped_receiver_stats(
            rx_stats, cfg, rx_session, f"{name} RX stats", COOP_MIN_FRAMES)
        _require(tx_final["frames"] == tx_frames_final,
                 f"{name} TX: stats frames {tx_final['frames']} != stopped status "
                 f"{tx_frames_final}")
        _require(rx_final["complete"] == rx_complete_final,
                 f"{name} RX: stats complete {rx_final['complete']} != stopped status "
                 f"{rx_complete_final}")

        tx_rows = _read_partial_rows(tx_hashlog_path, f"{name} TX hashlog")
        rx_rows = _read_partial_rows(rx_hashlog_path, f"{name} RX hashlog")
        _require(len(tx_rows) == tx_final["frames"],
                 f"{name} TX: {len(tx_rows)} hash rows != {tx_final['frames']} frames")
        _require(len(rx_rows) == rx_final["complete"],
                 f"{name} RX: {len(rx_rows)} hash rows != {rx_final['complete']} complete")
        _require(len(rx_rows) <= len(tx_rows),
                 f"{name}: RX has {len(rx_rows)} hash rows but TX sent {len(tx_rows)}")
        smoke.require_no_mismatch(
            {row["frame_id"]: row["hash"] for row in tx_rows},
            {row["frame_id"]: row["hash"] for row in rx_rows},
            f"{name} TX hashlog", f"{name} RX hashlog", "Raw source hash agreement")
        motion = _validate_motion(tx_rows, len(tx_rows), name)
        shm_summary = _validate_unlimited_shm(
            view, shm_name, cfg, rx_rows, COOP_MIN_FRAMES, f"{name} RX SHM")

        entry["result"] = {
            "status": "PASS",
            "mode": "unlimited-cooperative-stop",
            "frames": tx_frames_final,
            "fps_target": cfg["fps"],
            "fps_actual_tx": tx_final["fps_actual"],
            "frames_floor": COOP_MIN_FRAMES,
            "tx_frames_before_stop": tx_frames_before,
            "tx_frames_final": tx_frames_final,
            "rx_complete_before_stop": rx_before.get("complete"),
            "rx_complete_final": rx_complete_final,
            "native_stop_grace_s": COOP_STOP_GRACE_S,
            "tx_stop_elapsed_s": tx_cleanup["stop_elapsed_s"],
            "tx_total_elapsed_s": tx_cleanup["total_elapsed_s"],
            "rx_stop_elapsed_s": rx_cleanup["stop_elapsed_s"],
            "rx_total_elapsed_s": rx_cleanup["total_elapsed_s"],
            "module_loaded_after_stop": True,
            "module_loaded_after_destroy": True,
            "motion": motion,
            "shm": {key: shm_summary.get(key) for key in
                    ("version", "geometry", "latest_seq", "latest_frame_id",
                     "latest_hash", "hash_marked")},
            "source_info_ready": ready_source,
            "source_info_final": tx_final["source_info"],
        }
        return entry
    except Exception as exc:
        entry["status"] = "FAIL"
        entry["result"] = {**entry.get("result", {}), "status": "FAIL"}
        entry["error"] = {"type": type(exc).__name__, "message": str(exc)}
        entry["traceback"] = traceback.format_exc()
        raise
    finally:
        if view is not None:
            view.close()
        pending = []
        for role in ("tx", "rx"):
            session = sessions.get(role)
            if session is None or session.get("destroyed"):
                continue
            try:
                session["cleanup"] = _cleanup_entry(
                    api, session, f"{name} {role.upper()} cleanup")
            except Exception as exc:  # noqa: BLE001
                pending.append(f"{role.upper()}: {exc}")
                session["cleanup"] = {"error": str(exc)}
        entry["cleanup"] = {role: sessions.get(role, {}).get("cleanup")
                            for role in ("tx", "rx")}
        entry["cleanup_errors"] = pending
        if pending:
            partial = entries if entries is not None else [entry]
            dll._hard_exit_pending(run_dir, {"status": "FAIL", "combos": partial},
                                   "; ".join(pending))


def run_combo(api, run_dir, token, name, source, width, height, frames,
              resize=None, fixture_hwnd=None, fixture_evidence=None, entries=None):
    _require(source.get("gpu_convert") is True,
             f"{name}: desktop capture is GPU-only; gpu_convert=false is not allowed")
    cfg = smoke.build_config(frames, FPS, width, height, MTU, 1)
    tx_session = f"src-{token}-{name}-tx"
    rx_session = f"src-{token}-{name}-rx"
    shm_name = f"Local\\VirtualCaptureSourceSmoke.{token}.{name}"
    source_mac = dll._random_source_mac()
    tx_stats_path = run_dir / f"{name}-tx-stats.json"
    tx_hashlog_path = run_dir / f"{name}-tx-hash.jsonl"
    rx_stats_path = run_dir / f"{name}-rx-stats.json"
    rx_hashlog_path = run_dir / f"{name}-rx-hash.jsonl"

    tx_request = _sender_request(cfg, tx_session, source, source_mac,
                                 tx_stats_path, tx_hashlog_path)
    rx_request = _receiver_request(cfg, rx_session, source_mac, shm_name,
                                   rx_stats_path, rx_hashlog_path)
    entry = {
        "name": name,
        "config": cfg,
        "source": source,
        "requests": {"sender": tx_request, "receiver": rx_request},
        "sessions": {},
        "evidence": {
            "tx_stats": str(tx_stats_path),
            "tx_hashlog": str(tx_hashlog_path),
            "rx_stats": str(rx_stats_path),
            "rx_hashlog": str(rx_hashlog_path),
        },
    }
    if fixture_evidence is not None:
        entry["fixture_evidence"] = fixture_evidence
    # Publish the partial entry before any DLL/native work so a failed run is
    # still present in failure.json together with its staged status/cleanup.
    if entries is not None:
        entries.append(entry)
    sessions = {}
    view = None
    try:
        rx_handle = api.start(dll.ROLE_RECEIVER, rx_request, f"{name} RX start")
        sessions["rx"] = {"handle": rx_handle, "destroyed": False}
        rx_ready = dll._wait_ready(api, rx_handle, rx_session, f"{name} RX",
                                   cfg["ready_timeout_s"])
        dll._validate_receiver_ready(rx_ready, cfg, rx_session, _codec_raw())
        entry["sessions"]["receiver"] = {"ready": rx_ready}
        view = dll._open_shm_view(shm_name, f"{name} RX", cfg["ready_timeout_s"])

        tx_handle = api.start(dll.ROLE_SENDER, tx_request, f"{name} TX start")
        sessions["tx"] = {"handle": tx_handle, "destroyed": False}
        tx_ready = dll._wait_ready(api, tx_handle, tx_session, f"{name} TX",
                                   cfg["ready_timeout_s"])
        ready_source = _validate_sender_ready_source(tx_ready, cfg, tx_session, source)
        entry["sessions"]["sender"] = {"ready": tx_ready,
                                       "ready_source_info": ready_source}

        resize_status = None
        if resize is not None:
            _require(fixture_hwnd is not None, f"{name}: resize needs a fixture window")
            resize_status = _wait_frames(api, tx_handle, tx_session, f"{name} TX",
                                         resize["after_frames"], 12.0)
            _require(_resize_window(fixture_hwnd, resize["width"], resize["height"]),
                     f"{name}: SetWindowPos failed, win32 error "
                     f"{ctypes.get_last_error()}")
            entry["resize"] = {
                "requested": [resize["width"], resize["height"]],
                "frames_before_resize": resize_status.get("frames"),
                "applied": True,
            }

        tx_final = _wait_done(api, tx_handle, tx_session, f"{name} TX",
                              time.monotonic() + cfg["stream_timeout_s"])
        rx_final = _wait_done(api, rx_handle, rx_session, f"{name} RX",
                              time.monotonic() + cfg["stream_timeout_s"])
        entry["sessions"]["sender"]["final"] = tx_final
        entry["sessions"]["receiver"]["final"] = rx_final

        tx_stats = smoke.read_json(tx_stats_path, f"{name} TX stats")
        rx_stats = smoke.read_json(rx_stats_path, f"{name} RX stats")
        entry["stats"] = {"tx": tx_stats, "rx": rx_stats}
        fps_actual, final_source = _validate_sender_stats(tx_stats, cfg, tx_session, source)
        smoke.validate_rx_stats(rx_stats, frames, rx_session, True,
                                f"{name} RX stats")
        for role, stats in (("TX", tx_stats), ("RX", rx_stats)):
            smoke.require_adapter(stats.get("adapter"), f"{name} {role} stats")
            dll._check_geometry_echo(stats, cfg, f"{name} {role} stats")
            dll._check_echo(stats, "codec", "raw", f"{name} {role} stats")
            dll._check_echo(stats, "ethertype", "0x88b5", f"{name} {role} stats")
        rx_fps = rx_stats.get("fps_actual")
        _require(_is_number(rx_fps) and rx_fps > 0,
                 f"{name} RX stats: fps_actual {rx_fps!r}")

        tx_rows = smoke.parse_hash_rows(tx_hashlog_path, frames, f"{name} TX hashlog")
        rx_rows = smoke.parse_hash_rows(rx_hashlog_path, frames, f"{name} RX hashlog")
        dll._validate_raw_rows(tx_rows, rx_rows, frames)
        smoke.require_no_mismatch(
            {fid: row["hash"] for fid, row in tx_rows.items()},
            {fid: row["hash"] for fid, row in rx_rows.items()},
            f"{name} TX hashlog", f"{name} RX hashlog", "Raw source hash agreement")
        motion = _validate_motion(tx_rows, frames, name)

        shm_summary = dll._validate_shm(view, shm_name, cfg, frames, rx_rows,
                                        "hash", f"{name} RX SHM")
        nv12 = _nv12_stats(_latest_payload(view, shm_summary), width, height)
        _validate_nv12_content(nv12, name)

        if resize is not None:
            _require(final_source["source_width"] != ready_source["source_width"]
                     or final_source["source_height"] != ready_source["source_height"],
                     f"{name}: WGC source dimensions did not change after resize "
                     f"(before {ready_source['source_width']}x"
                     f"{ready_source['source_height']}, after "
                     f"{final_source['source_width']}x{final_source['source_height']})")
            _require(final_source["source_width"] >= width
                     and final_source["source_height"] >= height,
                     f"{name}: resized source is smaller than the crop output")
            entry["resize"].update({
                "source_before": [ready_source["source_width"],
                                  ready_source["source_height"]],
                "source_after": [final_source["source_width"],
                                 final_source["source_height"]],
            })

        entry["result"] = {
            "status": "PASS",
            "frames": frames,
            "fps_target": FPS,
            "fps_actual_tx": fps_actual,
            "fps_actual_rx": rx_fps,
            "tx_hash_rows": len(tx_rows),
            "rx_hash_rows": len(rx_rows),
            "tx_rx_hashes_agree": True,
            "motion": motion,
            "nv12": nv12,
            "shm": {key: shm_summary.get(key) for key in
                    ("version", "geometry", "latest_frame_id", "latest_seq",
                     "latest_hash", "hash_marked")},
            "source_info_ready": ready_source,
            "source_info_final": final_source,
        }
        if resize is not None:
            entry["result"]["resize"] = entry["resize"]

        entry["sessions"]["sender"]["cleanup"] = _cleanup_entry(
            api, sessions["tx"], f"{name} TX")
        entry["sessions"]["receiver"]["cleanup"] = _cleanup_entry(
            api, sessions["rx"], f"{name} RX")
        sessions["tx"]["cleanup"] = entry["sessions"]["sender"]["cleanup"]
        sessions["rx"]["cleanup"] = entry["sessions"]["receiver"]["cleanup"]
        entry["module_loaded_after_cleanup"] = dll._module_loaded()
        _require(entry["module_loaded_after_cleanup"] is not False,
                 f"{name}: vc_stream.dll was unloaded before session cleanup")
        return entry
    except Exception as exc:
        entry["status"] = "FAIL"
        entry["result"] = {**entry.get("result", {}), "status": "FAIL"}
        entry["error"] = {"type": type(exc).__name__, "message": str(exc)}
        entry["traceback"] = traceback.format_exc()
        raise
    finally:
        if view is not None:
            view.close()
        pending = []
        for role in ("tx", "rx"):
            session = sessions.get(role)
            if session is None or session.get("destroyed"):
                continue
            try:
                session["cleanup"] = _cleanup_entry(
                    api, session, f"{name} {role.upper()} cleanup")
            except Exception as exc:  # noqa: BLE001
                pending.append(f"{role.upper()}: {exc}")
                session["cleanup"] = {"error": str(exc)}
        entry["cleanup"] = {role: sessions.get(role, {}).get("cleanup")
                            for role in ("tx", "rx")}
        entry["cleanup_errors"] = pending
        if pending:
            partial = entries if entries is not None else [entry]
            dll._hard_exit_pending(run_dir, {"status": "FAIL", "combos": partial},
                                   "; ".join(pending))


def _read_partial_rows(path, label):
    _require(Path(path).is_file(), f"{label}: missing {path}")
    rows = []
    for line_number, line in enumerate(Path(path).read_text(
            encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as exc:
            raise smoke.SmokeError(
                f"{label}: line {line_number} is not JSON: {exc}") from exc
        _require(isinstance(row, dict), f"{label}: line {line_number} is not an object")
        fid = row.get("frame_id")
        _require(smoke.is_int(fid) and fid == len(rows),
                 f"{label}: line {line_number} frame_id {fid!r} is not {len(rows)}")
        digest = row.get("hash")
        _require(isinstance(digest, str) and len(digest) == 16,
                 f"{label}: line {line_number} hash {digest!r}")
        rows.append(row)
    return rows


def run_close_lifecycle(api, run_dir, token, source, fixture, entries=None):
    _require(source.get("gpu_convert") is True,
             "wgc-close: desktop capture is GPU-only; gpu_convert=false is not allowed")
    name = "wgc-close"
    close_frame_bytes = WGC_WIDTH * WGC_HEIGHT * 3 // 2
    close_payload = MTU - smoke.PACKET_OVERHEAD
    cfg = {
        "frames": 0,
        "fps": FPS,
        "width": WGC_WIDTH,
        "height": WGC_HEIGHT,
        "mtu": MTU,
        "frame_bytes": close_frame_bytes,
        "total_packets": (close_frame_bytes + close_payload - 1) // close_payload,
        "ready_timeout_s": 15.0,
        "stream_timeout_s": 30.0,
    }
    session = f"src-{token}-{name}-tx"
    source_mac = dll._random_source_mac()
    stats_path = run_dir / f"{name}-tx-stats.json"
    hashlog_path = run_dir / f"{name}-tx-hash.jsonl"
    request = _sender_request(cfg, session, source, source_mac, stats_path, hashlog_path)
    entry = {"name": name, "config": cfg, "source": source, "request": request,
             "evidence": {"tx_stats": str(stats_path),
                          "tx_hashlog": str(hashlog_path)}}
    if entries is not None:
        entries.append(entry)
    handle = None
    session_entry = None
    try:
        handle = api.start(dll.ROLE_SENDER, request, f"{name} TX start")
        session_entry = {"handle": handle, "destroyed": False}
        ready = dll._wait_ready(api, handle, session, f"{name} TX",
                                cfg["ready_timeout_s"])
        ready_source = _validate_sender_ready_source(ready, cfg, session, source)
        before = _wait_frames(api, handle, session, f"{name} TX",
                              CLOSE_MIN_FRAMES, 10.0)
        frames_before = before.get("frames")
        entry["frames_before_close"] = frames_before
        _require(_close_window(fixture[1]),
                 f"{name}: PostMessage(WM_CLOSE) failed, win32 error "
                 f"{ctypes.get_last_error()}")
        final = _wait_stopped(api, handle, session, f"{name} TX", 15.0)
        entry["final"] = final
        _require(final.get("exit_code") == 1,
                 f"{name}: closing the window ended with exit_code "
                 f"{final.get('exit_code')!r}, expected 1")
        error = str(final.get("error") or "")
        _require("closed" in error.lower(),
                 f"{name}: close error {error!r} does not report a closed capture item")
        _require(smoke.is_int(final.get("frames"))
                 and final["frames"] >= CLOSE_MIN_FRAMES,
                 f"{name}: final frames {final.get('frames')!r} < {CLOSE_MIN_FRAMES}")
        rows = _read_partial_rows(hashlog_path, f"{name} TX hashlog")
        _require(len(rows) >= CLOSE_MIN_FRAMES,
                 f"{name}: only {len(rows)} hashlog rows before close")
        entry["result"] = {
            "status": "PASS",
            "frames_before_close": frames_before,
            "final_frames": final.get("frames"),
            "exit_code": 1,
            "error": error,
            "hash_rows": len(rows),
            "source_info_ready": ready_source,
        }
        entry["cleanup"] = _cleanup_entry(api, session_entry, f"{name} TX")
        session_entry["cleanup"] = entry["cleanup"]
        entry["module_loaded_after_cleanup"] = dll._module_loaded()
        _require(entry["module_loaded_after_cleanup"] is not False,
                 f"{name}: vc_stream.dll was unloaded before session cleanup")
        return entry
    except Exception as exc:
        entry["status"] = "FAIL"
        entry["result"] = {**entry.get("result", {}), "status": "FAIL"}
        entry["error"] = {"type": type(exc).__name__, "message": str(exc)}
        entry["traceback"] = traceback.format_exc()
        raise
    finally:
        if session_entry is not None and not session_entry.get("destroyed"):
            try:
                session_entry["cleanup"] = _cleanup_entry(
                    api, session_entry, f"{name} TX cleanup")
            except Exception as exc:  # noqa: BLE001
                session_entry["cleanup"] = {"error": str(exc)}
                entry["cleanup_error"] = str(exc)
        if "cleanup" not in entry:
            entry["cleanup"] = session_entry.get("cleanup") if session_entry else None


def _dda_unavailable(error):
    """Only treat the host's missing-DXGI-output condition as an environment block."""
    text = str(error).lower()
    return ("dda output" in text or "dda" in text) and (
        "not available" in text or "no outputs" in text or "headless" in text
        or "0x887a0002" in text)


def run_combos(api, run_dir, token, allow_dda_unavailable=False, combos=None):
    if combos is None:
        combos = []
    fixture_process = None
    fixture_hwnd = None
    fixture_title = FIXTURE_TITLE_PREFIX + token
    fixture_evidence = None
    dda_blocked = None

    def mark_dda_blocked(name, source, error, reason):
        nonlocal dda_blocked
        dda_blocked = str(error)
        record = {
            "name": name,
            "status": "BLOCKED",
            "reason": reason,
            "source": source,
            "dda_error": dda_blocked,
        }
        if combos and combos[-1].get("name") == name \
                and combos[-1].get("status") == "FAIL":
            # Keep the partial entry (sessions/cleanup/error) but stop reporting
            # an expected missing-output host as an unhandled smoke failure.
            combos[-1].update(record)
            combos[-1]["result"] = {"status": "BLOCKED"}
        else:
            combos.append(record)

    try:
        fixture_process, fixture_hwnd = _start_fixture(
            fixture_title, RESIZE_TO[0] - 160, 360)

        def raise_fixture():
            # Re-assert only the private fixture before each DDA capture: it may
            # otherwise sit behind another window and produce no new content.
            # This is recorded evidence, not an assertion about an earlier failure.
            return {"topmost": _set_fixture_topmost(fixture_hwnd, True)}

        dda_specs = (
            ("dda-gpu-stretch", _source_config("dda", "", "stretch", True),
             DDA_WIDTH, DDA_HEIGHT, DDA_FRAMES),
            ("dda-gpu-fill", _source_config("dda", "", "fill", True),
             DDA_WIDTH, WGC_HEIGHT, DDA_FRAMES),
        )
        for name, source, width, height, frames in dda_specs:
            if dda_blocked is not None:
                combos.append({
                    "name": name,
                    "status": "BLOCKED",
                    "reason": "skipped because the host exposes no DXGI output",
                    "source": source,
                    "dda_error": dda_blocked,
                })
                continue
            fixture_evidence = raise_fixture()
            try:
                run_combo(api, run_dir, token, name, source, width, height,
                          frames, fixture_evidence=fixture_evidence,
                          entries=combos)
            except dll.DllError as exc:
                if not (allow_dda_unavailable and _dda_unavailable(exc)):
                    raise
                mark_dda_blocked(
                    name, source, exc,
                    "host has no DXGI output for DDA; the requested source "
                    "was not silently replaced")

        dda_stop_source = _source_config("dda", "", "stretch", True)
        if dda_blocked is not None:
            combos.append({
                "name": "dda-gpu-stop",
                "status": "BLOCKED",
                "reason": "skipped because the host exposes no DXGI output",
                "source": dda_stop_source,
                "dda_error": dda_blocked,
            })
        else:
            fixture_evidence = raise_fixture()
            try:
                run_cooperative_stop(
                    api, run_dir, token, "dda-gpu-stop", dda_stop_source,
                    DDA_WIDTH, DDA_HEIGHT, fixture_evidence=fixture_evidence,
                    entries=combos)
            except dll.DllError as exc:
                if not (allow_dda_unavailable and _dda_unavailable(exc)):
                    raise
                mark_dda_blocked(
                    "dda-gpu-stop", dda_stop_source, exc,
                    "host has no DXGI output for DDA; the requested source "
                    "was not silently replaced")

        restore = _set_fixture_topmost(fixture_hwnd, False)
        fixture_evidence = {**(fixture_evidence or {}), "restore": restore}

        run_combo(
            api, run_dir, token, "wgc-gpu-crop-resize",
            _source_config("wgc", fixture_title, "crop", True),
            WGC_WIDTH, WGC_HEIGHT, RESIZE_FRAMES,
            resize={"after_frames": RESIZE_AFTER_FRAMES,
                    "width": RESIZE_TO[0], "height": RESIZE_TO[1]},
            fixture_hwnd=fixture_hwnd, entries=combos)
        run_combo(
            api, run_dir, token, "wgc-gpu-stretch",
            _source_config("wgc", fixture_title, "stretch", True),
            WGC_WIDTH, WGC_HEIGHT, WGC_FRAMES, entries=combos)
        run_cooperative_stop(
            api, run_dir, token, "wgc-gpu-stop",
            _source_config("wgc", fixture_title, "crop", True),
            WGC_WIDTH, WGC_HEIGHT, entries=combos)
        run_close_lifecycle(
            api, run_dir, token,
            _source_config("wgc", fixture_title, "crop", True),
            (fixture_process, fixture_hwnd), entries=combos)
        return combos
    finally:
        cleanup = None
        try:
            cleanup = _stop_process(fixture_process, fixture_hwnd)
        except Exception as exc:  # noqa: BLE001 - fixture teardown must be recorded
            cleanup = {"error": str(exc)}
        finally:
            _terminate_silently(fixture_process)
        combos.append({"fixture_cleanup": cleanup,
                       "fixture_evidence": fixture_evidence})


# ---------------------------------------------------------------------------
# Evidence and self-check


def _make_run_dir():
    base = ROOT / "run"
    base.mkdir(parents=True, exist_ok=True)
    for _ in range(100):
        token = uuid.uuid4().hex[:12]
        path = base / f"{RUN_PREFIX}{token}"
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


def self_check():
    source = _source_config("wgc", "Self Check Window", "crop", True)
    cfg = smoke.build_config(10, FPS, WGC_WIDTH, WGC_HEIGHT, MTU, 1)
    mac = dll._random_source_mac()
    request = _sender_request(cfg, "self-tx", source, mac,
                              ROOT / "self-stats.json", ROOT / "self-hash.jsonl")
    _require(request["source"] == source, "self-check: source object changed")
    _require(set(request["source"]) == {"mode", "pattern", "output", "window",
                                        "transform", "gpu_convert"},
             "self-check: source config keys")
    _require(source["gpu_convert"] is True,
             "self-check: desktop capture must be GPU-only")
    _require(request["adapter"] == smoke.LOOPBACK, "self-check: adapter")
    _require(_validate_nv12_stats_selfcheck(), "self-check: NV12 statistics")
    _require(_dda_unavailable(dll.DllError(
                 "DDA output is not available: no outputs (0x887A0002)")),
             "self-check: missing-DXGI-output classification")
    _require(not _dda_unavailable(dll.DllError("WGC capture item closed")),
             "self-check: unrelated failures must not be treated as DDA blocked")

    stop_cfg = _unlimited_config(DDA_WIDTH, DDA_HEIGHT)
    stop_payload = MTU - smoke.PACKET_OVERHEAD
    _require(stop_cfg["frames"] == 0, "self-check: cooperative stop config is limited")
    _require(stop_cfg["frame_bytes"] == DDA_WIDTH * DDA_HEIGHT * 3 // 2,
             "self-check: cooperative stop frame_bytes")
    _require(stop_cfg["total_packets"] ==
             (stop_cfg["frame_bytes"] + stop_payload - 1) // stop_payload,
             "self-check: cooperative stop packet count")
    _require(stop_cfg["receiver_timeout_s"] > COOP_FRAME_TIMEOUT_S,
             "self-check: receiver timeout must outlive the frame-advance window")
    _require(COOP_MIN_FRAMES >= 1 and COOP_STOP_GRACE_S == 2.0
             and COOP_FRAME_TIMEOUT_S > COOP_STOP_GRACE_S,
             "self-check: cooperative stop constants")

    stop_source = _source_config("wgc", "", "stretch", True)
    stop_stats = {
        "session_id": "self-stop-tx",
        "adapter": smoke.LOOPBACK,
        "running": False,
        "failed": False,
        "exit_code": 0,
        "cancelled": True,
        "send_errors": 0,
        "frames": COOP_MIN_FRAMES,
        "sent_frames": COOP_MIN_FRAMES,
        "fps_actual": 30.0,
        "source_info": {
            "mode": "wgc",
            "window": "",
            "transform": "stretch",
            "output": 0,
            "gpu_convert": True,
            "gpu": "Self Check GPU",
            "source_width": WGC_WIDTH,
            "source_height": WGC_HEIGHT,
            "output_width": stop_cfg["width"],
            "output_height": stop_cfg["height"],
            "frame_bytes": stop_cfg["frame_bytes"],
            "source_skipped": 0,
        },
    }
    stopped_tx = _validate_stopped_sender_stats(
        stop_stats, stop_cfg, "self-stop-tx", stop_source,
        "self-check stop TX", COOP_MIN_FRAMES)
    _require(stopped_tx["frames"] == COOP_MIN_FRAMES,
             "self-check: stopped sender frame count")
    try:
        _validate_stopped_sender_stats(
            {**stop_stats, "cancelled": False}, stop_cfg, "self-stop-tx",
            stop_source, "self-check non-cooperative TX", COOP_MIN_FRAMES)
    except smoke.SmokeError:
        pass
    else:
        raise smoke.SmokeError(
            "self-check: stopped sender validator accepted cancelled=false")

    stop_rx = {
        "session_id": "self-stop-rx",
        "adapter": smoke.LOOPBACK,
        "running": False,
        "failed": False,
        "error": None,
        "cancelled": True,
        "expect_frames": 0,
        "timeout_s": stop_cfg["receiver_timeout_s"],
        "hash_mismatch": 0,
        "complete": COOP_MIN_FRAMES,
        "hash_rows": COOP_MIN_FRAMES,
    }
    stopped_rx = _validate_stopped_receiver_stats(
        stop_rx, stop_cfg, "self-stop-rx", "self-check stop RX", COOP_MIN_FRAMES)
    _require(stopped_rx["complete"] == COOP_MIN_FRAMES,
             "self-check: stopped receiver frame count")
    _require(_validate_cooperative_late_stop_selfcheck(),
             "self-check: cooperative late stop")

    return {"source_keys": sorted(source), "frames": cfg["frames"],
            "fps": cfg["fps"], "geometry": [cfg["width"], cfg["height"]],
            "gpu_convert": source["gpu_convert"],
            "coop_frames_floor": COOP_MIN_FRAMES,
            "coop_stop_grace_s": COOP_STOP_GRACE_S,
            "coop_late_stop_checked": True}


def _validate_nv12_stats_selfcheck():
    width, height = 4, 2
    payload = bytes([16, 32, 64, 128, 160, 200, 220, 240]) + bytes(
        [128, 130, 132, 134])
    stats = _nv12_stats(payload, width, height)
    _validate_nv12_content(stats, "self-check")
    _require(stats["y_unique"] == 8 and stats["uv_unique"] == 4,
             "self-check: NV12 distinct counts")
    return True


def _validate_cooperative_late_stop_selfcheck():
    """Exercise the real stop helper: rc=0 after the grace must still fail."""
    original_monotonic = time.monotonic
    original_module_loaded = dll._module_loaded
    status = {
        "session_id": "self-coop-late",
        "pid": os.getpid(),
        "module": dll.DLL_NAME,
        "abi_version": dll.ABI_VERSION,
        "running": False,
        "cancelled": True,
        "exit_code": 0,
        "frames": COOP_MIN_FRAMES,
    }

    class FakeApi:
        def stop(self, handle, label):
            return 0

        def destroy(self, handle, label):
            return 0

        def status(self, handle, label):
            return dict(status)

    clock = [0.0]

    def fake_monotonic():
        value = clock[0]
        clock[0] += 1.25
        return value

    session_entry = {"handle": 0xC0FFEE}
    time.monotonic = fake_monotonic
    dll._module_loaded = lambda: True
    try:
        try:
            _cooperative_stop_destroy(FakeApi(), session_entry, "self-coop-late",
                                      "self-check late stop")
        except dll.DllError as exc:
            message = str(exc)
        else:
            raise smoke.SmokeError(
                "self-check: late rc=0 stop/destroy was accepted")
        _require("native stop grace" in message,
                 f"self-check: late stop error {message!r}")
        _require(session_entry.get("destroyed") is True,
                 "self-check: late destroy did not mark the session destroyed")
        cleanup = session_entry.get("cleanup")
        _require(isinstance(cleanup, dict),
                 "self-check: late stop did not preserve cleanup evidence")
        _require(cleanup.get("stop_codes") == [0]
                 and cleanup.get("destroy_codes") == [0],
                 "self-check: late stop codes")
        _require(cleanup.get("total_elapsed_s") == 2.5,
                 f"self-check: late stop total {cleanup.get('total_elapsed_s')!r}")
        _require(cleanup.get("module_loaded_after_destroy") is True,
                 "self-check: late stop module evidence")
    finally:
        time.monotonic = original_monotonic
        dll._module_loaded = original_module_loaded
    return True


def parse_args(argv=None):
    parser = argparse.ArgumentParser(
        description="Real DDA/WGC capture smoke through the release vc_stream.dll ABI.")
    parser.add_argument("--dll", type=Path, default=DEFAULT_DLL,
                        help=f"release vc_stream.dll (default {DEFAULT_DLL})")
    parser.add_argument("--window-fixture", action="store_true",
                        help=argparse.SUPPRESS)
    parser.add_argument("--title", default=FIXTURE_TITLE_PREFIX + "fixture",
                        help=argparse.SUPPRESS)
    parser.add_argument("--width", type=int, default=480, help=argparse.SUPPRESS)
    parser.add_argument("--height", type=int, default=360, help=argparse.SUPPRESS)
    parser.add_argument("--self-check", action="store_true",
                        help="check pure helpers only; never loads the DLL")
    parser.add_argument("--allow-dda-unavailable", action="store_true",
                        help="record a host with no DXGI output as DDA BLOCKED and "
                             "continue the WGC window runs; never substitutes a backend")
    return parser.parse_args(argv)


def execute(args, source_dll, run_dir, token, report):
    # Publish the empty list before any native work so failure.json always has a
    # combos field, even when staging or capability checks fail.
    report["combos"] = []
    stage, staged_dll, copied = _stage_module(source_dll, token)
    report["staging"] = {"source": str(Path(source_dll).resolve()),
                         "staged_dir": str(stage),
                         "files": copied}
    report["dll"] = {"path": str(staged_dll),
                     "size": staged_dll.stat().st_size,
                     "sha256": _sha256(staged_dll)}
    report["environment"] = {
        "platform": sys.platform,
        "python": sys.version,
        "executable": sys.executable,
        "isolated": bool(sys.flags.isolated),
        "pid": os.getpid(),
        "cwd": str(Path.cwd()),
    }
    api = dll._load_api(staged_dll, report)
    capabilities, capabilities_text = api.capabilities()
    _require(isinstance(capabilities, dict), "capabilities: not an object")
    for mode in ("dda", "wgc"):
        _require(mode in capabilities.get("sources", []),
                 f"capabilities do not list source {mode!r}: "
                 f"{capabilities.get('sources')!r}")
    _require("raw" in capabilities.get("codecs", []),
             f"capabilities do not list raw: {capabilities.get('codecs')!r}")
    _require(capabilities.get("shm_version") == 3, "capabilities: SHM must be v3")
    _require(capabilities.get("pixel_format") == "nv12", "capabilities: NV12 required")
    report["capabilities"] = capabilities
    report["capabilities_text"] = capabilities_text

    adapters, adapters_text = api.list_adapters()
    names = [entry[0] for entry in adapters
             if isinstance(entry, (list, tuple)) and entry]
    _require(smoke.LOOPBACK in names,
             f"Npcap Loopback adapter is missing: {names!r}")
    report["adapters"] = names
    report["adapters_text"] = adapters_text

    report["scope"] = ("same-process vc_stream.dll ABI v1, same-host Npcap Loopback, "
                       "GPU-only desktop capture, private window and SHM; not a 240 fps, "
                       "physical-link, two-machine or capacity acceptance")
    run_combos(api, run_dir, token,
               allow_dda_unavailable=args.allow_dda_unavailable,
               combos=report["combos"])
    blocked = [combo for combo in report["combos"]
               if combo.get("status") == "BLOCKED"]
    if blocked:
        report["blocked"] = [{
            "name": combo.get("name"),
            "reason": combo.get("reason"),
            "dda_error": combo.get("dda_error"),
        } for combo in blocked]


def main(argv=None):
    args = parse_args(argv)
    if args.window_fixture:
        try:
            return _fixture_main(args)
        except Exception:  # noqa: BLE001 - child prints its failure for the parent
            traceback.print_exc()
            return 1
    if args.self_check:
        try:
            _print_record({"self_check": "PASS", **self_check()})
            return 0
        except Exception as exc:  # noqa: BLE001
            _print_record({"self_check": "FAIL", "error": str(exc),
                           "traceback": traceback.format_exc()})
            return 1

    try:
        run_dir, token = _make_run_dir()
    except Exception as exc:  # noqa: BLE001
        _print_record({"status": "FAIL", "phase": "evidence", "error": str(exc)})
        return 2
    report = {
        "status": "RUNNING",
        "run_dir": str(run_dir),
        "token": token,
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    try:
        execute(args, args.dll, run_dir, token, report)
    except Exception as exc:  # noqa: BLE001 - preserve evidence and report failure
        report["status"] = "FAIL"
        report["failure"] = {"error": str(exc), "type": type(exc).__name__,
                             "traceback": traceback.format_exc()}
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
        })
        return 1

    report["status"] = "PARTIAL" if report.get("blocked") else "PASS"
    report["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    _write_json_exclusive(run_dir / "report.json", report)
    _print_record({
        "status": report["status"],
        "run_dir": str(run_dir),
        "run_json": str(run_dir / "report.json"),
        "dll_sha256": report.get("dll", {}).get("sha256"),
        "blocked": report.get("blocked"),
        "combos": [{
            "name": combo.get("name"),
            "status": combo.get("result", {}).get("status", combo.get("status")),
            "fps_target": combo.get("result", {}).get("fps_target"),
            "fps_actual_tx": combo.get("result", {}).get("fps_actual_tx"),
            "fps_actual_rx": combo.get("result", {}).get("fps_actual_rx"),
            "frames": combo.get("result", {}).get("frames"),
        } for combo in report["combos"] if "result" in combo or "status" in combo],
    })
    return 0


if __name__ == "__main__":
    sys.exit(main())
