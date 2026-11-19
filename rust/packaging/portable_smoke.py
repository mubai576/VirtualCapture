"""Validate extracted Rust release files and launchers without registering a camera.

The payload intentionally has no C++ sender/receiver.  This smoke checks the
Rust DLL ABI/capability contract (raw/pyrowave, sources, SHM v3), the OTA
manifest layout, the Rust DirectShow DLL through the isolated verifier,
launchers, app-local runtime, managed-layout resolution and normal shutdown.
"""
import argparse
import ctypes as c
from ctypes import wintypes as w
import importlib.util
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import subprocess
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[2]


def load_inventory():
    path = ROOT / "rust/packaging/inventory.py"
    spec = importlib.util.spec_from_file_location("vc_packaging_inventory", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


inventory = load_inventory()

USER = c.windll.user32
KERNEL = c.windll.kernel32
PSAPI = c.windll.psapi
USER.GetWindowThreadProcessId.argtypes = [w.HWND, c.POINTER(w.DWORD)]
USER.GetWindowTextW.argtypes = [w.HWND, w.LPWSTR, c.c_int]
USER.PostMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
KERNEL.OpenProcess.argtypes = [w.DWORD, w.BOOL, w.DWORD]
KERNEL.OpenProcess.restype = w.HANDLE
KERNEL.CloseHandle.argtypes = [w.HANDLE]
KERNEL.GetExitCodeProcess.argtypes = [w.HANDLE, c.POINTER(w.DWORD)]
PSAPI.EnumProcessModulesEx.argtypes = [w.HANDLE, c.POINTER(c.c_void_p), w.DWORD, c.POINTER(w.DWORD), w.DWORD]
PSAPI.GetModuleFileNameExW.argtypes = [w.HANDLE, c.c_void_p, w.LPWSTR, w.DWORD]
CALLBACK = c.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)


class VcStreamApiV1(c.Structure):
    _fields_ = [
        ("abi_version", w.UINT),
        ("struct_size", w.UINT),
        ("list_adapters", c.c_void_p),
        ("start_session", c.c_void_p),
        ("session_status", c.c_void_p),
        ("stop_session", c.c_void_p),
        ("destroy_session", c.c_void_p),
        ("capabilities", c.c_void_p),
    ]


def validate_payload(package):
    missing = [name for name in inventory.REQUIRED_FILES if not (package / name).is_file()]
    assert not missing, f"Missing payload files: {missing}"
    assert not (package / "licenses").exists(), "License audit materials must not be distributed"
    present = {p.name.lower() for p in package.rglob("*") if p.is_file()}
    forbidden = sorted(present & inventory.FORBIDDEN_NAMES)
    assert not forbidden, f"Legacy C++ production binaries in payload: {forbidden}"


def validate_manifest(package):
    """Cross-check the OTA manifest against the actual Rust runtime layout."""
    manifest = json.loads((package / "package-manifest.json").read_text(encoding="utf-8"))
    assert manifest["product"] == "VirtualCapture-Rust-portable", manifest["product"]
    assert manifest["schema"] == 1, manifest["schema"]
    assert manifest["default_codec"] == inventory.CODECS[0], manifest
    assert set(manifest["codecs"]) == set(inventory.CODECS), manifest
    modules = manifest["runtime_modules"]
    assert set(modules["in_process_dlls"]) == set(inventory.RUST_DLLS), modules
    assert modules["codec_dlls"] == inventory.CODEC_DLLS, modules
    assert set(modules["codecs"]) == set(inventory.CODECS), modules
    assert set(modules["sources"]) == set(inventory.SOURCES), modules
    assert modules["stream_abi"] == inventory.STREAM_ABI, modules
    declared = {entry["path"] for entry in manifest["files"]}
    missing = sorted(set(inventory.REQUIRED_FILES) - declared)
    assert not missing, f"Manifest does not declare payload files: {missing}"


def validate_stream_capabilities(caps):
    """Mirror vc-core::process::validate_capabilities for the packaged DLL."""
    assert caps["stream_revision"] == 2, caps
    assert caps["shm_version"] == 3, caps
    assert caps["variable_resolution"] is True, caps
    assert caps["pixel_format"] == "nv12", caps
    assert caps["sendqueue"] == "frame", caps
    assert set(caps["codecs"]) == set(inventory.CODECS), caps
    assert set(caps["sources"]) == set(inventory.SOURCES), caps


