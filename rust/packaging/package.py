"""Build and verify the Rust Windows portable ZIP without replacing the legacy dist.
Run: python -I rust/packaging/package.py --allow-dirty
Development requirements: MSVC, CMake, Cargo, pytest, pefile, Pillow and Npcap.
"""
import argparse
from datetime import datetime, timezone
import hashlib
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
NATIVE_FILES = (
    "vc-sender.exe", "vc-receiver.exe", "vc-vcam.dll", "vc-vcam-verify.exe",
    "vc-monitor.exe", "vc-monitor-diag.exe", "vc-testwin.exe", "libpyrowave-shared-1.dll",
)
REQUIRED_LICENSES = (
    "libjpeg-turbo-LICENSE.md", "libjpeg-turbo-README.ijg",
    "pyrowave-LICENSE.txt", "granite-LICENSE.txt",
)


def sha256(path):
    with Path(path).open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


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


def import_audit(stage):
    import pefile
    local = {p.name.lower(): p for p in stage.iterdir() if p.suffix.lower() in (".exe", ".dll")}
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
                if dependency.startswith(("msvcp", "vcruntime", "concrt")) and dependency not in local:
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
    (stage / "licenses/MSVC-runtime.txt").write_text(
        "Microsoft Visual C++ 2015-2022 x64 runtime DLLs are copied from the installed "
        "Visual Studio Build Tools VC/Redist/MSVC Microsoft.VC143.CRT redistributable directory "
        "by CMake InstallRequiredSystemLibraries. These files are Microsoft software, "
        "not covered by the Rust/native open-source dependency licenses. "
        "See https://learn.microsoft.com/cpp/windows/redistributing-visual-cpp-files and "
        "the applicable Microsoft Visual Studio license terms. Windows OS DLLs, graphics "
        "drivers and the Npcap kernel driver are not redistributed in this archive.\n",
        encoding="utf-8")
    for relative, name in (
        ("build/_deps/pyrowave-src/Granite/third_party/volk/LICENSE.md", "volk-LICENSE.md"),
        ("build/_deps/pyrowave-src/Granite/third_party/khronos/vulkan-headers/LICENSE.md", "Vulkan-Headers-LICENSE.md"),
    ):
        source = ROOT / relative
        if not source.is_file():
            raise RuntimeError(f"Missing native dependency license: {source}")
        shutil.copy2(source, stage / "licenses" / name)
    shutil.copytree(ROOT / "build/_deps/pyrowave-src/Granite/third_party/khronos/vulkan-headers/LICENSES",
                    stage / "licenses/Vulkan-Headers-LICENSES")


def manifest(stage, source, version, licenses, imports, tag, checks):
    files = [{"path": p.relative_to(stage).as_posix(), "bytes": p.stat().st_size, "sha256": sha256(p)}
             for p in sorted(stage.rglob("*")) if p.is_file() and p.name != MARKER]
    result = {"product": "VirtualCapture-Rust-portable", "schema": 1, "version": version,
              "build_id": tag, "platform": "windows-11-x64", "profile": "release", "codec": "raw",
              "created_utc": datetime.now(timezone.utc).isoformat(), "source": source,
              "rust_dependencies": licenses, "pe_imports": imports,
              "checks": dict(checks),
              "limits": ["No Rust dual-PC/240fps/long-run acceptance", "No full DShow registration test",
                         "Npcap driver must be installed separately", "Rust OTA only; not a legacy Python update payload"], "files": files}
    (stage / MARKER).write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return result


