"""HostController local/peer update smoke test with a real helper and GPUI host.

This test does not call REST endpoints directly to simulate the host: a small
standalone Rust driver (rust/tests/host_update_fixture, built in an independent
Cargo target under run/) links vc-core, sends the real `host::Command`s to
`HostController`, and exits normally after `restart_requested`. The spawned
`vc-updater` helper then switches the host root and starts `VirtualCapture.exe`;
the parent Python process waits for its first-frame ready marker, verifies the
native window, posts WM_CLOSE, and checks for a zero exit code.

On the peer side `VirtualCapture.exe capture` runs on a random loopback port in
a private ROOT. The driver pairs its token and stages and activates a synthetic
package through HostController; the real capture helper restarts it.

Both roots are private. With --package, the local update uses the original
manifest and verified payload of the supplied candidate. Nothing is registered
with DShow, no NIC capture or PC2 connection is performed, and the rust
workspace manifest/lockfile is not touched.

Run after a build:
    python -I rust/tests/host_update_smoke.py
    python -I rust/tests/host_update_smoke.py --package <extracted Release dir>
"""

import argparse
import ctypes
from ctypes import wintypes
import hashlib
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
from PIL import ImageGrab

ROOT = Path(__file__).resolve().parents[2]
TESTS = Path(__file__).resolve().parent
FIXTURE = TESTS / "host_update_fixture"
CORE = ROOT / "rust/crates/vc-core"
RUN = ROOT / "run" / ("rust-host-update-smoke-" + uuid.uuid4().hex[:10])
PRODUCT = "VirtualCapture-Rust-portable"
ENTRYPOINT = "unified-v1"
APP_EXE = "VirtualCapture.exe"
UPDATER_EXE = "vc-updater.exe"
CREATE_NO_WINDOW = 0x08000000
WM_CLOSE = 0x0010
WAIT_OBJECT_0 = 0
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
SYNCHRONIZE = 0x00100000
CLSID = "{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}"
DSHOW_KEY = rf"Software\Classes\CLSID\{CLSID}\InprocServer32"


class SmokeError(RuntimeError):
    pass


# ------------------------------------------------------------------ Windows

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
user32 = ctypes.WinDLL("user32", use_last_error=True)
user32.SetProcessDPIAware()
EnumWindowsProc = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)

kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
kernel32.OpenProcess.restype = wintypes.HANDLE
kernel32.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
kernel32.GetExitCodeProcess.restype = wintypes.BOOL
kernel32.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
kernel32.WaitForSingleObject.restype = wintypes.DWORD
kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
user32.EnumWindows.argtypes = [EnumWindowsProc, wintypes.LPARAM]
user32.EnumWindows.restype = wintypes.BOOL
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.IsWindowVisible.argtypes = [wintypes.HWND]
user32.GetWindowTextLengthW.argtypes = [wintypes.HWND]
user32.GetWindowTextLengthW.restype = ctypes.c_int
user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
user32.SetWindowPos.argtypes = [wintypes.HWND, wintypes.HWND, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, wintypes.UINT]
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.GetForegroundWindow.restype = wintypes.HWND
user32.GetClientRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.ClientToScreen.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.POINT)]
user32.GetDpiForWindow.argtypes = [wintypes.HWND]
user32.AttachThreadInput.argtypes = [wintypes.DWORD, wintypes.DWORD, wintypes.BOOL]
user32.AttachThreadInput.restype = wintypes.BOOL
user32.PeekMessageW.argtypes = [ctypes.POINTER(wintypes.MSG), wintypes.HWND, wintypes.UINT, wintypes.UINT, wintypes.UINT]
user32.PeekMessageW.restype = wintypes.BOOL
user32.ShowWindow.argtypes = [wintypes.HWND, ctypes.c_int]
user32.BringWindowToTop.argtypes = [wintypes.HWND]
user32.SetActiveWindow.argtypes = [wintypes.HWND]
user32.SetFocus.argtypes = [wintypes.HWND]
kernel32.GetCurrentThreadId.restype = wintypes.DWORD


