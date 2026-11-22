"""Validate the extracted unified Windows package without registering a camera.

Verify portable and managed launches, app-local runtime loading, the same-process
stream ABI and a bounded subprocess probe of the actual DirectShow DLL export.
The probe is not an isolated COM graph test or a registration test.
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
USER.IsWindowVisible.argtypes = [w.HWND]
USER.PostMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
USER.ShowWindow.argtypes = [w.HWND, c.c_int]
KERNEL.GetConsoleWindow.restype = w.HWND
KERNEL.OpenProcess.argtypes = [w.DWORD, w.BOOL, w.DWORD]
KERNEL.OpenProcess.restype = w.HANDLE
KERNEL.CloseHandle.argtypes = [w.HANDLE]
KERNEL.GetExitCodeProcess.argtypes = [w.HANDLE, c.POINTER(w.DWORD)]
KERNEL.QueryFullProcessImageNameW.argtypes = [w.HANDLE, w.DWORD, w.LPWSTR, c.POINTER(w.DWORD)]
KERNEL.WaitForSingleObject.argtypes = [w.HANDLE, w.DWORD]
KERNEL.WaitForSingleObject.restype = w.DWORD
KERNEL.TerminateProcess.argtypes = [w.HANDLE, w.UINT]
PSAPI.EnumProcessModulesEx.argtypes = [w.HANDLE, c.POINTER(c.c_void_p), w.DWORD, c.POINTER(w.DWORD), w.DWORD]
PSAPI.GetModuleFileNameExW.argtypes = [w.HANDLE, c.c_void_p, w.LPWSTR, w.DWORD]
CALLBACK = c.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)
USER.EnumWindows.argtypes = [CALLBACK, w.LPARAM]


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


def validate_manifest(package):
    """Cross-check the OTA manifest against the actual unified payload."""
    actual = inventory.validate_payload_directory(package, with_manifest=True)
    manifest = json.loads((package / inventory.MANIFEST_NAME).read_text(encoding="utf-8"))
    assert manifest["product"] == "VirtualCapture-Rust-portable", manifest["product"]
    assert manifest["schema"] == 1, manifest["schema"]
    assert manifest["entrypoint"] == inventory.ENTRYPOINT, manifest["entrypoint"]
    assert manifest["default_codec"] == inventory.CODECS[0], manifest
    assert set(manifest["codecs"]) == set(inventory.CODECS), manifest
    modules = manifest["runtime_modules"]
    assert set(modules["in_process_dlls"]) == set(inventory.RUST_DLLS), modules
    assert modules["diagnostic_commands"] == [list(inventory.VCAM_PROBE_COMMAND)], modules
    assert "diagnostic_exes" not in modules, modules
    assert modules["codec_dlls"] == inventory.CODEC_DLLS, modules
    assert set(modules["codecs"]) == set(inventory.CODECS), modules
    assert set(modules["sources"]) == set(inventory.SOURCES), modules
    assert modules["stream_abi"] == inventory.STREAM_ABI, modules
    declared = inventory.validate_payload_paths(entry["path"] for entry in manifest["files"])
    assert actual == declared | {inventory.MANIFEST_NAME}, "Extracted files do not match the manifest"
    return manifest


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


def modules(pid, release):
    handle = KERNEL.OpenProcess(0x410, False, pid)  # QUERY_INFORMATION | VM_READ
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
                assert path.parent.samefile(release), f"Runtime loaded outside release: {path}"
                result[path.name] = str(path)
        assert any(name.lower() == "vcruntime140.dll" for name in result), result
        return result
    finally:
        KERNEL.CloseHandle(handle)


def open_process(pid, expected_exe):
    # SYNCHRONIZE | QUERY_LIMITED_INFORMATION | TERMINATE (test-only cleanup).
    handle = KERNEL.OpenProcess(0x101001, False, pid)
    assert handle, f"Cannot open test process PID {pid}"
    image = c.create_unicode_buffer(32768)
    length = w.DWORD(len(image))
    try:
        assert KERNEL.QueryFullProcessImageNameW(handle, 0, image, c.byref(length)), pid
        assert Path(image.value).samefile(expected_exe), (pid, image.value, expected_exe)
    except BaseException:
        KERNEL.CloseHandle(handle)
        raise
    return handle


def require_normal_exit(handle, timeout, label):
    assert KERNEL.WaitForSingleObject(handle, timeout * 1000) == 0, f"{label} did not exit"
    code = w.DWORD()
    assert KERNEL.GetExitCodeProcess(handle, c.byref(code)), label
    assert code.value == 0, f"{label} exit={code.value}"


def cleanup_process(proc, handle):
    if handle:
        if KERNEL.WaitForSingleObject(handle, 1000) == 258:  # WAIT_TIMEOUT
            KERNEL.TerminateProcess(handle, 1)
            KERNEL.WaitForSingleObject(handle, 5000)
        KERNEL.CloseHandle(handle)
    if proc.poll() is None:
        proc.kill()
        proc.wait(timeout=5)


def owned_windows(pid):
    result = []

    @CALLBACK
    def visit(hwnd, _):
        owner = w.DWORD()
        USER.GetWindowThreadProcessId(hwnd, c.byref(owner))
        if owner.value == pid:
            result.append(hwnd)
        return True

    USER.EnumWindows(visit, 0)
    return result


def find_window(pid):
    for hwnd in owned_windows(pid):
        title = c.create_unicode_buffer(512)
        USER.GetWindowTextW(hwnd, title, len(title))
        if title.value in {
            "VirtualCapture · 选择本机角色",
            "VirtualCapture · 接收端",
            "VirtualCapture · 采集端",
        } and USER.IsWindowVisible(hwnd):
            return hwnd
    return None


def smoke_host(argv, root, release, evidence, env, *, forwarded=False):
    run_dir = root / ("data/run" if forwarded else "run")
    assert not list(run_dir.glob("host-*.launch.json")), run_dir
    log = evidence / ("managed-host.log" if forwarded else "host.log")
    with log.open("wb") as output:
        host = subprocess.Popen(argv, cwd=evidence, env=env, stdout=output, stderr=output)
        pid = None if forwarded else host.pid
        handle = None
        try:
            if forwarded:
                assert host.wait(timeout=20) == 0, log
            deadline = time.monotonic() + 15
            receipt = None
            while receipt is None:
                if not forwarded:
                    assert host.poll() is None, f"Direct host exited early; see {log}"
                receipts = list(run_dir.glob("host-*.launch.json"))
                assert len(receipts) <= 1, receipts
                if receipts:
                    try:
                        receipt = json.loads(receipts[0].read_text(encoding="utf-8"))
                        break
                    except json.JSONDecodeError:
                        pass  # The process may still be writing the receipt.
                assert time.monotonic() < deadline, f"No host launch receipt in {run_dir}"
                time.sleep(.1)
            pid = int(receipt["pid"])
            assert (pid != host.pid) == forwarded, receipt
            exe = release / inventory.APP_EXE
            handle = open_process(pid, exe)
            assert Path(receipt["root"]).samefile(root) and Path(receipt["exe"]).samefile(exe), receipt
            tag = receipts[0].name.removesuffix(".launch.json")
            for channel in ("stdout", "stderr"):
                assert Path(receipt[channel]).samefile(run_dir / f"{tag}.{channel}.log"), receipt
            hwnd = find_window(pid)
            while hwnd is None:
                assert time.monotonic() < deadline, f"Host PID {pid} did not open a visible window; see {log}"
                time.sleep(.1)
                hwnd = find_window(pid)
            runtime = modules(pid, release)
            assert USER.PostMessageW(hwnd, 0x0010, 0, 0)  # WM_CLOSE
            require_normal_exit(handle, 15, "Host")
            return {"entry_pid": host.pid, "process": receipt, "runtime_modules": runtime}
        finally:
            if handle and KERNEL.WaitForSingleObject(handle, 0) == 258:
                for hwnd in owned_windows(pid):
                    USER.PostMessageW(hwnd, 0x0010, 0, 0)
            cleanup_process(host, handle)


def smoke_capture(argv, root, release, manifest, evidence, env, *, label, managed=False):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    log = evidence / f"{label}.log"
    with log.open("wb") as output:
        capture = subprocess.Popen([*argv, "--bind", "127.0.0.1", "--port", str(port)],
                                   cwd=evidence, env=env, stdout=output, stderr=output,
                                   creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
        handle = None
        try:
            deadline = time.monotonic() + 20
            while True:
                assert capture.poll() is None, f"Capture entry exited early; see {log}"
                try:
                    with opener.open(f"http://127.0.0.1:{port}/health", timeout=1) as response:
                        health = json.load(response)
                    break
                except (OSError, urllib.error.URLError):
                    assert time.monotonic() < deadline, f"Capture health timeout; see {log}"
                    time.sleep(.1)
            assert health["ok"] and health["implementation"] == "virtualcapture-rust", health
            assert health["role"] == "capture" and not health["running"], health
            assert health["version"] == manifest["version"] and Path(health["root"]).samefile(root), health
            assert health["layout"] == ("managed" if managed else "legacy"), health
            assert health["build"] == (manifest["build_id"] if managed else None), health
            pid = health["pid"]
            assert (pid != capture.pid) == managed, health
            handle = open_process(pid, release / inventory.APP_EXE)
            runtime = modules(pid, release)
            capture.send_signal(signal.CTRL_BREAK_EVENT)
            assert capture.wait(timeout=10) == 0, f"Capture entry failed to stop normally; see {log}"
            require_normal_exit(handle, 5, "Capture")
            return {"entry_pid": capture.pid, "health": health, "runtime_modules": runtime}
        finally:
            if capture.poll() is None:
                capture.send_signal(signal.CTRL_BREAK_EVENT)
                try:
                    capture.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    taskkill = Path(os.environ["SystemRoot"]) / "System32/taskkill.exe"
                    subprocess.run([taskkill, "/PID", str(capture.pid), "/T", "/F"], env=env,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
            cleanup_process(capture, handle)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--report-dir", type=Path, required=True)
    args = parser.parse_args()
    package = args.package.resolve()
    evidence = args.report_dir.resolve()
    evidence.mkdir(parents=True, exist_ok=False)
    manifest = validate_manifest(package)
    assert json.loads((package / "version.json").read_text(encoding="utf-8"))["version"] == manifest["version"]
    if not KERNEL.GetConsoleCP():
        assert KERNEL.AllocConsole(), "Cannot allocate a private console for Ctrl+Break smoke"
        USER.ShowWindow(KERNEL.GetConsoleWindow(), 0)
    env = dict(os.environ)
    for key in ("VC_ROOT", "VC_NATIVE_DIR", "CARGO_HOME", "RUSTUP_HOME", "PYTHONPATH"):
        env.pop(key, None)
    env["PATH"] = os.pathsep.join([str(Path(os.environ["SystemRoot"]) / "System32"), os.environ["SystemRoot"]])

    def execute(argv, *, code=0, timeout=30):
        reply = subprocess.run(argv, cwd=evidence, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               encoding="utf-8", errors="replace", timeout=timeout)
        assert reply.returncode == code, (argv, reply.returncode, reply.stdout, reply.stderr)
        return reply

    app = package / inventory.APP_EXE
    updater = package / inventory.UPDATER_EXE
    capture_root = evidence / "采集 root"
    host_root = evidence / "主机 root"
    managed = evidence / "版本化 root"
    managed_app = managed / inventory.APP_EXE
    managed_release = managed / "app/releases" / manifest["build_id"]
    shutil.copytree(package, managed_release)
    # Keep only the bootstrap EXE and its app-local CRT at ROOT. No product DLL
    # or version.json at ROOT can mask incorrect release selection.
    shutil.copy2(app, managed_app)
    for entry in manifest["files"]:
        name = entry["path"]
        if inventory.is_msvc_runtime_dll(name):
            shutil.copy2(package / name, managed / name)
    (managed / "app/current.json").write_text(json.dumps({"schema": 1, "build_id": manifest["build_id"],
        "version": manifest["version"], "generation": 1, "activated_utc": "smoke"}), encoding="utf-8")
    (managed / "data").mkdir()
    (managed / "data/vc-rust-config.json").write_text(json.dumps({"version": 1,
        "peer_url": "", "capture": {"fps": 120}}), encoding="utf-8")
    (managed / "data/vc-startup.json").write_text(json.dumps({"schema": 1, "role": "receive"}), encoding="utf-8")

    import pefile
    for exe, subsystem in ((app, 2), (updater, 3)):
        pe = pefile.PE(str(exe))
        try:
            assert pe.OPTIONAL_HEADER.Subsystem == subsystem, f"Unexpected PE subsystem: {exe}"
            if exe == app:
                resource_types = {entry.id for entry in pe.DIRECTORY_ENTRY_RESOURCE.entries}
                assert {3, 14} <= resource_types, f"{exe} has no native application icon"
        finally:
            pe.close()

    stream_caps = stream_capabilities(package)
    validate_stream_capabilities(stream_caps)
    probe = [app, *inventory.VCAM_PROBE_ARGS]
    dll_caps = json.loads(execute([*probe, package / "vc_vcam.dll"], timeout=5).stdout)
    assert dll_caps["stream_revision"] == 2 and dll_caps["shm_version"] == 3, dll_caps
    assert dll_caps["variable_resolution"] is True and dll_caps["max_dimension"] == 8192, dll_caps
    missing = execute([*probe, evidence / "不存在-vcam.dll"], code=2, timeout=5)
    assert not missing.stdout.strip() and missing.stderr.strip(), missing
    no_export = execute([*probe, package / "vc_stream.dll"], code=2, timeout=5)
    assert not no_export.stdout.strip() and "VCStreamCapabilities" in no_export.stderr, no_export

    # Exercise global options before capture here and after capture in the live launch.
    # 首次双击必须出现明确角色选择，选择后写入独立启动偏好；不污染数据面配置。
    missing_role = execute([app, "--root", capture_root, "--native-dir", package,
                            "--start-role", "bogus"], code=2)
    assert not (capture_root / "vc-startup.json").exists(), missing_role
    # GUI smoke 会在独立 ROOT 验证真实选择交互和再次启动读取。
    config = json.loads(execute([app, "--root", capture_root, "--native-dir", package, "capture",
                                 "--print-config"]).stdout)
    assert config["capture"]["codec"] == inventory.CODECS[0], config
    managed_config = json.loads(execute([managed_app, "capture", "--print-config"]).stdout)
    assert managed_config["capture"]["fps"] == 120, managed_config
    assert "[default: 5143]" in execute([app, "capture", "--help"]).stdout
    assert inventory.VCAM_PROBE_ARGS[0] not in execute([app, "--help"]).stdout
    invalid_capture = execute([app, "capture", "--port", "65536"], code=2)
    assert "65536" in invalid_capture.stderr, invalid_capture
    assert execute([updater, "--version"]).stdout.strip() == f"vc-updater {manifest['version']}"
    invalid_launch = execute([updater, "launch", "--root", managed, "--role", "capture",
                              "--port", "65536"], code=2)
    assert "vc-updater launch" in invalid_launch.stderr, invalid_launch
    bad_pointer_root = evidence / "坏指针 root"
    pointer = bad_pointer_root / "app/current.json"
    pointer.parent.mkdir(parents=True)
    for contents in ('{', '{"schema":2,"build_id":"valid"}', '{"schema":1,"build_id":"../escape"}',
                     '{"schema":1,"build_id":"absent-release"}'):
        pointer.write_text(contents, encoding="utf-8")
        invalid_pointer = execute([app, "capture", "--root", bad_pointer_root, "--print-config"], code=2)
        assert not invalid_pointer.stdout.strip() and invalid_pointer.stderr.strip(), invalid_pointer

    direct_capture = smoke_capture([app, "capture", "--root", capture_root, "--native-dir", package],
                                   capture_root, package, manifest, evidence, env, label="capture")
    managed_capture = smoke_capture([managed_app, "capture"], managed, managed_release, manifest,
                                    evidence, env, label="managed-capture", managed=True)
    updater_capture = smoke_capture([updater, "launch", "--root", managed, "--role", "capture"],
                                    managed, managed_release, manifest, evidence, env,
                                    label="updater-capture", managed=True)
    host_root.mkdir()
    (host_root / "vc-startup.json").write_text(json.dumps({"schema": 1, "role": "receive"}), encoding="utf-8")
    direct_host = smoke_host([app, "--root", host_root], host_root, package, evidence, env)
    managed_host = smoke_host([managed_app], managed, managed_release, evidence, env, forwarded=True)
    report = {"ok": True, "package": str(package),
              "direct": {"capture": direct_capture, "host": direct_host},
              "managed": {"capture": managed_capture, "host": managed_host, "config_fps": 120},
              "updater_launch": updater_capture,
              "stream_dll_capabilities": stream_caps,
              "dll_capabilities": dll_caps,
              "checks": ["unified-v1 manifest and exact flat payload with two EXEs",
                         "sanitized PATH without developer tools and Chinese/space paths",
                         "GUI PE icon and console updater PE subsystem",
                         "vc_stream.dll ABI, codecs/sources/SHM capability contract in-process",
                         "bounded real vc_vcam.dll export probe, missing DLL and missing export",
                         "raw default, capture port 5143 and global flags on either side of capture",
                         "capture and updater invalid CLI arguments and invalid current.json rejected",
                         "direct, managed and synchronous updater capture: PID, release and graceful Ctrl+Break",
                         "direct and managed host window belongs to the actual EXE PID, normal close",
                         "actual app-local MSVC runtime loads and managed data configuration"]}
    (evidence / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
