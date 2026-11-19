"""HostController local/peer update smoke test with a real helper and GPUI host.

This test does not call REST endpoints directly to simulate the host: a small
standalone Rust driver (rust/tests/host_update_fixture, built in an independent
Cargo target under run/) links vc-core, sends the real `host::Command`s to
`HostController`, and exits normally after `restart_requested`. The spawned
`vc-updater` helper then switches the host root and starts a new `vc-host.exe`;
the parent Python process waits for the GPUI ready marker, verifies the window,
posts WM_CLOSE, and checks the process exits.

On the peer side a private `vc-capture.exe` root is started on a random loopback
port; the driver pairs its update token, stages and activates a synthetic
package through HostController, and the real capture helper restarts it.

Everything uses private temporary roots and synthetic packages. Nothing is
registered with DShow, no NIC capture or PC2 connection is performed, and the
rust workspace manifest/lockfile is not touched.

Run after a build:
    python -I rust/tests/host_update_smoke.py
    python -I rust/tests/host_update_smoke.py --package <extracted Release dir>
"""

import argparse
import ctypes
from ctypes import wintypes
import hashlib
import io
import json
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
import winreg
import zipfile

ROOT = Path(__file__).resolve().parents[2]
TESTS = Path(__file__).resolve().parent
FIXTURE = TESTS / "host_update_fixture"
CORE = ROOT / "rust/crates/vc-core"
RUN = ROOT / "run" / ("rust-host-update-smoke-" + uuid.uuid4().hex[:10])
PRODUCT = "VirtualCapture-Rust-portable"
CREATE_NO_WINDOW = 0x08000000
WM_CLOSE = 0x0010
STILL_ACTIVE = 259
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
CLSID = "{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}"
DSHOW_KEY = rf"Software\Classes\CLSID\{CLSID}\InprocServer32"


class SmokeError(RuntimeError):
    pass


# ------------------------------------------------------------------ Windows

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
user32 = ctypes.WinDLL("user32", use_last_error=True)
EnumWindowsProc = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)

kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
kernel32.OpenProcess.restype = wintypes.HANDLE
kernel32.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
kernel32.GetExitCodeProcess.restype = wintypes.BOOL
kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
user32.EnumWindows.argtypes = [EnumWindowsProc, wintypes.LPARAM]
user32.EnumWindows.restype = wintypes.BOOL
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.IsWindowVisible.argtypes = [wintypes.HWND]
user32.GetWindowTextLengthW.argtypes = [wintypes.HWND]
user32.GetWindowTextLengthW.restype = ctypes.c_int
user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]


def pid_alive(pid):
    if not pid:
        return False
    handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, int(pid))
    if not handle:
        return False
    try:
        code = wintypes.DWORD()
        return bool(kernel32.GetExitCodeProcess(handle, ctypes.byref(code))) and code.value == STILL_ACTIVE
    finally:
        kernel32.CloseHandle(handle)


def windows_for_pid(pid):
    found = []

    @EnumWindowsProc
    def visit(hwnd, _lparam):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid and user32.IsWindowVisible(hwnd):
            length = user32.GetWindowTextLengthW(hwnd)
            buffer = ctypes.create_unicode_buffer(length + 1)
            user32.GetWindowTextW(hwnd, buffer, length + 1)
            found.append((int(hwnd), buffer.value))
        return True

    user32.EnumWindows(visit, 0)
    return found


def close_windows(handles):
    for hwnd, _title in handles:
        user32.PostMessageW(hwnd, WM_CLOSE, 0, 0)


def dshow_registration():
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, DSHOW_KEY) as key:
            return winreg.QueryValueEx(key, "")[0]
    except FileNotFoundError:
        return None


# ------------------------------------------------------------------ helpers

def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def wait_for(check, timeout, label, interval=0.2):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = check()
        if last:
            return last
        time.sleep(interval)
    raise SmokeError(f"timed out waiting for {label}; last={last!r}")


