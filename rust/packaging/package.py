"""Build and verify the Rust-R3 Windows portable ZIP without replacing legacy dist.

Run: python -I rust/packaging/package.py --allow-dirty

The package contains only the Rust product plus the native runtime DLLs it
loads.  The legacy C++ product (vc-sender/vc-receiver/vc-vcam.dll) is neither
built nor copied here; ``python scripts/verify.py`` remains a separate legacy
gate and is intentionally not invoked by this Rust-only packager.

Development requirements: MSVC, CMake, Cargo, pytest, pefile, Pillow and Npcap.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tomllib
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[2]
MARKER = "package-manifest.json"
NATIVE_SOURCE = ROOT / "rust/packaging/native"
NATIVE_BUILD = ROOT / "build/rust-package-native"
DEFAULT_DLL_SMOKE = ROOT / "rust/tests/dll_stream_smoke.py"


def sha256(path):
    with Path(path).open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def load_sibling(name):
    """Load a trusted packaging module; ``python -I`` excludes its directory."""
    path = ROOT / "rust/packaging" / f"{name}.py"
    spec = importlib.util.spec_from_file_location(f"vc_packaging_{name}", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


inventory = load_sibling("inventory")


def source_snapshot():
    def git(*args):
        return subprocess.check_output(["git", "-C", str(ROOT), *args])
    names = git("ls-files", "-z", "--cached", "--others", "--exclude-standard")
    files = {}
    for name in sorted({os.fsdecode(n) for n in names.split(b"\0") if n}):
        path = ROOT / name
        if path.is_file():
            files[name] = sha256(path)
    status = git("status", "--porcelain=v1", "--untracked-files=all").decode("utf-8")
    return {"revision": git("rev-parse", "HEAD").decode().strip(),
            "branch": git("branch", "--show-current").decode().strip(),
            "dirty": bool(status), "status": status.splitlines(), "files": files}


def run(argv, log, cwd=ROOT):
    argv = [str(a) for a in argv]
    title = "> " + subprocess.list2cmdline(argv)
    print(title, flush=True)
    log.write(title + "\n")
    log.flush()
    env = dict(os.environ, PYTHONUTF8="1", PYTHONUNBUFFERED="1", VSLANG="1033")
    with subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, text=True, encoding="utf-8", errors="replace") as proc:
        for line in proc.stdout:
            print(line, end="", flush=True)
            log.write(line)
        code = proc.wait()
    log.flush()
    if code:
        raise RuntimeError(f"Command failed ({code}): {subprocess.list2cmdline(argv)}")


def native_cmake_args():
    """Match the repository's configured generator when one is available."""
    cache = ROOT / "build/CMakeCache.txt"
    if cache.is_file():
        for line in cache.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("CMAKE_GENERATOR:INTERNAL="):
                generator = line.split("=", 1)[1].strip()
                if generator.startswith("Visual Studio"):
                    # The legacy cache leaves the platform implicit; pin x64
                    # explicitly because Rust artifacts are x86_64.
                    return ["-G", generator, "-A", "x64"]
                return ["-G", generator]
    return []


def build_native(log):
    """Build/install only the native runtime component into the staged payload."""
    run(["cmake", "-S", NATIVE_SOURCE, "-B", NATIVE_BUILD,
         *native_cmake_args(), "-DCMAKE_BUILD_TYPE=Release"], log)
    run(["cmake", "--build", NATIVE_BUILD, "--config", "Release"], log)


def install_native(stage, log):
    run(["cmake", "--install", NATIVE_BUILD, "--config", "Release",
         "--component", "Runtime", "--prefix", stage], log)


def copy_rust_payload(stage):
    release = ROOT / "rust/target/release"
    for name in inventory.RUST_EXES + inventory.RUST_DLLS:
        source = release / name
        if not source.is_file():
            hint = ""
            if name == "vc_vcam.dll":
                hint = " (crates/vc-vcam must be a member of the rust workspace)"
            raise RuntimeError(f"Missing Rust payload file: {source}{hint}")
        shutil.copy2(source, stage / name)
    launcher = release / "vc-launcher.exe"
    if not launcher.is_file():
        raise RuntimeError(f"Missing Rust launcher: {launcher}")
    shutil.copy2(launcher, stage / "VirtualCapture.exe")