def stream_capabilities(package):
    """Exercise the versioned vc_stream.dll ABI in this process."""
    dll_path = package / "vc_stream.dll"
    directory = os.add_dll_directory(str(package))
    try:
        library = c.WinDLL(str(dll_path))
        library.vc_module_get_api.argtypes = [w.UINT]
        library.vc_module_get_api.restype = c.c_void_p
        address = library.vc_module_get_api(1)
        assert address, "vc_module_get_api(1) returned NULL"
        assert not library.vc_module_get_api(2), "vc_stream.dll accepted an unknown ABI version"
        api = c.cast(address, c.POINTER(VcStreamApiV1)).contents
        assert api.abi_version == 1, api.abi_version
        assert api.struct_size >= c.sizeof(VcStreamApiV1), api.struct_size
        query = c.WINFUNCTYPE(
            c.c_int, c.c_char_p, w.UINT, c.POINTER(w.UINT),
            c.c_char_p, w.UINT, c.POINTER(w.UINT))(api.capabilities)
        output = c.create_string_buffer(4096)
        written = w.UINT()
        error = c.create_string_buffer(1024)
        error_written = w.UINT()
        code = query(output, len(output), c.byref(written),
                     error, len(error), c.byref(error_written))
        assert code == 0, (code, error.value, error_written.value)
        return json.loads(output.value)
    finally:
        directory.close()