def focus_window(hwnd, timeout=2):
    """将测试窗口切到前台；调用线程先建立输入队列以配合 AttachThreadInput。"""
    message = wintypes.MSG()
    user32.PeekMessageW(ctypes.byref(message), None, 0, 0, 0)
    current_thread = kernel32.GetCurrentThreadId()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        foreground = user32.GetForegroundWindow()
        foreground_thread = user32.GetWindowThreadProcessId(foreground, None) if foreground else 0
        target_thread = user32.GetWindowThreadProcessId(hwnd, None)
        attached = []
        for thread_id in dict.fromkeys((foreground_thread, target_thread)):
            if thread_id and thread_id != current_thread:
                if user32.AttachThreadInput(current_thread, thread_id, True):
                    attached.append(thread_id)
        try:
            user32.ShowWindow(hwnd, 9)
            user32.BringWindowToTop(hwnd)
            user32.SetForegroundWindow(hwnd)
            user32.SetActiveWindow(hwnd)
            user32.SetFocus(hwnd)
        finally:
            for thread_id in reversed(attached):
                user32.AttachThreadInput(current_thread, thread_id, False)
        if user32.GetForegroundWindow() == hwnd:
            return True
        time.sleep(.05)
    return user32.GetForegroundWindow() == hwnd


def window_screenshot(hwnd, name):
    assert user32.SetWindowPos(hwnd, wintypes.HWND(-1), 0, 0, 0, 0, 0x0013)
    try:
        # 繁忙桌面上前台可能被其他应用短暂抢走；有界重试，但截图前仍必须是本窗口前台。
        deadline = time.monotonic() + 8
        while True:
            focus_window(hwnd)
            time.sleep(.3)
            if user32.GetForegroundWindow() == hwnd:
                break
            assert time.monotonic() < deadline, "target window is not foreground"
        rect = wintypes.RECT()
        origin = wintypes.POINT(0, 0)
        assert user32.GetClientRect(hwnd, ctypes.byref(rect))
        assert user32.ClientToScreen(hwnd, ctypes.byref(origin))
        image = ImageGrab.grab(bbox=(origin.x, origin.y, origin.x + rect.right,
                                    origin.y + rect.bottom), all_screens=True)
        image.save(RUN / name)
        colors = image.getcolors(256)
        assert colors is None or len(colors) > 8, ("blank GPUI window", name)
        return image
    finally:
        user32.SetWindowPos(hwnd, wintypes.HWND(-2), 0, 0, 0, 0, 0x0013)


def choose_managed_capture(hwnd):
    """Click the real chooser in the forwarded GPUI window, not a config fixture."""
    image = window_screenshot(hwnd, "managed-role-chooser.png")
    try:
        scale = user32.GetDpiForWindow(hwnd) / 96
        # Match both outlined action borders in the visible chooser before clicking.
        def edge(pixel):
            return (all(abs(channel - expected) <= 4 for channel, expected in zip(pixel[:3], (225, 230, 237)))
                    or (pixel[0] < 120 and 50 < pixel[1] < 150 and pixel[2] > 200))
        def runs(y, left, right):
            result, start = [], None
            for x in range(left, right):
                if edge(image.getpixel((x, y))):
                    if start is None:
                        start = x
                elif start is not None:
                    if x - start >= 55 * scale:
                        result.append((start, x - 1))
                    start = None
            return result
        buttons = []
        for top in range(image.height - round(42 * scale)):
            for left, right in runs(top, 0, image.width):
                if right - left > 230 * scale:
                    continue
                for bottom in range(top + round(25 * scale), top + round(40 * scale)):
                    if any(abs(a - left) <= 3 * scale and abs(b - right) <= 3 * scale
                           for a, b in runs(bottom, max(0, left - 5), min(image.width, right + 6))):
                        center = ((left + right) / 2, (top + bottom) / 2)
                        if not any(abs(center[0] - x) < 12 * scale and abs(center[1] - y) < 20 * scale
                                   for x, y in buttons):
                            buttons.append(center)
                        break
        buttons.sort()
        assert len(buttons) == 2, ("chooser must show exactly two outlined buttons", buttons)
        x, y = buttons[0]
        point = wintypes.POINT(round(x), round(y))
        # Posted client coordinates are physical pixels on this DPI-aware process.
        position = (point.y << 16) | point.x
        assert user32.PostMessageW(hwnd, 0x0201, 1, position)
        assert user32.PostMessageW(hwnd, 0x0202, 0, position)
    finally:
        user32.SetWindowPos(hwnd, wintypes.HWND(-2), 0, 0, 0, 0, 0x0013)


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