def import_audit(stage):
    import pefile
    local = {p.name.lower(): p for p in stage.iterdir()
             if p.is_file() and p.suffix.lower() in (".exe", ".dll")}
    expected = set(inventory.EXPECTED_PE_NAMES)
    runtime_families = ("msvcp", "vcruntime", "concrt", "vcomp", "ucrtbase")
    unexpected = sorted(name for name in local
                        if name not in expected and not name.startswith(runtime_families))
    if unexpected:
        raise RuntimeError(f"Unexpected PE payload: {unexpected}")
    nested = [p.relative_to(stage).as_posix() for p in stage.rglob("*")
              if p.is_file() and p.parent != stage and p.suffix.lower() in (".exe", ".dll")]
    if nested:
        raise RuntimeError(f"Unexpected nested PE payload: {nested}")
    system = Path(os.environ["SystemRoot"]) / "System32"
    report = {}
    for name, path in sorted(local.items()):
        pe = pefile.PE(str(path))
        try:
            if pe.FILE_HEADER.Machine != 0x8664:
                raise RuntimeError(f"Not an x64 PE: {path}")
            imports = sorted({entry.dll.decode("ascii").lower()
                              for group in ("DIRECTORY_ENTRY_IMPORT", "DIRECTORY_ENTRY_DELAY_IMPORT")
                              for entry in getattr(pe, group, [])})
            report[name] = imports
            for dependency in imports:
                if dependency.startswith(("api-ms-", "ext-ms-")):
                    continue  # Windows API-set contracts, supplied by the OS.
                if dependency.startswith(("msvcp", "vcruntime", "concrt", "vcomp")) and dependency not in local:
                    raise RuntimeError(f"Missing app-local MSVC runtime: {name} -> {dependency}")
                if dependency not in local and not (system / dependency).is_file():
                    raise RuntimeError(f"Unresolved import: {name} -> {dependency}")
        finally:
            pe.close()
    return report


def support_files(stage):
    support = ROOT / "rust/packaging"
    scripts = stage / "scripts"
    scripts.mkdir()
    for name in ("start-host.ps1", "resolve-capture.ps1"):
        (scripts / name).write_text((support / name).read_text(encoding="utf-8"),
                                    encoding="utf-8-sig", newline="\r\n")
    host = ('@echo off\nsetlocal\n'
            '"%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0scripts\\start-host.ps1" %*\n'
            'exit /b %ERRORLEVEL%\n')
    (stage / "Start-Host.cmd").write_text(host, encoding="ascii", newline="\r\n")
    # Execute the CLI directly: PowerShell interprets Ctrl+Break as a debugger
    # request. A plain CMD parent preserves the native console shutdown handler.
    capture = ('@echo off\nsetlocal\n'
               'set "VC_RELEASE_ID="\n'
               'for /f "usebackq delims=" %%D in (`""%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0scripts\\resolve-capture.ps1" -Root "%~dp0.""`) do set "VC_RELEASE_ID=%%D"\n'
               'if not defined VC_RELEASE_ID exit /b 1\n'
               'set "VC_RELEASE_DIR=%~dp0app\\releases\\%VC_RELEASE_ID%"\n'
               'if "%VC_RELEASE_ID%"=="." set "VC_RELEASE_DIR=%~dp0."\n'
               'if "%~1"=="" (\n'
               '  "%VC_RELEASE_DIR%\\vc-capture.exe" --root "%~dp0." --native-dir "%VC_RELEASE_DIR%" --port 15143\n'
               ') else (\n'
               '  "%VC_RELEASE_DIR%\\vc-capture.exe" --root "%~dp0." --native-dir "%VC_RELEASE_DIR%" %*\n'
               ')\nexit /b %ERRORLEVEL%\n')
    (stage / "Start-Capture.cmd").write_text(capture, encoding="ascii", newline="\r\n")
    shutil.copy2(support / "README-zh.md", stage / "README.md")
    (stage / "licenses").mkdir(exist_ok=True)
    (stage / "licenses/MSVC-runtime.txt").write_text(
        "Microsoft Visual C++ 2015-2022 x64 runtime DLLs are copied from the installed "
        "Visual Studio Build Tools VC/Redist/MSVC Microsoft.VC143.CRT redistributable directory "
        "by CMake InstallRequiredSystemLibraries. These files are Microsoft software, "
        "not covered by the Rust/native open-source dependency licenses. "
        "See https://learn.microsoft.com/cpp/windows/redistributing-visual-cpp-files and "
        "the applicable Microsoft Visual Studio license terms. Windows OS DLLs, graphics "
        "drivers and the Npcap kernel driver are not redistributed in this archive. "
        "PyroWave additionally requires a Vulkan 1.3-capable graphics driver at runtime; "
        "vulkan-1.dll is not bundled. turbojpeg.dll is the pinned libjpeg-turbo 3.1.2 "
        "shared build used by the Rust MJPEG module.\n",
        encoding="utf-8")


