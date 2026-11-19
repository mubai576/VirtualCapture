"""Rust REST → native Raw motion Loopback smoke, private runtime and SHM.
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
NATIVE = args.package.resolve() if args.package else ROOT / "build" / "Release"
CAPTURE = NATIVE / "vc-capture.exe" if args.package else ROOT / "rust" / "target" / "debug" / "vc-capture.exe"
SID = "s-" + uuid.uuid4().hex
MAC = "02:14:11:26:00:01"
SHM = "Local\\VirtualCaptureRustSmoke." + uuid.uuid4().hex
LOOPBACK = "\\Device\\NPF_Loopback"
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


def main():
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
        assert health["implementation"] == "virtualcapture-rust" and health["api_revision"] == 1
        config = request("/config")
        bad = request("/config", {"capture": {"width": 417}}, 400)
        assert bad["error"] and request("/config")["capture"]["width"] == config["capture"]["width"]
        config = request("/config", {"capture": {"mode": "pattern", "width": 416, "height": 416, "fps": 60, "frames": 120, "batch": 1, "gpu_convert": False}})["config"]
        capture = config["capture"]
        body = {"capture": capture, "session_id": SID, "source_mac": MAC, "auto_adapter": True, "adapter": ""}
        prepared = request("/sender/prepare", body)
        assert prepared["capture"] == capture and prepared["session_id"] == SID and prepared["adapter"]["name"] == LOOPBACK
        assert not request("/health")["running"]
        with (RUN / "receiver.log").open("wb") as log:
            receiver = subprocess.Popen([str(NATIVE / "vc-receiver.exe"), "--adapter", LOOPBACK, "--width", "416", "--height", "416", "--fps", "60", "--mtu", "9000", "--source-mac", MAC, "--session", SID, "--stats", str(RUN / "rx.json"), "--hashlog", str(RUN / "rx.jsonl"), "--shm", SHM, "--timeout-s", "12"], stdout=log, stderr=log, creationflags=flags)
        time.sleep(.4)
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
        stop(receiver)
        runtime_data = RUN / "data" if health.get("layout") == "managed" else RUN
        tx_path = runtime_data / "run" / f"{SID}.sender.hash.jsonl"
        tx, rx = hash_rows(tx_path), hash_rows(RUN / "rx.jsonl")
        assert len(tx) == 120 and tx == rx, (len(tx), len(rx), len(tx.keys() - rx.keys()))
        evidence = tx_path.read_bytes()
        reused = request("/sender/start", body, 409)
        assert reused["error"] and tx_path.read_bytes() == evidence, "Reusing a session must preserve its hashlog"
        rstats = json.loads((RUN / "rx.json").read_text())
        assert rstats["hash_mismatch"] == 0 and rstats["dropped"] == 0, rstats
        report = {"ok": True, "scope": "local Loopback functional evidence only", "pattern": "motion", "batch": 1, "adapter": LOOPBACK, "width": 416, "height": 416, "fps_limit": 60, "sent_frames": len(tx), "received_frames": len(rx), "hash_equal": True, "shm": SHM, "checks": ["Rust health identity", "invalid config leaves draft intact", "prepare has no process side effects", "exact capture/session/source echo", "reject wrong-session stop", "reject running config change", "reject replacing running sender", "reject reused session without overwriting hashlog", "120-frame FNV reconciliation", "Ctrl-Break graceful service shutdown"]}
        stop(service)
        assert service.returncode == 0, service.returncode
        (RUN / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps({"run": str(RUN), **report}, ensure_ascii=False))
    finally:
        stop(receiver)
        stop(service)


if __name__ == "__main__":
    main()