def close_host(pid, windows, label):
    # Hold the real host's handle across WM_CLOSE so disappearance alone can't
    # pass the check after a crash or forced shutdown.
    process = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, False, pid)
    if not process:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        for hwnd, _title in windows:
            assert user32.PostMessageW(hwnd, WM_CLOSE, 0, 0), (label, hwnd)
        waited = kernel32.WaitForSingleObject(process, 20_000)
        assert waited == WAIT_OBJECT_0, (label, "WM_CLOSE did not end the host", waited)
        code = wintypes.DWORD()
        assert kernel32.GetExitCodeProcess(process, ctypes.byref(code)) and code.value == 0, (label, code.value)
    finally:
        kernel32.CloseHandle(process)


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


def pending_json(path):
    # These files are created before the writer has completed the JSON record.
    try:
        return read_json(path)
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def wait_launch_receipt(root, exe, before=()):
    run_dir = root / "data/run"

    def find_new():
        receipts = set(run_dir.glob("host-*.launch.json")).difference(before)
        assert len(receipts) <= 1, receipts
        return pending_json(next(iter(receipts))) if receipts else None

    receipt = wait_for(find_new, 30, "fresh GPUI launch receipt")
    assert Path(receipt["root"]).samefile(root), receipt
    assert Path(receipt["exe"]).samefile(exe), receipt
    for stream in ("stdout", "stderr"):
        logfile = Path(receipt[stream])
        assert logfile.is_file() and logfile.parent.samefile(run_dir), receipt
    assert isinstance(receipt["pid"], int) and receipt["pid"] > 0, receipt
    return receipt