def validate_inventory(stage):
    missing = [name for name in inventory.REQUIRED_FILES if not (stage / name).is_file()]
    if missing:
        raise RuntimeError(f"Missing payload files: {missing}")
    missing_licenses = [name for name in inventory.REQUIRED_LICENSES
                        if not (stage / "licenses" / name).is_file()]
    if missing_licenses:
        raise RuntimeError(f"Missing native/legal files: {missing_licenses}")
    missing_rust_licenses = [name for name in inventory.RUST_LICENSE_OUTPUTS
                             if not (stage / "licenses" / name).is_file()]
    if missing_rust_licenses:
        raise RuntimeError(f"Missing Rust license outputs: {missing_rust_licenses}")
    present = {p.name.lower() for p in stage.rglob("*") if p.is_file()}
    forbidden = sorted(present & inventory.FORBIDDEN_NAMES)
    if forbidden:
        raise RuntimeError(f"Legacy C++ production binaries in payload: {forbidden}")
    forbidden_paths = [p.relative_to(stage).as_posix() for p in stage.rglob("*")
                       if p.name in ("vc-rust-config.json", "vc-config.json", "update-token.json",
                                     "update-peer-token.json", "run", "target", "__pycache__")
                       or p.suffix.lower() in (".pdb", ".log", ".pyc")]
    if forbidden_paths:
        raise RuntimeError(f"Development/runtime data in payload: {forbidden_paths}")


def collect_rust_licenses(stage):
    collector = load_sibling("collect_licenses")
    summary = collector.collect_licenses(ROOT, stage / "licenses/rust")
    if summary.get("exceptions"):
        raise RuntimeError(f'Unresolved dependency license texts: {summary["exceptions"]}')
    return summary


def manifest(stage, source, version, licenses, imports, tag, checks):
    files = [{"path": p.relative_to(stage).as_posix(), "bytes": p.stat().st_size, "sha256": sha256(p)}
             for p in sorted(stage.rglob("*")) if p.is_file() and p.name != MARKER]
    result = {
        "product": "VirtualCapture-Rust-portable",
        "schema": 1,
        "version": version,
        "build_id": tag,
        "platform": "windows-11-x64",
        "profile": "release",
        "default_codec": inventory.CODECS[0],
        "codecs": list(inventory.CODECS),
        "created_utc": datetime.now(timezone.utc).isoformat(),
        "source": source,
        "runtime_modules": {
            "in_process_dlls": list(inventory.RUST_DLLS),
            "diagnostic_exes": ["vc-stream.exe", "vc-transport.exe", "vc-vcam-verify.exe"],
            "codec_dlls": dict(inventory.CODEC_DLLS),
            "codecs": list(inventory.CODECS),
            "sources": list(inventory.SOURCES),
            "stream_abi": inventory.STREAM_ABI,
            "legacy_cpp_product": [],
        },
        "rust_dependencies": licenses,
        "pe_imports": imports,
        "checks": dict(checks),
        "limits": [
            "No Rust dual-PC/240fps/long-run acceptance",
            "Raw is default; MJPEG/PyroWave require explicit opt-in and exact backend match",
            "No MJPEG/PyroWave dual-PC or quality acceptance",
            "No full DShow registration test",
            "Npcap driver must be installed separately",
            "PyroWave requires a Vulkan 1.3-capable driver; vulkan-1.dll is not bundled",
            "Rust OTA only; not a legacy Python update payload",
        ],
        "files": files,
    }
    (stage / MARKER).write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return result


