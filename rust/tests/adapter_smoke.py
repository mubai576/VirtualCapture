"""Check capture-side adapter visibility without starting any capture.

Runs the real `vc-capture.exe` on a random loopback port with an isolated ROOT.
`--list-adapters` must stay a full diagnostic list, while `/adapters` must expose
only confirmed physical NICs by default. Loopback requires the explicit
`--allow-loopback-diagnostics` flag; if this build does not accept it yet, the
loopback half is reported as unsupported instead of being silently skipped.
"""
import argparse
import ctypes as c
import json
from pathlib import Path
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[2]
EXE = ROOT / "rust/target/debug/vc-capture.exe"
NATIVE = ROOT / "rust/target/debug"
RUN = ROOT / "run" / ("rust-adapter-smoke-" + uuid.uuid4().hex[:10])
RUN.mkdir(parents=True)
LOOPBACK = r"\Device\NPF_Loopback"


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def get(port, path, timeout=40):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=timeout) as response:
        return json.load(response)


def post(port, path, body, timeout=40):
    payload = json.dumps(body).encode()
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}", data=payload,
        headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, json.load(error)


def start_service(port, root, extra=()):
    log = (root / "capture.log").open("wb")
    proc = subprocess.Popen(
        [str(EXE), "--bind", "127.0.0.1", "--port", str(port), "--root", str(root),
         "--native-dir", str(NATIVE), *extra],
        stdout=log, stderr=log,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
    )
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError((root / "capture.log").read_text(errors="replace"))
        try:
            return proc, get(port, "/health", timeout=2)
        except OSError:
            time.sleep(.1)
    proc.kill()
    raise RuntimeError("capture service did not become healthy")


def stop_service(proc):
    if proc.poll() is not None:
        return proc.returncode
    # Only our own freshly spawned console process group; no active capture exists.
    c.windll.kernel32.GenerateConsoleCtrlEvent(1, proc.pid)
    try:
        return proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        return proc.wait()


def supported_flag():
    out = subprocess.run([str(EXE), "--help"], capture_output=True, text=True,
                         encoding="utf-8", errors="replace", timeout=20)
    return "--allow-loopback-diagnostics" in (out.stdout + out.stderr)


def main():
    global EXE, NATIVE
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path)
    args = parser.parse_args()
    if args.package:
        NATIVE = args.package.resolve()
        EXE = NATIVE / "vc-capture.exe"
    assert EXE.is_file() and (NATIVE / "vc_stream.dll").is_file(), (
        f"Missing {EXE} or {NATIVE / 'vc_stream.dll'}; build rust workspace first")
    listed = subprocess.run(
        [str(EXE), "--root", str(RUN), "--native-dir", str(NATIVE), "--list-adapters"],
        capture_output=True, encoding="utf-8", timeout=45, check=True,
    )
    full = json.loads(listed.stdout)
    (RUN / "all-adapters.json").write_text(
        json.dumps(full, ensure_ascii=False, indent=2), encoding="utf-8")

    root = RUN / "default"
    root.mkdir()
    port = free_port()
    proc, health = start_service(port, root)
    try:
        visible = get(port, "/adapters?refresh=true")["items"]
        # Auto-route from a loopback controller would need NPF_Loopback; without the
        # explicit diagnostics flag this must be refused before any process starts.
        capture = get(port, "/config")["capture"]
        capture.update({"mode": "pattern", "pattern": "motion", "fps": 60,
                        "width": 416, "height": 416, "gpu_convert": False})
        status, refused = post(port, "/sender/prepare", {
            "capture": capture, "session_id": "s-adapter-probe",
            "source_mac": "02:14:11:26:00:02", "auto_adapter": True, "adapter": "",
        })
        still_idle = not get(port, "/health")["running"]
    finally:
        code = stop_service(proc)
    assert status == 409, (status, refused)
    assert "回环" in refused.get("error", ""), refused
    assert still_idle, "Loopback probe must not start a capture session"
    assert health["implementation"] == "virtualcapture-rust", health
    (RUN / "visible-adapters.json").write_text(
        json.dumps(visible, ensure_ascii=False, indent=2), encoding="utf-8")
    assert all(item.get("hardware_interface") is True and item.get("virtual") is False
               for item in visible), visible
    assert all(item["name"].lower() != LOOPBACK.lower() for item in visible), visible
    assert code == 0, code

    loopback_check = "unsupported by this build"
    if supported_flag():
        root = RUN / "diagnostics"
        root.mkdir()
        port = free_port()
        proc, _ = start_service(port, root, ("--allow-loopback-diagnostics",))
        try:
            with_diag = get(port, "/adapters?refresh=true")["items"]
        finally:
            code = stop_service(proc)
        (RUN / "diagnostic-adapters.json").write_text(
            json.dumps(with_diag, ensure_ascii=False, indent=2), encoding="utf-8")
        names = {item["name"] for item in with_diag}
        assert all(item.get("hardware_interface") is True and item.get("virtual") is False
                   for item in with_diag if item["name"].lower() != LOOPBACK.lower()), with_diag
        loopback_check = "listed" if LOOPBACK in names else "device not present"
        assert code == 0, code

    report = {
        "ok": True, "run": str(RUN), "full_listing": len(full),
        "visible": [item.get("label") or item["description"] for item in visible],
        "hidden": len(full) - len(visible),
        "loopback_with_flag": loopback_check,
        "loopback_prepare_status": status,
        "checks": ["diagnostic --list-adapters remains full",
                   "REST /adapters exposes only physical NICs",
                   "REST /adapters hides Npcap Loopback by default",
                   "Loopback auto-route refused without diagnostics flag",
                   "refused probe started no capture session"],
        "scope": "adapter visibility only; no packets or DShow operations",
    }
    (RUN / "report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    sys.exit(main())