def expect_release(root, build, native_files):
    pointer = read_json(root / "app/current.json")
    assert pointer["schema"] == 1 and pointer["build_id"] == build, pointer
    release = root / "app/releases" / build
    manifest = read_json(release / "package-manifest.json")
    assert (manifest["schema"], manifest["entrypoint"], manifest["build_id"]) == (1, ENTRYPOINT, build), manifest
    assert manifest["capabilities"] == {"role_launch_args": True, "desktop_capture": True}, manifest
    assert {path.relative_to(release).as_posix() for path in release.rglob("*.exe")} == {APP_EXE, UPDATER_EXE}
    for name in native_files:
        assert (release / name).is_file(), f"installed release missing {name}: {release}"
    return release


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
    native = (args.native or package or bin_dir).resolve()
    required = [APP_EXE, UPDATER_EXE]
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
            "entrypoint": ENTRYPOINT,
            "capabilities": {"role_launch_args": True, "desktop_capture": True},
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
    if (manifest.get("product"), manifest.get("schema"), manifest.get("entrypoint")) != (PRODUCT, 1, ENTRYPOINT):
        raise SmokeError(f"{manifest_path} is not a unified-v1 schema 1 portable manifest")
    if manifest.get("capabilities") != {"role_launch_args": True, "desktop_capture": True}:
        raise SmokeError(f"{manifest_path} does not declare current launch capabilities")
    forbidden = {
        "update-token.json", "update-peer-token.json", "vc-rust-config.json",
        "vc-config.json", "package-manifest.json",
    }
    declared = manifest["files"]
    names = [entry["path"] for entry in declared]
    if {name for name in names if name.lower().endswith(".exe")} != {APP_EXE, UPDATER_EXE}:
        raise SmokeError("package manifest must declare exactly the two top-level unified EXEs")
    if len({name.lower() for name in names}) != len(names):
        raise SmokeError("package manifest has duplicate paths")
    files = {}
    for entry in declared:
        name = entry["path"]
        # Portable package paths are flat; do not follow links or read a credential
        # misdeclared as payload (including case/NTFS trailing-dot aliases).
        if (not name or name in (".", "..") or name.rstrip(" .") != name
                or any(char in name for char in "/\\:") or name.lower() in forbidden
                or name.lower().endswith((".cmd", ".ps1", ".bat", ".py"))):
            raise SmokeError(f"unexpected package manifest path: {name}")
        source = package / name
        if source.is_symlink() or not source.is_file():
            raise SmokeError(f"package manifest does not name a regular file: {source}")
        data = source.read_bytes()
        if entry["bytes"] != len(data) or entry["sha256"] != sha256(data):
            raise SmokeError(f"package manifest size or hash mismatch: {source}")
        files[name] = data
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
    parser.add_argument("--bin", type=Path, help="directory containing VirtualCapture.exe and vc-updater.exe")
    parser.add_argument("--native", type=Path, help="directory containing vc_stream.dll and vc_vcam.dll")
    parser.add_argument("--capture-desktop", action="store_true",
                        help="also verify the peer OTA restores its native capture window")
    args = parser.parse_args()

    RUN.mkdir(parents=True)
    host_root = RUN / "host"
    peer_root = RUN / "peer"
    host_root.mkdir()
    peer_root.mkdir()
    # The saved desktop choice must not override the Host OTA's receive role.
    (host_root / "vc-startup.json").write_text(
        json.dumps({"schema": 1, "role": "capture"}), encoding="utf-8")
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
                APP_EXE: (bin_dir / APP_EXE).read_bytes(),
                UPDATER_EXE: (bin_dir / UPDATER_EXE).read_bytes(),
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
            APP_EXE: (bin_dir / APP_EXE).read_bytes(),
            UPDATER_EXE: (bin_dir / UPDATER_EXE).read_bytes(),
            "vc_stream.dll": (native_dir / "vc_stream.dll").read_bytes(),
            "version.json": version_json(peer_version),
        }
        peer_files.update(native_runtime_files(native_dir))
        make_package(peer_zip, peer_build, peer_version, peer_files)

        port = free_port()
        capture_args = [str(bin_dir / APP_EXE), "--root", str(peer_root), "--native-dir", str(native_dir)]
        if args.capture_desktop:
            (peer_root / "vc-startup.json").write_text(
                json.dumps({"schema": 1, "role": "capture"}), encoding="utf-8")
            capture_args.extend(["--capture-bind", "127.0.0.1", "--capture-port", str(port)])
        else:
            capture_args.extend(["capture", "--bind", "127.0.0.1", "--port", str(port)])
        capture = subprocess.Popen(capture_args, cwd=peer_root, stdout=log, stderr=log,
                                   creationflags=CREATE_NO_WINDOW)
        pids.add(capture.pid)
        health = wait_for(lambda: capture_health(port), 20, "private capture health")
        assert health["api_revision"] == 2 and health["layout"] == "legacy" and health["build"] is None, health
        assert health["pid"] == capture.pid and Path(health["root"]).samefile(peer_root), health
        first_pid = capture.pid
        if args.capture_desktop:
            peer_windows = wait_for(
                lambda: [(hwnd, title) for hwnd, title in windows_for_pid(first_pid)
                         if title == "VirtualCapture · 采集端"] or None,
                30, "original desktop capture window")
            window_screenshot(peer_windows[0][0], "desktop-capture-before-ota.png")
        token_path = wait_for(
            lambda: (peer_root / "update-token.json") if (peer_root / "update-token.json").is_file() else None,
            15, "capture update token")
        peer_token = read_json(token_path)["token"]
        assert len(peer_token) == 64 and all(ch in "0123456789abcdefABCDEF" for ch in peer_token)
        checks.append("private unified capture started on a random port with its own token")

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
        assert peer_after["api_revision"] == 2 and peer_after["layout"] == "managed", peer_after
        assert Path(peer_after["root"]).samefile(peer_root), peer_after
        pids.add(peer_after["pid"])
        assert capture.wait(timeout=5) == 0, "original capture did not shut down gracefully"
        pids.discard(first_pid)
        expect_release(peer_root, peer_build, ("vc_stream.dll",))
        assert any(entry.get("build_id") == peer_build and entry.get("state") == "active"
                   for entry in history(peer_root)), history(peer_root)
        checks.append("peer capture exited cleanly; helper started the unified managed release with vc_stream.dll")
        if args.capture_desktop:
            peer_jobs = list((peer_root / "updates/worker").glob("job-*.json"))
            assert len(peer_jobs) == 1, peer_jobs
            peer_job = read_json(peer_jobs[0])
            assert peer_job["role"] == "capture" and peer_job["capture_desktop"] is True, peer_job
            assert peer_job["start_role"] is None and peer_job["bind"] == "127.0.0.1" and peer_job["port"] == port, peer_job
            peer_windows = wait_for(
                lambda: [(hwnd, title) for hwnd, title in windows_for_pid(peer_after["pid"])
                         if title == "VirtualCapture · 采集端"] or None,
                30, "OTA restored desktop capture window")
            window_screenshot(peer_windows[0][0], "desktop-capture-after-ota.png")
            assert read_json(peer_root / "vc-startup.json") == {"schema": 1, "role": "capture"}
            close_host(peer_after["pid"], peer_windows, "desktop capture after OTA")
            pids.discard(peer_after["pid"])
            assert capture_health(port) is None, "OTA desktop capture listener survived close"
            checks.append("desktop capture OTA restored its window and original loopback bind/port; closed with exit 0")

        jobs = list((host_root / "updates/worker").glob("job-*.json"))
        assert len(jobs) == 1, jobs
        job = read_json(jobs[0])
        assert job["schema"] == 1 and job["role"] == "host" and job["target"] == local_build, job
        assert job["start_role"] == "receive" and job["capture_desktop"] is False, job
        assert job["parent_pid"] == fixture.pid and Path(job["root"]).samefile(host_root), job
        pids.discard(fixture.pid)
        ready_path = Path(job["ready_path"])
        ready = wait_for(lambda: pending_json(ready_path), 120, "new GPUI host ready marker")
        assert ready["schema"] == 1 and ready["build"] == local_build, ready
        assert Path(ready["root"]).samefile(host_root), ready
        host_release = expect_release(host_root, local_build, ("vc_stream.dll", "vc_vcam.dll"))
        receipt = wait_launch_receipt(host_root, host_release / APP_EXE)
        assert receipt["pid"] == ready["pid"], (receipt, ready)
        host_pid = receipt["pid"]
        pids.add(host_pid)
        checks.append("unified GPUI first-frame marker matches the installed build and exact launch receipt")

        windows = wait_for(lambda: windows_for_pid(host_pid) or None, 30, "new GPUI window")
        titles = [title for _hwnd, title in windows]
        assert any("VirtualCapture" in title and "接收端" in title for title in titles), windows
        wait_for(lambda: any(entry.get("build_id") == local_build and entry.get("state") == "active"
                             for entry in history(host_root)), 30, "helper confirmed first frame")
        close_host(host_pid, windows, "activated GPUI host")
        pids.discard(host_pid)
        checks.append(f"new GPUI window(s) {titles!r} closed with WM_CLOSE; process exited 0")

        helper_logs = list((host_root / "updates/worker").glob("updater-*.log"))
        wait_for(lambda: any(f"active {local_build}" in path.read_text(encoding="utf-8", errors="replace")
                             for path in helper_logs), 30, "host helper active log")
        assert any(entry.get("build_id") == local_build and entry.get("state") == "active"
                   for entry in history(host_root)), history(host_root)
        checks.append("host helper log/history confirm active local build")

        entry = host_root / APP_EXE
        assert entry.read_bytes() == local_files[APP_EXE], "new host did not install ROOT entry"
        receipts_before = set((host_root / "data/run").glob("host-*.launch.json"))
        capture_port = free_port()
        launcher = subprocess.Popen(
            [str(entry), "--capture-bind", "127.0.0.1", "--capture-port", str(capture_port)],
            cwd=RUN, stdout=log, stderr=log)
        pids.add(launcher.pid)
        assert launcher.wait(timeout=20) == 0, "ROOT entry could not forward to the updated host"
        pids.discard(launcher.pid)
        reopened_receipt = wait_launch_receipt(host_root, host_release / APP_EXE, receipts_before)
        reopened_pid = reopened_receipt["pid"]
        assert reopened_pid != launcher.pid, reopened_receipt
        pids.add(reopened_pid)
        reopened = wait_for(lambda: windows_for_pid(reopened_pid) or None, 30, "updated host reopened from ROOT EXE")
        assert any("VirtualCapture" in title and "采集端" in title for _hwnd, title in reopened), reopened
        capture_reopened = wait_for(
            lambda: capture_health(capture_port), 20, "saved capture role on forwarded port")
        assert capture_reopened["pid"] == reopened_pid, capture_reopened
        close_host(reopened_pid, reopened, "GPUI capture reopened from ROOT entry")
        pids.discard(reopened_pid)
        assert capture_health(capture_port) is None, "capture listener survived window close"
        checks.append("ROOT entry reopened saved capture role on custom loopback port; exited 0 after WM_CLOSE")

        # Seed only the previous choice; the new capture preference must come from a real click.
        preference_path = host_root / "data/vc-startup.json"
        receive_preference = {"schema": 1, "role": "receive"}
        preference_path.write_text(json.dumps(receive_preference), encoding="utf-8")
        config_before_reselect = (host_root / "data/vc-rust-config.json").read_bytes()
        receipts_before = set((host_root / "data/run").glob("host-*.launch.json"))
        select_port = free_port()
        launcher = subprocess.Popen(
            [str(entry), "--select-role", "--capture-bind", "127.0.0.1",
             "--capture-port", str(select_port)], cwd=RUN, stdout=log, stderr=log)
        pids.add(launcher.pid)
        assert launcher.wait(timeout=20) == 0, "ROOT entry could not forward role selection"
        pids.discard(launcher.pid)
        select_receipt = wait_launch_receipt(host_root, host_release / APP_EXE, receipts_before)
        select_pid = select_receipt["pid"]
        pids.add(select_pid)
        chooser = wait_for(
            lambda: [(hwnd, title) for hwnd, title in windows_for_pid(select_pid)
                     if title == "VirtualCapture · 选择本机角色"] or None,
            30, "Managed ROOT role chooser")
        assert read_json(preference_path) == receive_preference
        assert capture_health(select_port) is None, "capture started before a role was selected"
        choose_managed_capture(chooser[0][0])
        selected_capture = wait_for(
            lambda: [(hwnd, title) for hwnd, title in windows_for_pid(select_pid)
                     if title == "VirtualCapture · 采集端"] or None,
            20, "capture role after Managed chooser click")
        wait_for(lambda: pending_json(preference_path) == {"schema": 1, "role": "capture"},
                 10, "Managed preference persisted by the chooser")
        selected_health = wait_for(lambda: capture_health(select_port), 20,
                                   "capture listening on the forwarded chooser port")
        assert selected_health["pid"] == select_pid, selected_health
        assert selected_health["build"] == local_build and selected_health["layout"] == "managed", selected_health
        assert Path(selected_health["root"]).samefile(host_root), selected_health
        assert (host_root / "data/vc-rust-config.json").read_bytes() == config_before_reselect
        window_screenshot(selected_capture[0][0], "managed-selected-capture.png")
        close_host(select_pid, selected_capture, "capture selected through Managed ROOT")
        pids.discard(select_pid)
        assert capture_health(select_port) is None, "selected capture listener survived window close"
        checks.append("ROOT --select-role forwarded chooser and custom loopback port; real click saved data/vc-startup.json")

        peer_token_file = host_root / "data/update-peer-token.json"
        assert read_json(peer_token_file)["token"] == peer_token, peer_token_file
        assert read_json(peer_root / "data/update-token.json")["token"] == peer_token
        host_config = read_json(host_root / "data/vc-rust-config.json")
        assert host_config["peer_url"] == f"http://127.0.0.1:{port}", host_config
        checks.append("pairing token, peer token and config survived flat -> managed migration")

        dshow_after = dshow_registration()
        assert dshow_before == dshow_after, (dshow_before, dshow_after)
        checks.append("no DShow registration change")

        report = {
            "ok": True,
            "scope": "HostController -> real vc-updater -> new GPUI first frame; peer capture restarted via real helper",
            "run": str(RUN),
            "builds": {"peer": peer_build, "local": local_build},
            "peer_capture_desktop": args.capture_desktop,
            "screenshots": ["managed-role-chooser.png", "managed-selected-capture.png"] + (
                ["desktop-capture-before-ota.png", "desktop-capture-after-ota.png"]
                if args.capture_desktop else []),
            "checks": checks,
            "limits": [
                "private temporary roots; random loopback control port",
                "no DShow registration, no NIC capture, no PC2 connection",
                "peer package is synthetic; under --package the local update uses the verified candidate payload and unchanged manifest",
                "flat data layout migration does not exercise an old split-binary release",
                "SHA-256 manifest verification is not a signature check",
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