def main():
    # -I ignores PYTHONUTF8; configure the coordinator's redirected output too.
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-dirty", action="store_true", help="Include this reviewed worktree's pending changes")
    args = parser.parse_args()
    if os.name != "nt":
        raise RuntimeError("Packaging requires Windows x64")
    source = source_snapshot()
    if source["dirty"] and not args.allow_dirty:
        raise RuntimeError("Worktree has pending changes; use --allow-dirty after reviewing git status")
    version = tomllib.loads((ROOT / "rust/Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    tag = datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + uuid.uuid4().hex[:6]
    name = f"VirtualCapture-Rust-{version}-windows-x64-{tag}"
    work = ROOT / "build/rust-package" / tag
    work.mkdir(parents=True, exist_ok=False)
    stage = work / name
    log_path = work / "package.log"
    with log_path.open("w", encoding="utf-8") as log:
        run(["cargo", "fmt", "--manifest-path", "rust/Cargo.toml", "--all", "--check"], log)
        run(["cargo", "test", "--manifest-path", "rust/Cargo.toml", "--workspace", "--locked"], log)
        run(["cargo", "clippy", "--manifest-path", "rust/Cargo.toml", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"], log)
        run(["cargo", "build", "--manifest-path", "rust/Cargo.toml", "--workspace", "--release", "--locked"], log)
        run([sys.executable, "-I", "-X", "utf8", "scripts/verify.py"], log)
        run(["cmake", "--install", "build", "--config", "Release", "--component", "Runtime", "--prefix", stage], log)
        for executable in ("vc-host.exe", "vc-capture.exe", "vc-updater.exe", "vc-transport.exe"):
            shutil.copy2(ROOT / "rust/target/release" / executable, stage / executable)
        shutil.copy2(ROOT / "rust/target/release/vc-launcher.exe", stage / "VirtualCapture.exe")
        for required in (*NATIVE_FILES, "msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll"):
            if not (stage / required).is_file():
                raise RuntimeError(f"Missing payload file: {required}")
        for license_name in REQUIRED_LICENSES:
            if not (stage / "licenses" / license_name).is_file():
                raise RuntimeError(f"Missing license: {license_name}")
        support_files(stage)
        # -I intentionally excludes script directories: explicitly load the trusted sibling.
        import importlib.util
        spec = importlib.util.spec_from_file_location("vc_collect_licenses", ROOT / "rust/packaging/collect_licenses.py")
        collector = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(collector)
        license_summary = collector.collect_licenses(ROOT, stage / "licenses/rust")
        if license_summary.get("exceptions"):
            raise RuntimeError(f'Unresolved dependency license texts: {license_summary["exceptions"]}')
        forbidden = [p.relative_to(stage).as_posix() for p in stage.rglob("*")
                     if p.name in ("vc-rust-config.json", "vc-config.json", "update-token.json",
                                   "update-peer-token.json", "run", "target", "__pycache__")
                     or p.suffix.lower() in (".pdb", ".log", ".pyc")]
        if forbidden:
            raise RuntimeError(f"Development/runtime data in payload: {forbidden}")
        imports = import_audit(stage)
        (stage / "version.json").write_text(json.dumps({"product": "VirtualCapture-Rust",
             "version": version, "profile": "release", "native_source_version": json.loads((ROOT / "version.json").read_text(encoding="utf-8"))},
             ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        # Stage a complete OTA-valid manifest before testing the extracted candidate.
        # Pending runtime checks are only promoted after their actual commands pass.
        checks = {"legacy_verify": "passed", "rust_tests": "passed", "fmt": "passed",
                  "clippy": "passed", "portable_smoke": "pending", "gui_smoke": "pending",
                  "raw_loopback_120_frames": "pending", "capture_ota": "pending",
                  "host_controller_ota": "pending", "physical_adapters": "pending"}
        manifest(stage, source, version, license_summary, imports, tag, checks)
        # ZIP and extract the payload before all runtime checks, including Unicode/spaces.
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
        run([sys.executable, "-I", "-X", "utf8", "rust/packaging/portable_smoke.py", "--package", extracted, "--report-dir", work / "smoke"], log)
        checks["portable_smoke"] = "passed"
        run([sys.executable, "-I", "-X", "utf8", "rust/tests/adapter_smoke.py", "--package", extracted], log)
        checks["physical_adapters"] = "passed"
        run([sys.executable, "-I", "-X", "utf8", "rust/tests/control_smoke.py", "--package", extracted], log)
        checks["raw_loopback_120_frames"] = "passed"
        run([sys.executable, "-I", "-X", "utf8", "rust/tests/gui_smoke.py", "--package", extracted], log)
        checks["gui_smoke"] = "passed"
        run([sys.executable, "-I", "-X", "utf8", "rust/tests/ota_smoke.py", "--package", extracted, "--candidate", candidate], log)
        checks["capture_ota"] = "passed"
        run([sys.executable, "-I", "-X", "utf8", "rust/tests/host_update_smoke.py", "--package", extracted], log)
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
                  "sha256": digest, "bytes": archive_path.stat().st_size, "files": len(result["files"]) + 1,
                  "log": str(log_path), "validation_directory": str(work), "checks": result["checks"]}
        (work / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(report, ensure_ascii=False, indent=2), flush=True)


if __name__ == "__main__":
    main()