def modules(pid, package):
    handle = KERNEL.OpenProcess(0x410, False, pid)
    assert handle, f"Cannot inspect PID {pid}"
    try:
        values = (c.c_void_p * 2048)()
        needed = w.DWORD()
        assert PSAPI.EnumProcessModulesEx(handle, values, c.sizeof(values), c.byref(needed), 3)
        assert needed.value <= c.sizeof(values)
        result = {}
        for value in values[:needed.value // c.sizeof(c.c_void_p)]:
            buffer = c.create_unicode_buffer(32768)
            assert PSAPI.GetModuleFileNameExW(handle, value, buffer, len(buffer))
            path = Path(buffer.value)
            if path.name.lower().startswith(("vcruntime140", "msvcp140")):
                assert path.parent.samefile(package), f"Runtime loaded outside package: {path}"
                result[path.name] = str(path)
        assert any(name.lower() == "vcruntime140.dll" for name in result), result
        return result
    finally:
        KERNEL.CloseHandle(handle)


def find_window(pid):
    result = []
    @CALLBACK
    def visit(hwnd, _):
        owner = w.DWORD()
        USER.GetWindowThreadProcessId(hwnd, c.byref(owner))
        title = c.create_unicode_buffer(512)
        USER.GetWindowTextW(hwnd, title, len(title))
        if owner.value == pid and title.value == "VirtualCapture" and USER.IsWindowVisible(hwnd):
            result.append(hwnd)
        return True
    USER.EnumWindows(visit, 0)
    return result[0] if result else None


def host_pid(root):
    data = root / "data" if (root / "app/current.json").is_file() else root
    receipts = list((data / "run").glob("host-*.launch.json"))
    assert receipts, f"No graphical launcher receipt in {root}"
    receipt = json.loads(max(receipts, key=lambda path: path.stat().st_mtime_ns).read_text(encoding="utf-8"))
    assert Path(receipt["root"]).samefile(root), receipt
    return int(receipt["pid"])


def cmdline(package, launcher, arguments):
    return f'"{os.environ["COMSPEC"]}" /d /s /c ""{package / launcher}" {arguments}"'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--report-dir", type=Path, required=True)
    args = parser.parse_args()
    package = args.package.resolve()
    evidence = args.report_dir.resolve()
    evidence.mkdir(parents=True, exist_ok=False)
    validate_payload(package)
    validate_manifest(package)
    if not KERNEL.GetConsoleWindow():
        KERNEL.AllocConsole()
        KERNEL.GetConsoleWindow.restype = w.HWND
        USER.ShowWindow.argtypes = [w.HWND, c.c_int]
        USER.ShowWindow(KERNEL.GetConsoleWindow(), 0)
    env = dict(os.environ)
    for key in ("VC_ROOT", "VC_NATIVE_DIR", "CARGO_HOME", "RUSTUP_HOME", "PYTHONPATH"):
        env.pop(key, None)
    env["PATH"] = os.pathsep.join([str(Path(os.environ["SystemRoot"]) / "System32"), os.environ["SystemRoot"]])
    def execute(argv, timeout=30):
        command = argv if isinstance(argv, str) else [str(a) for a in argv]
        reply = subprocess.run(command, cwd=package, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, encoding="utf-8", errors="replace", timeout=timeout)
        assert reply.returncode == 0, (argv, reply.returncode, reply.stdout)
        return reply.stdout
    # Keep a pristine payload for a second launch through current.json.
    # Only the graphical entry stays at ROOT; no CMD/PowerShell is used for the GUI.
    managed = evidence / "版本化 root"
    managed_id = json.loads((package / "package-manifest.json").read_text(encoding="utf-8"))["build_id"]
    managed_release = managed / "app/releases" / managed_id
    shutil.copytree(package, managed_release)
    shutil.copytree(package / "scripts", managed / "scripts")
    for launcher in ("VirtualCapture.exe", "Start-Host.cmd", "Start-Capture.cmd"):
        shutil.copy2(package / launcher, managed / launcher)
    package_version = json.loads((package / "version.json").read_text(encoding="utf-8"))["version"]
    (managed / "app/current.json").write_text(json.dumps({"schema": 1, "build_id": managed_id,
        "version": package_version, "generation": 1, "activated_utc": "smoke"}), encoding="utf-8")
    (managed / "data").mkdir()
    (managed / "data/vc-rust-config.json").write_text(json.dumps({"version": 1,
        "peer_url": "", "capture": {"fps": 120}}), encoding="utf-8")

    stream_caps = stream_capabilities(package)
    validate_stream_capabilities(stream_caps)
    # Diagnostic-only probe: --capabilities loads the DLL and reads one export,
    # never DllRegisterServer or a filter instance.
    dll_caps = json.loads(execute([package / "vc-vcam-verify.exe", "--dll", package / "vc_vcam.dll", "--capabilities"]))
    assert dll_caps["stream_revision"] == 2 and dll_caps["shm_version"] == 3
    assert dll_caps["variable_resolution"] is True and dll_caps["max_dimension"] == 8192, dll_caps
    config = json.loads(execute([package / "vc-capture.exe", "--root", evidence, "--native-dir", package, "--print-config"]))
    # Raw remains the default; encoded codecs require explicit selection.
    assert config["capture"]["codec"] == inventory.CODECS[0], config
    import pefile
    for name in ("VirtualCapture.exe", "vc-host.exe"):
        pe = pefile.PE(str(package / name))
        try:
            assert pe.OPTIONAL_HEADER.Subsystem == 2, f"{name} must use the Windows GUI subsystem"
            resource_types = {entry.id for entry in pe.DIRECTORY_ENTRY_RESOURCE.entries}
            assert {3, 14} <= resource_types, f"{name} has no native application icon"
        finally:
            pe.close()
    powershell = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    parse = r"$bad=$false; Get-ChildItem scripts/*.ps1 | ForEach-Object { $t=$null; $e=$null; [System.Management.Automation.Language.Parser]::ParseFile($_.FullName,[ref]$t,[ref]$e)|Out-Null; if($e){$e|Format-List;$bad=$true} }; if($bad){exit 1}"
    execute([powershell, "-NoProfile", "-NonInteractive", "-Command", parse])
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    capture_root = package
    capture_log = (evidence / "capture-launcher.log").open("wb")
    capture = subprocess.Popen(cmdline(package, "Start-Capture.cmd", f'--bind 127.0.0.1 --port {port}'),
                               cwd=evidence, env=env, stdin=subprocess.PIPE, stdout=capture_log, stderr=capture_log,
                               creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
    host_handle = None
    capture_handle = None
    hwnd = None
    try:
        deadline = time.monotonic() + 20
        while True:
            assert capture.poll() is None, "Capture launcher exited early"
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1) as response:
                    health = json.load(response)
                break
            except (OSError, urllib.error.URLError):
                assert time.monotonic() < deadline, "Capture launcher health timeout"
                time.sleep(.1)
        assert health["ok"] and health["implementation"] == "virtualcapture-rust" and health["role"] == "capture"
        assert health["version"] == package_version and not health["running"]
        assert Path(health["root"]).samefile(capture_root), health
        capture_handle = KERNEL.OpenProcess(0x101000, False, health["pid"])
        assert capture_handle
        capture_modules = modules(health["pid"], package)
        host_root = evidence / "主机 root"
        host_launch_log = evidence / "host-launcher.log"
        before = set(host_root.glob("run/host-*.stderr.log"))
        with host_launch_log.open("wb") as output:
            result = subprocess.run([package / "VirtualCapture.exe", "--root", host_root], cwd=evidence,
                                    env=env, stdout=output, stderr=output, timeout=20)
        assert result.returncode == 0, host_launch_log.read_bytes()
        # The graphical launcher reports no console output; its private receipt
        # identifies the actual GUI PID and ROOT without consulting a shell.
        pid = host_pid(host_root)
        assert set(host_root.glob("run/host-*.stderr.log")) - before
        host_handle = KERNEL.OpenProcess(0x101000, False, pid)
        assert host_handle
        deadline = time.monotonic() + 15
        while not hwnd:
            hwnd = find_window(pid)
            if time.monotonic() >= deadline:
                # Include hidden windows in cleanup if launching the GUI regresses.
                @CALLBACK
                def close_owned(window, _):
                    owner = w.DWORD()
                    USER.GetWindowThreadProcessId(window, c.byref(owner))
                    if owner.value == pid:
                        USER.PostMessageW(window, 0x0010, 0, 0)
                    return True
                USER.EnumWindows(close_owned, 0)
                raise AssertionError("Host launcher window timeout")
            time.sleep(.1)
        host_modules = modules(pid, package)
        assert USER.PostMessageW(hwnd, 0x0010, 0, 0)
        deadline = time.monotonic() + 15
        while True:
            code = w.DWORD()
            assert KERNEL.GetExitCodeProcess(host_handle, c.byref(code))
            if code.value != 259:
                assert code.value == 0, code.value
                break
            assert time.monotonic() < deadline, "Host did not close normally"
            time.sleep(.1)
        capture.send_signal(signal.CTRL_BREAK_EVENT)
        # CMD asks whether to terminate a batch after Ctrl+Break; answer only on
        # this private launcher's stdin, never through global keyboard input.
        try:
            capture.communicate(input=b"Y\r\n", timeout=10)
        except subprocess.TimeoutExpired:
            raise AssertionError("Capture launcher did not finish after console break")
        code = w.DWORD()
        deadline = time.monotonic() + 5
        while True:
            assert KERNEL.GetExitCodeProcess(capture_handle, c.byref(code))
            if code.value != 259:
                assert code.value == 0, f"Capture shutdown exit={code.value}"
                break
            assert time.monotonic() < deadline, "Capture did not stop with its launcher"
            time.sleep(.1)
        managed_config = json.loads(execute(cmdline(managed, "Start-Capture.cmd", "--print-config")))
        assert managed_config["capture"]["fps"] == 120, managed_config
        managed_log = evidence / "managed-host-launcher.log"
        with managed_log.open("wb") as output:
            result = subprocess.run([managed / "VirtualCapture.exe"], cwd=evidence,
                                    env=env, stdout=output, stderr=output, timeout=20)
        assert result.returncode == 0, managed_log.read_bytes()
        managed_pid = host_pid(managed)
        # Release resolution must also govern the DLL loader, including Unicode paths.
        managed_handle = KERNEL.OpenProcess(0x101000, False, managed_pid)
        assert managed_handle
        managed_window = None
        try:
            deadline = time.monotonic() + 15
            while not managed_window:
                managed_window = find_window(managed_pid)
                assert time.monotonic() < deadline, "Managed launcher window timeout"
                time.sleep(.1)
            managed_modules = modules(managed_pid, managed_release)
            assert USER.PostMessageW(managed_window, 0x0010, 0, 0)
            while True:
                exit_code = w.DWORD()
                assert KERNEL.GetExitCodeProcess(managed_handle, c.byref(exit_code))
                if exit_code.value != 259:
                    assert exit_code.value == 0, exit_code.value
                    break
                assert time.monotonic() < deadline, "Managed host close timeout"
                time.sleep(.1)
        finally:
            if managed_window:
                USER.PostMessageW(managed_window, 0x0010, 0, 0)
            KERNEL.CloseHandle(managed_handle)
        report = {"ok": True, "package": str(package), "health": health,
                  "managed_launcher": {"root": str(managed), "config_fps": 120,
                                       "runtime_modules": managed_modules},
                  "app_local_runtime_modules": {"capture": capture_modules, "host": host_modules},
                  "stream_dll_capabilities": stream_caps,
                  "dll_capabilities": dll_caps,
                  "checks": ["payload inventory, OTA manifest and no legacy C++ production binaries",
                             "sanitized PATH without developer tools",
                             "vc_stream.dll ABI, codecs/sources/SHM capability contract in-process",
                             "actual Rust DShow DLL capability probe without registration",
                             "PowerShell 5.1 parser",
                             "capture CMD launcher, health and exit zero on console break",
                             "graphical EXE entry without CMD/PowerShell and normal close",
                             "Chinese and space paths", "actual app-local MSVC runtime loads",
                             "managed current.json launchers and data configuration"]}
        (evidence / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps(report, ensure_ascii=False), flush=True)
    finally:
        if hwnd:
            USER.PostMessageW(hwnd, 0x0010, 0, 0)
        if host_handle:
            KERNEL.CloseHandle(host_handle)
        if capture_handle:
            KERNEL.CloseHandle(capture_handle)
        if capture.poll() is None:
            capture.send_signal(signal.CTRL_BREAK_EVENT)
            try:
                capture.wait(timeout=10)
            except subprocess.TimeoutExpired:
                subprocess.run(["taskkill", "/PID", str(capture.pid), "/T", "/F"], stdout=subprocess.DEVNULL)
                capture.wait(timeout=5)
        capture_log.close()


if __name__ == "__main__":
    main()