def main():
    # -I ignores PYTHONUTF8; configure the coordinator's redirected output too.
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-dirty", action="store_true",
                        help="Include this reviewed worktree's pending changes")
    parser.add_argument("--dll-smoke", type=Path, default=DEFAULT_DLL_SMOKE,
                        help="Same-process vc_stream.dll smoke test (default: rust/tests/dll_stream_smoke.py)")
    parser.add_argument("--extended-smokes", action="store_true",
                        help="Also exercise REST-to-DLL streaming and the host controller's local/peer OTA")
    args = parser.parse_args()
    if os.name != "nt":
        raise RuntimeError("Packaging requires Windows x64")
    if not args.dll_smoke.is_file():
        raise RuntimeError(
            f"Missing same-process DLL smoke: {args.dll_smoke}; add rust/tests/dll_stream_smoke.py "
            "or pass --dll-smoke <path> (it must accept --package <extracted release>)")
    source = source_snapshot()
    if source["dirty"] and not args.allow_dirty:
        raise RuntimeError("Worktree has pending changes; use --allow-dirty after reviewing git status")
    version = tomllib.loads((ROOT / "rust/Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    tag = datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + uuid.uuid4().hex[:6]
    name = f"VirtualCapture-Rust-{version}-windows-x64-{tag}"
    work = ROOT / "build/rust-package" / tag
    work.mkdir(parents=True, exist_ok=False)
    stage = work / name
    stage.mkdir()
    log_path = work / "package.log"
    with log_path.open("w", encoding="utf-8") as log:
        if source["dirty"]:
            log.write("Reviewed dirty worktree status (--allow-dirty):\n")
            log.write("\n".join(source["status"]) + "\n\n")
        log.write("Legacy scripts/verify.py is a separate required gate; "
                  "this Rust package does not run or copy the C++ product.\n\n")
        run(["cargo", "fmt", "--manifest-path", "rust/Cargo.toml", "--all", "--check"], log)
        run(["cargo", "test", "--manifest-path", "rust/Cargo.toml", "--workspace", "--locked"], log)
        run(["cargo", "clippy", "--manifest-path", "rust/Cargo.toml", "--workspace",
             "--all-targets", "--locked", "--", "-D", "warnings"], log)
        run(["cargo", "build", "--manifest-path", "rust/Cargo.toml", "--workspace",
             "--release", "--locked"], log)

        build_native(log)
        install_native(stage, log)
        copy_rust_payload(stage)
        support_files(stage)
        license_summary = collect_rust_licenses(stage)
        validate_inventory(stage)
        imports = import_audit(stage)
        (stage / "version.json").write_text(json.dumps({
            "product": "VirtualCapture-Rust",
            "version": version,
            "profile": "release",
            "runtime_modules": {
                "in_process_dlls": list(inventory.RUST_DLLS),
                "codec_dlls": list(inventory.CODEC_DLLS.values()),
                "codecs": list(inventory.CODECS),
                "sources": list(inventory.SOURCES),
                "stream_abi": inventory.STREAM_ABI,
            },
            "native_source_version": json.loads((ROOT / "version.json").read_text(encoding="utf-8")),
        }, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

        # Stage a complete OTA-valid manifest before testing the extracted
        # candidate. Pending checks are promoted only after their command passes.
        checks = {
            "legacy_verify": "external (scripts/verify.py)",
            "rust_tests": "passed",
            "fmt": "passed",
            "clippy": "passed",
            "portable_smoke": "pending",
            "dll_stream_smoke": "pending",
            "physical_adapters": "pending",
            "gui_smoke": "pending",
            "capture_ota": "pending",
            "raw_loopback_120_frames": "not run (requires --extended-smokes)",
            "host_controller_ota": "not run (requires --extended-smokes)",
        }
        manifest(stage, source, version, license_summary, imports, tag, checks)

        # ZIP and extract the payload before runtime checks, including Unicode/spaces.
        candidate = work / "candidate.zip"
        with zipfile.ZipFile(candidate, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
            for file in sorted(stage.rglob("*")):
                if file.is_file():
                    archive.write(file, Path(name) / file.relative_to(stage))
        extract_root = work / "解压运行 smoke"
        extract_root.mkdir()
        with zipfile.ZipFile(candidate) as archive:
            if archive.testzip() is not None:
                raise RuntimeError("Candidate archive CRC check failed")
            archive.extractall(extract_root)
        extracted = extract_root / name

        run([sys.executable, "-I", "-X", "utf8", "rust/packaging/portable_smoke.py",
             "--package", extracted, "--report-dir", work / "smoke"], log)
        checks["portable_smoke"] = "passed"
        run([sys.executable, "-I", "-X", "utf8", args.dll_smoke,
             "--package", extracted], log)
        checks["dll_stream_smoke"] = "passed"
        run([sys.executable, "-I", "-X", "utf8", "rust/tests/adapter_smoke.py",
             "--package", extracted], log)
        checks["physical_adapters"] = "passed"
        run([sys.executable, "-I", "-X", "utf8", "rust/tests/gui_smoke.py",
             "--package", extracted], log)
        checks["gui_smoke"] = "passed"
        run([sys.executable, "-I", "-X", "utf8", "rust/tests/ota_smoke.py",
             "--package", extracted, "--candidate", candidate], log)
        checks["capture_ota"] = "passed"
        if args.extended_smokes:
            run([sys.executable, "-I", "-X", "utf8", "rust/tests/control_smoke.py",
                 "--package", extracted], log)
            checks["raw_loopback_120_frames"] = "passed"
            run([sys.executable, "-I", "-X", "utf8", "rust/tests/host_update_smoke.py",
                 "--package", extracted], log)
            checks["host_controller_ota"] = "passed"

        if source_snapshot() != source:
            raise RuntimeError("Source files changed while building; no archive published")
        result = manifest(stage, source, version, license_summary, imports, tag, checks)
        dist = ROOT / "dist"
        dist.mkdir(exist_ok=True)
        output = dist / name
        archive_path = dist / (name + ".zip")
        # Unique paths: never replace a prior package, user config, evidence or legacy dist.
        if output.exists() or archive_path.exists():
            raise RuntimeError("Output already exists")
        stage.rename(output)
        with zipfile.ZipFile(archive_path, "x", zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
            for file in sorted(output.rglob("*")):
                if file.is_file():
                    archive.write(file, Path(name) / file.relative_to(output))
        with zipfile.ZipFile(archive_path) as archive:
            if archive.testzip() is not None:
                raise RuntimeError("Final archive CRC check failed")
            if json.loads(archive.read(f"{name}/{MARKER}")) != result:
                raise RuntimeError("Final archive manifest mismatch")
            for file in result["files"]:
                with archive.open(f'{name}/{file["path"]}') as stream:
                    if hashlib.file_digest(stream, "sha256").hexdigest() != file["sha256"]:
                        raise RuntimeError(f'Final archive hash mismatch: {file["path"]}')
        digest = sha256(archive_path)
        checksum = archive_path.with_suffix(".zip.sha256")
        checksum.write_text(f"{digest}  {archive_path.name}\n", encoding="ascii")
        report = {"ok": True, "directory": str(output), "zip": str(archive_path),
                  "sha256": digest, "bytes": archive_path.stat().st_size,
                  "files": len(result["files"]) + 1, "log": str(log_path),
                  "validation_directory": str(work), "checks": result["checks"]}
        (work / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n",
                                          encoding="utf-8")
        print(json.dumps(report, ensure_ascii=False, indent=2), flush=True)


if __name__ == "__main__":
    main()