def http_json(port, path, timeout=2.0):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=timeout) as response:
            return json.loads(response.read() or b"{}")
    except (urllib.error.URLError, TimeoutError, ConnectionError, json.JSONDecodeError):
        return None


def capture_health(port):
    value = http_json(port, "/health")
    if value and value.get("ok") is True and value.get("implementation") == "virtualcapture-rust" \
            and value.get("role") == "capture":
        return value
    return None


def kill_tree(pid):
    if pid:
        subprocess.run(["taskkill", "/PID", str(pid), "/T", "/F"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def kill_under(root):
    text = str(root).replace("'", "''")
    script = (
        "$self=$PID; Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -ne $self -and $_.CommandLine -and $_.CommandLine.Contains('"
        + text + "') } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }"
    )
    subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", script],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def history(root):
    path = Path(root) / "updates/history.jsonl"
    try:
        return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    except FileNotFoundError:
        return []


# ------------------------------------------------------------------ binaries

def resolve_binaries(args):
    package = args.package.resolve() if args.package else None
    bin_dir = (args.bin or package or ROOT / "rust/target/debug").resolve()
    native = args.native or package or bin_dir
    required = ["vc-host.exe", "vc-capture.exe", "vc-updater.exe"]
    missing = [name for name in required if not (bin_dir / name).is_file()]
    if missing:
        raise SmokeError(
            f"missing {', '.join(missing)} in {bin_dir}; build the rust workspace or pass --package")
    missing_native = [
        name for name in ("vc_stream.dll", "vc_vcam.dll") if not (native / name).is_file()
    ]
    if missing_native:
        raise SmokeError(
            f"missing {', '.join(missing_native)} in {native}; pass --package <extracted release> "
            "or --native <dir> (the peer capture needs vc_stream.dll; the new host needs both "
            "vc_stream.dll and vc_vcam.dll)")
    return bin_dir, native


def build_fixture():
    fixture = RUN / "fixture"
    shutil.copytree(FIXTURE, fixture)
    manifest = fixture / "Cargo.toml"
    text = manifest.read_text(encoding="utf-8")
    core = str(CORE).replace("\\", "/")
    text = text.replace('path = "../../crates/vc-core"', f'path = "{core}"')
    manifest.write_text(text, encoding="utf-8")
    target = ROOT / "build/host-update-fixture-smoke-target"
    log_path = RUN / "fixture-build.log"
    with log_path.open("w", encoding="utf-8") as log:
        result = subprocess.run(
            ["cargo", "build", "--manifest-path", str(manifest), "--target-dir", str(target)],
            cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, text=True)
    if result.returncode:
        tail = log_path.read_text(encoding="utf-8", errors="replace")[-4000:]
        raise SmokeError(f"fixture build failed; see {log_path}\n{tail}")
    exe = target / "debug/host-update-fixture.exe"
    if not exe.is_file():
        exe = target / "debug/host-update-fixture"
    if not exe.is_file():
        raise SmokeError(f"fixture binary not found under {target}")
    return exe


# ------------------------------------------------------------------ packages

def make_package(path, build_id, version, files, manifest_bytes=None):
    prefix = f"VirtualCapture-Rust-{version}-{build_id}"
    if manifest_bytes is None:
        manifest = {
            "product": PRODUCT,
            "schema": 1,
            "version": version,
            "build_id": build_id,
            "files": [
                {"path": name, "bytes": len(data), "sha256": sha256(data)}
                for name, data in files.items()
            ],
        }
        manifest_bytes = json.dumps(manifest).encode()
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(f"{prefix}/package-manifest.json", manifest_bytes)
        for name, data in files.items():
            archive.writestr(f"{prefix}/{name}", data)


def rezip_extracted_package(package, target):
    """Re-ZIP an extracted candidate release with its own manifest/build_id intact.

    Only files named by the manifest are copied, plus the manifest itself. The
    extracted root may also contain runtime data left by earlier smoke runs
    (for example `update-token.json`); `rglob` would leak that into the update.
    """
    manifest_path = package / "package-manifest.json"
    if not manifest_path.is_file():
        raise SmokeError(f"{package} has no package-manifest.json; pass the extracted package root")
    manifest = read_json(manifest_path)
    forbidden = {
        "update-token.json", "update-peer-token.json", "vc-rust-config.json",
        "vc-config.json", "package-manifest.json",
    }
    files = {}
    for entry in manifest.get("files", []):
        name = entry["path"]
        if Path(name).name in forbidden:
            raise SmokeError(f"package manifest lists runtime data: {name}")
        source = package / name
        if not source.is_file():
            raise SmokeError(f"package manifest lists missing file: {source}")
        files[name] = source.read_bytes()
    make_package(target, manifest["build_id"], manifest["version"], files,
                 manifest_bytes=manifest_path.read_bytes())
    return manifest["build_id"], manifest["version"], files


def native_runtime_files(native_dir):
    files = {}
    for name in ("libpyrowave-shared-1.dll", "msvcp140.dll", "vcruntime140.dll",
                 "vcruntime140_1.dll"):
        source = native_dir / name
        if source.is_file():
            files[name] = source.read_bytes()
    return files


def version_json(version):
    return json.dumps({"product": "VirtualCapture-Rust", "version": version}).encode()


# ------------------------------------------------------------------ test

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path, help="extracted Rust portable Release directory")
    parser.add_argument("--bin", type=Path, help="directory containing vc-host/vc-capture/vc-updater")
    parser.add_argument("--native", type=Path, help="directory containing vc_stream.dll and vc_vcam.dll")
    args = parser.parse_args()

    RUN.mkdir(parents=True)
    host_root = RUN / "host"
    peer_root = RUN / "peer"
    host_root.mkdir()
    peer_root.mkdir()
    bin_dir, native_dir = resolve_binaries(args)
    dshow_before = dshow_registration()
    checks = []
    pids = set()
    log = (RUN / "smoke.log").open("wb")
    try:
        peer_zip = RUN / "peer-update.zip"
        local_zip = RUN / "local-update.zip"
        # The local candidate keeps the extracted package's own manifest/build_id
        # and complete payload, so activation installs the real release tree
        # (including native DLLs) rather than a hand-picked subset.
        if args.package:
            local_build, local_version, local_files = rezip_extracted_package(
                args.package.resolve(), local_zip)
        else:
            local_build = "local-" + uuid.uuid4().hex[:8]
            local_version = "0.1.2"
            local_files = {
                "vc-host.exe": (bin_dir / "vc-host.exe").read_bytes(),
                "vc-updater.exe": (bin_dir / "vc-updater.exe").read_bytes(),
                "version.json": version_json(local_version),
            }
            local_files.update(native_runtime_files(native_dir))
            for name in ("vc_stream.dll", "vc_vcam.dll"):
                local_files[name] = (native_dir / name).read_bytes()
            make_package(local_zip, local_build, local_version, local_files)

        # The peer package is intentionally minimal, but a capture release is
        # launched with --native-dir pointing at its own release directory, so
        # vc_stream.dll and its runtime DLLs must travel with it.
        peer_build = "peer-" + uuid.uuid4().hex[:8]
        peer_version = local_version if args.package else "0.1.1"
        peer_files = {
            "vc-capture.exe": (bin_dir / "vc-capture.exe").read_bytes(),
            "vc-updater.exe": (bin_dir / "vc-updater.exe").read_bytes(),
            "vc_stream.dll": (native_dir / "vc_stream.dll").read_bytes(),
            "version.json": version_json(peer_version),
        }
        peer_files.update(native_runtime_files(native_dir))
        make_package(peer_zip, peer_build, peer_version, peer_files)

        port = free_port()
        capture = subprocess.Popen(
            [str(bin_dir / "vc-capture.exe"), "--root", str(peer_root), "--native-dir", str(native_dir),
             "--bind", "127.0.0.1", "--port", str(port)],
            cwd=peer_root, stdout=log, stderr=log, creationflags=CREATE_NO_WINDOW)
        pids.add(capture.pid)
        health = wait_for(lambda: capture_health(port), 20, "private capture health")
        assert health["layout"] == "legacy", health
        first_pid = health["pid"]
        pids.add(first_pid)
        token_path = wait_for(
            lambda: (peer_root / "update-token.json") if (peer_root / "update-token.json").is_file() else None,
            15, "capture update token")
        peer_token = read_json(token_path)["token"]
        assert len(peer_token) == 64 and all(ch in "0123456789abcdefABCDEF" for ch in peer_token)
        checks.append("private vc-capture started; random port; token read from its root")

        print(f"building standalone fixture (independent target, no workspace build): {RUN / 'fixture'}")
        fixture_exe = build_fixture()
        checks.append("standalone vc-core fixture built without changing rust/Cargo.toml")

        result_path = RUN / "fixture-result.json"
        fixture_log = RUN / "fixture.log"
        with fixture_log.open("wb") as fixture_out:
            fixture = subprocess.Popen(
                [str(fixture_exe),
                 "--host-root", str(host_root), "--bin", str(native_dir),
                 "--peer-url", f"http://127.0.0.1:{port}", "--peer-token", peer_token,
                 "--peer-zip", str(peer_zip), "--peer-build", peer_build,
                 "--local-zip", str(local_zip), "--local-build", local_build,
                 "--result", str(result_path)],
                cwd=host_root, stdout=fixture_out, stderr=fixture_out,
                creationflags=CREATE_NO_WINDOW)
        pids.add(fixture.pid)
        try:
            code = fixture.wait(timeout=360)
        except subprocess.TimeoutExpired:
            kill_tree(fixture.pid)
            raise SmokeError(f"HostController fixture timed out; see {fixture_log}")
        if code != 0:
            tail = fixture_log.read_text(encoding="utf-8", errors="replace")[-4000:]
            raise SmokeError(f"HostController fixture failed ({code}); see {fixture_log}\n{tail}")
        result = read_json(result_path)
        assert result["ok"] is True, result
        assert result["peer_build"] == peer_build and result["local_build"] == local_build, result
        checks.append("HostController: token format rejection, no-session rejects, Connect, peer stage/activate, local stage/activate")

        peer_after = wait_for(
            lambda: (lambda value: value if value and value.get("build") == peer_build and value.get("pid") != first_pid else None)(capture_health(port)),
            90, "peer capture restarted with new build")
        pids.add(peer_after["pid"])
        assert capture.poll() is not None, "old capture process still running"
        peer_pointer = read_json(peer_root / "app/current.json")
        assert peer_pointer["build_id"] == peer_build, peer_pointer
        peer_release = peer_root / "app/releases" / peer_build
        assert (peer_release / "vc-capture.exe").is_file()
        assert (peer_release / "vc_stream.dll").is_file(), (
            "peer capture release must carry the real vc_stream.dll")
        assert any(entry.get("build_id") == peer_build and entry.get("state") == "active"
                   for entry in history(peer_root)), history(peer_root)
        checks.append("peer helper switched capture to the staged build and restarted it with vc_stream.dll")

        jobs = sorted((host_root / "updates/worker").glob("job-*.json"))
        assert jobs, "host helper job not created"
        job = read_json(jobs[-1])
        assert job["role"] == "host" and job["target"] == local_build, job
        assert job["parent_pid"] == fixture.pid, job
        ready_path = Path(job["ready_path"])
        ready = wait_for(lambda: read_json(ready_path) if ready_path.is_file() else None,
                         120, "new GPUI host ready marker")
        assert ready["schema"] == 1 and ready["build"] == local_build, ready
        host_pid = ready["pid"]
        pids.add(host_pid)
        assert Path(ready["root"]).samefile(host_root), ready
        pointer = read_json(host_root / "app/current.json")
        assert pointer["build_id"] == local_build, pointer
        host_release = host_root / "app/releases" / local_build
        assert (host_release / "vc-host.exe").is_file()
        assert (host_release / "vc_stream.dll").is_file(), "host release must carry vc_stream.dll"
        assert (host_release / "vc_vcam.dll").is_file(), "host release must carry vc_vcam.dll"
        checks.append("vc-updater helper installed local build with vc_stream.dll/vc_vcam.dll and new vc-host wrote first-frame ready marker")

        windows = wait_for(lambda: windows_for_pid(host_pid) or None, 30, "new GPUI window")
        titles = [title for _hwnd, title in windows]
        assert any("VirtualCapture" in title for title in titles), windows
        wait_for(lambda: any(entry.get("build_id") == local_build and entry.get("state") == "active"
                             for entry in history(host_root)), 30, "helper confirmed first frame")
        close_windows(windows)
        exited = wait_for(lambda: not pid_alive(host_pid), 20, "new GPUI host exit after WM_CLOSE")
        assert exited, "WM_CLOSE did not end the host"
        checks.append(f"new GPUI window(s) {titles!r} closed with WM_CLOSE; process exited")

        helper_logs = list((host_root / "updates/worker").glob("updater-*.log"))
        wait_for(lambda: any(f"active {local_build}" in path.read_text(encoding="utf-8", errors="replace")
                             for path in helper_logs), 30, "host helper active log")
        assert any(entry.get("build_id") == local_build and entry.get("state") == "active"
                   for entry in history(host_root)), history(host_root)
        checks.append("host helper log/history confirm active local build")

        if "VirtualCapture.exe" in local_files:
            entry = host_root / "VirtualCapture.exe"
            assert entry.read_bytes() == local_files["VirtualCapture.exe"], "new host did not install ROOT entry"
            launcher = subprocess.run([str(entry)], cwd=RUN, stdout=log, stderr=log, timeout=20)
            assert launcher.returncode == 0, "graphical entry could not reopen the updated host"
            receipts = list((host_root / "data/run").glob("host-*.launch.json"))
            assert receipts, "no graphical launch receipt after OTA"
            receipt = read_json(max(receipts, key=lambda path: path.stat().st_mtime_ns))
            assert Path(receipt["root"]).samefile(host_root), receipt
            assert Path(receipt["exe"]).samefile(host_root / "app/releases" / local_build / "vc-host.exe"), receipt
            reopened_pid = int(receipt["pid"])
            pids.add(reopened_pid)
            reopened = wait_for(lambda: windows_for_pid(reopened_pid) or None, 30, "updated host reopened from ROOT EXE")
            close_windows(reopened)
            wait_for(lambda: not pid_alive(reopened_pid), 20, "reopened host closed")
            checks.append("OTA installed ROOT VirtualCapture.exe; direct EXE reopened current release and closed normally")

        peer_token_file = host_root / "data/update-peer-token.json"
        assert read_json(peer_token_file)["token"] == peer_token, peer_token_file
        host_config = read_json(host_root / "data/vc-rust-config.json")
        assert host_config["peer_url"] == f"http://127.0.0.1:{port}", host_config
        checks.append("pairing token and config survived legacy -> managed migration")

        dshow_after = dshow_registration()
        assert dshow_before == dshow_after, (dshow_before, dshow_after)
        checks.append("no DShow registration change")

        report = {
            "ok": True,
            "scope": "HostController -> real vc-updater -> new GPUI first frame; peer capture restarted via real helper",
            "run": str(RUN),
            "builds": {"peer": peer_build, "local": local_build},
            "checks": checks,
            "limits": [
                "private temporary roots; random loopback control port",
                "no DShow registration, no NIC capture, no PC2 connection",
                "peer package is synthetic; local activation reuses the extracted candidate tree under --package; "
                "SHA-256 manifest only, not a signature",
                "GPUI window is closed with WM_CLOSE from this process",
            ],
        }
        (RUN / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps(report, ensure_ascii=False))
    finally:
        log.close()
        for pid in pids:
            kill_tree(pid)
        kill_under(peer_root)
        kill_under(host_root)


if __name__ == "__main__":
    try:
        main()
    except SmokeError as error:
        print(f"host_update_smoke: {error}", file=sys.stderr)
        raise SystemExit(1)
