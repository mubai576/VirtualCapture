"""Rust REST sender → in-process vc_stream.dll Raw motion Loopback smoke, private runtime and SHM.
Run from repo root: python -I rust/tests/control_smoke.py
Does not register DShow or contact the deployed agent. Keeps all run evidence.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--package", type=Path, help="Exercise an extracted portable package instead of debug outputs")
args = parser.parse_args()
RUN = ROOT / "run" / ("rust-smoke-" + uuid.uuid4().hex[:10])
RUN.mkdir(parents=True)
NATIVE = args.package.resolve() if args.package else ROOT / "rust" / "target" / "debug"
CAPTURE = NATIVE / "vc-capture.exe"
STREAM_DLL = NATIVE / "vc_stream.dll"
SID = "s-" + uuid.uuid4().hex
MAC = "02:14:11:26:00:01"
MAC_BYTES = [int(part, 16) for part in MAC.split(":")]
SHM = "Local\\VirtualCaptureRustSmoke." + uuid.uuid4().hex
LOOPBACK = "\\Device\\NPF_Loopback"
WIDTH = HEIGHT = 416
FPS = 60
FRAMES = 120
MTU = 9000
ABI_VERSION = 1
ROLE_RECEIVER = 2
RAW_CODEC = {
    "codec": "raw", "experimental": False, "encoder_backend": "", "decoder_backend": "",
    "jpeg_quality": 80, "max_au_bytes": 8_388_608, "max_bitrate_bps": 80_000_000,
    "decode_in_flight": 2,
}
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    PORT = sock.getsockname()[1]
URL = f"http://127.0.0.1:{PORT}"


def request(path, body=None, expected=200):
    payload = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(URL + path, data=payload, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=40) as reply:
            code, data = reply.status, json.load(reply)
    except urllib.error.HTTPError as error:
        code = error.code
        text = error.read().decode()
        try:
            data = json.loads(text)
        except ValueError:
            data = {"error": text}
    assert code == expected, (path, code, data)
    return data


def stop(process):
    if process and process.poll() is None:
        try:
            process.send_signal(signal.CTRL_BREAK_EVENT)
            process.wait(timeout=5)
        except (OSError, subprocess.TimeoutExpired):
            process.kill()
            process.wait(timeout=5)


def hash_rows(path):
    return {row["frame_id"]: row["hash"] for row in (json.loads(line) for line in path.read_text().splitlines() if line.strip())}


# ---- 进程内 vc_stream.dll 接收端：真实 ABI v1，不启动旧接收器 EXE ----

TEXT_QUERY = ctypes.CFUNCTYPE(
    ctypes.c_int32, ctypes.POINTER(ctypes.c_char), ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint32),
    ctypes.POINTER(ctypes.c_char), ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32))
START_SESSION = ctypes.CFUNCTYPE(
    ctypes.c_int32, ctypes.c_uint32, ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p),
    ctypes.POINTER(ctypes.c_char), ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32))
SESSION_STATUS = ctypes.CFUNCTYPE(
    ctypes.c_int32, ctypes.c_void_p, ctypes.POINTER(ctypes.c_char), ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint32), ctypes.POINTER(ctypes.c_char), ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint32))
STOP_SESSION = ctypes.CFUNCTYPE(
    ctypes.c_int32, ctypes.c_void_p, ctypes.POINTER(ctypes.c_char), ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint32))
DESTROY_SESSION = ctypes.CFUNCTYPE(ctypes.c_int32, ctypes.c_void_p)


class StreamApiV1(ctypes.Structure):
    _fields_ = [
        ("abi_version", ctypes.c_uint32),
        ("struct_size", ctypes.c_uint32),
        ("list_adapters", TEXT_QUERY),
        ("start_session", START_SESSION),
        ("session_status", SESSION_STATUS),
        ("stop_session", STOP_SESSION),
        ("destroy_session", DESTROY_SESSION),
        ("capabilities", TEXT_QUERY),
    ]


def dll_error(buffer, fallback):
    return buffer.value.decode("utf-8", "replace") or fallback


def dll_text(make_call):
    """Read bounded DLL text: probe for size, retry while status changes."""
    error = ctypes.create_string_buffer(2048)
    error_written = ctypes.c_uint32(0)
    required = ctypes.c_uint32(0)
    code = make_call(None, 0, ctypes.byref(required), error, len(error),
                     ctypes.byref(error_written))
    if code < 0:
        raise RuntimeError(dll_error(error, "data-plane query failed"))
    if code != 1 or required.value == 0 or required.value > 4 * 1024 * 1024:
        raise RuntimeError(f"invalid DLL text length: code={code} required={required.value}")
    for _ in range(8):
        output = ctypes.create_string_buffer(required.value + 1024)
        written = ctypes.c_uint32(0)
        code = make_call(output, len(output), ctypes.byref(written), error, len(error),
                         ctypes.byref(error_written))
        if code == 1:
            required = written
            continue
        if code < 0:
            raise RuntimeError(dll_error(error, "data-plane query failed"))
        if code != 0 or written.value == 0 or written.value > len(output):
            raise RuntimeError(f"invalid DLL text result: code={code} written={written.value}")
        return output.value.decode("utf-8")
    raise RuntimeError("DLL status changed too quickly to read")


class DllReceiver:
    """A receiver session owned by this process through the real vc_stream.dll ABI."""

    def __init__(self, dll_path):
        self.dll_path = dll_path
        # Keep the loader handle alive for the lifetime of the session.
        self._dll_dir = os.add_dll_directory(str(dll_path.parent))
        self._dll = ctypes.CDLL(str(dll_path))
        get_api = self._dll.vc_module_get_api
        get_api.argtypes = [ctypes.c_uint32]
        get_api.restype = ctypes.POINTER(StreamApiV1)
        pointer = get_api(ABI_VERSION)
        if not pointer:
            raise RuntimeError(f"{dll_path} does not implement data-plane ABI v{ABI_VERSION}")
        api = pointer.contents
        if api.abi_version != ABI_VERSION or api.struct_size < ctypes.sizeof(StreamApiV1):
            raise RuntimeError(
                f"{dll_path} returned incompatible data-plane ABI "
                f"version={api.abi_version} size={api.struct_size}")
        self._api_pointer = pointer
        self.api = api
        self.handle = None
        self._log = None

    def start(self, request, log_path):
        self._log = log_path.open("w", encoding="utf-8")
        self._record({"event": "load", "dll": str(self.dll_path), "pid": os.getpid(),
                      "abi_version": ABI_VERSION, "struct_size": ctypes.sizeof(StreamApiV1)})
        self._record({"event": "start", "role": ROLE_RECEIVER, "request": request})
        payload = json.dumps(request).encode()
        handle = ctypes.c_void_p()
        error = ctypes.create_string_buffer(2048)
        written = ctypes.c_uint32(0)
        code = self.api.start_session(ROLE_RECEIVER, payload, ctypes.byref(handle),
                                      error, len(error), ctypes.byref(written))
        if code != 0:
            raise RuntimeError(dll_error(error, "start receiver failed"))
        if not handle.value:
            raise RuntimeError("vc_stream.dll returned a null session handle")
        self.handle = handle
        return self

    def _record(self, value):
        if self._log:
            self._log.write(json.dumps(value, ensure_ascii=False, default=str) + "\n")
            self._log.flush()

    def _open(self):
        return self.handle is not None and bool(self.handle.value)

    def status(self):
        if not self._open():
            raise RuntimeError("receiver session is not open")
        text = dll_text(lambda out, cap, written, err, err_cap, err_written:
                        self.api.session_status(self.handle, out, cap, written,
                                                err, err_cap, err_written))
        value = json.loads(text)
        self._record({"event": "status", "status": value})
        return value

    def wait_for(self, predicate, timeout, label):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            last = self.status()
            if last.get("error") or last.get("exit_code") not in (None, 0):
                raise AssertionError(f"receiver failed while waiting for {label}: {last}")
            if predicate(last):
                return last
            time.sleep(.05)
        raise AssertionError(f"timed out waiting for {label}: {last}")

    def stop(self):
        error = ctypes.create_string_buffer(2048)
        written = ctypes.c_uint32(0)
        code = self.api.stop_session(self.handle, error, len(error), ctypes.byref(written))
        if code < 0:
            raise RuntimeError(dll_error(error, "stop receiver failed"))
        self._record({"event": "stop", "joined": code == 0})
        return code

    def destroy(self):
        code = self.api.destroy_session(self.handle)
        self._record({"event": "destroy", "code": code})
        return code

    def close(self, timeout=5):
        if not self._open():
            self._close_log()
            return
        deadline = time.monotonic() + timeout
        try:
            while time.monotonic() < deadline:
                try:
                    status = self.status()
                except Exception as error:
                    self._record({"event": "cleanup-status-error", "error": str(error)})
                    break
                if status.get("running") is False:
                    break
                try:
                    self.stop()
                except Exception as error:
                    self._record({"event": "cleanup-stop-error", "error": str(error)})
                    break
                time.sleep(.05)
            while self.destroy() == 1 and time.monotonic() < deadline:
                time.sleep(.05)
        finally:
            self.handle = None
            self._close_log()

    def _close_log(self):
        if self._log:
            self._log.close()
            self._log = None


def main():
    assert CAPTURE.is_file() and STREAM_DLL.is_file(), (
        f"missing {CAPTURE} or {STREAM_DLL}; build the rust workspace or pass --package")
    # Ensure native child console control handlers can flush final stats.
    if not ctypes.windll.kernel32.GetConsoleWindow():
        ctypes.windll.kernel32.AllocConsole()
        ctypes.windll.user32.ShowWindow(ctypes.windll.kernel32.GetConsoleWindow(), 0)
    flags = subprocess.CREATE_NEW_PROCESS_GROUP
    service = receiver = None
    try:
        with (RUN / "capture.log").open("wb") as log:
            service = subprocess.Popen([str(CAPTURE), "--allow-loopback-diagnostics", "--bind", "127.0.0.1", "--port", str(PORT), "--root", str(RUN), "--native-dir", str(NATIVE)], stdout=log, stderr=log, creationflags=flags)
        deadline = time.monotonic() + 10
        while True:
            assert service.poll() is None, (RUN / "capture.log").read_text()
            try:
                health = request("/health")
                break
            except urllib.error.URLError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(.1)
        assert health["implementation"] == "virtualcapture-rust" and health["api_revision"] == 2
        config = request("/config")
        bad = request("/config", {"capture": {"width": 417}}, 400)
        assert bad["error"] and request("/config")["capture"]["width"] == config["capture"]["width"]
        config = request("/config", {"capture": {"mode": "pattern", "width": WIDTH, "height": HEIGHT, "fps": FPS, "frames": FRAMES, "gpu_convert": False}})["config"]
        capture = config["capture"]
        body = {"capture": capture, "session_id": SID, "source_mac": MAC, "auto_adapter": True, "adapter": ""}
        prepared = request("/sender/prepare", body)
        assert prepared["capture"] == capture and prepared["session_id"] == SID and prepared["adapter"]["name"] == LOOPBACK
        assert prepared["batch_packets"] == 1  # Npcap Loopback diagnostic exception; physical NICs batch all 29 packets
        assert not request("/health")["running"]
        receiver = DllReceiver(STREAM_DLL)
        receiver.start({
            "adapter": LOOPBACK, "width": WIDTH, "height": HEIGHT, "fps": FPS, "mtu": MTU,
            "session": SID, "codec": RAW_CODEC, "source_mac": MAC_BYTES,
            "expect_frames": FRAMES, "timeout_s": 30, "shm": SHM,
            "stats": str(RUN / "rx.json"), "hashlog": str(RUN / "rx.jsonl"),
        }, RUN / "receiver.log")
        ready = receiver.wait_for(lambda value: value.get("ready") is True, 15, "DLL receiver ready")
        assert ready.get("running") is True and ready.get("role") == "receiver", ready
        assert ready.get("session_id") == SID and ready.get("adapter") == LOOPBACK, ready
        assert ready["module"] == "vc_stream.dll" and ready["abi_version"] == 1, ready
        assert ready["pid"] == os.getpid(), ready
        started = request("/sender/start", body)
        assert started["capture"] == capture and started["source_mac"] == MAC
        request("/sender/stop", {"session_id": "s-wrong"}, 409)
        request("/config", {"capture": {"fps": 30}}, 409)
        request("/sender/start", body, 409)
        deadline = time.monotonic() + 10
        while True:
            stats = request("/sender/stats")
            if not stats["running"]:
                break
            assert time.monotonic() < deadline, stats
            time.sleep(.1)
        time.sleep(.3)
        request("/sender/stop", {"session_id": SID})
        done = receiver.wait_for(lambda value: value.get("running") is False, 35, "DLL receiver completion")
        assert done["exit_code"] == 0, done
        joined = receiver.stop()
        join_deadline = time.monotonic() + 5
        while joined == 1 and time.monotonic() < join_deadline:
            time.sleep(.05)
            joined = receiver.stop()
        assert joined == 0, "receiver worker must join after completion"
        assert receiver.destroy() == 0, "destroy must release a joined session"
        receiver.handle = None
        receiver.close()
        runtime_data = RUN / "data" if health.get("layout") == "managed" else RUN
        tx_path = runtime_data / "run" / f"{SID}.sender.hash.jsonl"
        tx, rx = hash_rows(tx_path), hash_rows(RUN / "rx.jsonl")
        assert len(tx) == FRAMES and tx == rx, (len(tx), len(rx), len(tx.keys() - rx.keys()))
        evidence = tx_path.read_bytes()
        reused = request("/sender/start", body, 409)
        assert reused["error"] and tx_path.read_bytes() == evidence, "Reusing a session must preserve its hashlog"
        rstats = json.loads((RUN / "rx.json").read_text())
        assert rstats["hash_mismatch"] == 0 and rstats["dropped"] == 0, rstats
        report = {"ok": True, "scope": "local Loopback functional evidence only", "pattern": "motion", "batch_packets": prepared["batch_packets"], "frame_packets": 29, "batch_policy": "Loopback diagnostic per-packet exception; physical NIC uses one frame per sendqueue submit", "adapter": LOOPBACK, "width": WIDTH, "height": HEIGHT, "fps_limit": FPS, "sent_frames": len(tx), "received_frames": len(rx), "hash_equal": True, "shm": SHM, "receiver": {"module": done["module"], "abi_version": done["abi_version"], "pid": done["pid"], "ready_before_send": True, "exit_code": done["exit_code"], "stop_joined": True, "destroyed": True}, "checks": ["Rust health identity", "invalid config leaves draft intact", "prepare has no process side effects", "exact capture/session/source echo", "reject wrong-session stop", "reject running config change", "reject replacing running sender", "reject reused session without overwriting hashlog", "in-process vc_stream.dll receiver ready before sender", "DLL receiver 120-frame FNV reconciliation", "DLL stop joins a completed receiver and destroy releases it", "Ctrl-Break graceful service shutdown"]}
        stop(service)
        assert service.returncode == 0, service.returncode
        (RUN / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps({"run": str(RUN), **report}, ensure_ascii=False))
    finally:
        if receiver:
            receiver.close()
        stop(service)


if __name__ == "__main__":
    main()
