"""构建并验证 Rust Windows 便携包：python -I rust/packaging/package.py --allow-dirty。

产品与隔离验证器均由 Cargo 构建；CMake 仅提供固定 PyroWave 第三方运行库。
开发环境：Windows x64、MSVC、CMake、Cargo、pefile、Pillow、Npcap。
完整门禁成功才发布到唯一 dist 目录；不替换任何既有部署。
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
MARKER = inventory.MANIFEST_NAME


def source_snapshot():
    def git(*args):
        return subprocess.check_output(["git", "-C", str(ROOT), *args])
    names = git("ls-files", "-z", "--cached", "--others", "--exclude-standard")
    files = {}
    for name in sorted({os.fsdecode(n) for n in names.split(b"\0") if n}):
        path = ROOT / name
        if path.name.lower() in {"update-token.json", "update-peer-token.json"}:
            raise RuntimeError("Update credentials may not be included in a source snapshot")
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
    """复用原生依赖构建目录的生成器，不再依赖旧工程缓存。"""
    cache = NATIVE_BUILD / "CMakeCache.txt"
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


def install_native(stage, audit_root, log):
    run(["cmake", "--install", NATIVE_BUILD, "--config", "Release",
         "--component", "Runtime", "--prefix", stage], log)
    native_audit = audit_root / "native"
    native_audit.mkdir(parents=True, exist_ok=False)
    run(["cmake", "--install", NATIVE_BUILD, "--config", "Release",
         "--component", "LicenseAudit", "--prefix", native_audit], log)


def copy_rust_payload(stage):
    release = ROOT / "rust/target/release"
    # Build-tree diagnostic binaries are deliberately not copied into the portable package.
    for name in inventory.RUST_EXES + inventory.RUST_DLLS:
        source = release / name
        if not source.is_file():
            hint = ""
            if name == "vc_vcam.dll":
                hint = " (crates/vc-vcam must be a member of the rust workspace)"
            raise RuntimeError(f"Missing Rust payload file: {source}{hint}")
        shutil.copy2(source, stage / name)


def import_audit(stage):
    import pefile
    local = {p.name.lower(): p for p in stage.iterdir()
             if p.is_file() and p.suffix.lower() in (".exe", ".dll")}
    unexpected = sorted(name for name in local
                        if name not in inventory.EXPECTED_PE_NAMES
                        and not inventory.is_msvc_runtime_dll(name))
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


def support_files(stage, native_license_dir):
    shutil.copy2(ROOT / "rust/packaging/README-zh.md", stage / "README.md")
    native_license_dir.mkdir(parents=True, exist_ok=True)
    (native_license_dir / "MSVC-runtime.txt").write_text(
        "Microsoft Visual C++ 2015-2022 x64 runtime DLLs are copied from the installed "
        "Visual Studio Build Tools VC/Redist/MSVC Microsoft.VC143.CRT redistributable directory "
        "by CMake InstallRequiredSystemLibraries. These files are Microsoft software, "
        "not covered by the Rust/native open-source dependency licenses. "
        "See https://learn.microsoft.com/cpp/windows/redistributing-visual-cpp-files and "
        "the applicable Microsoft Visual Studio license terms. Windows OS DLLs, graphics "
        "drivers and the Npcap kernel driver are not redistributed in this archive. "
        "PyroWave additionally requires a Vulkan 1.3-capable graphics driver at runtime; "
        "vulkan-1.dll is not bundled. No CPU codec fallback is included.\n",
        encoding="utf-8")


def collect_rust_licenses(audit_root):
    collector = load_sibling("collect_licenses")
    summary = collector.collect_licenses(ROOT, audit_root)
    if summary.get("exceptions"):
        raise RuntimeError(f'Unresolved dependency license texts: {summary["exceptions"]}')
    return summary


def validate_license_material(native_license_dir, rust_audit_root, summary):
    missing_native = [name for name in inventory.NATIVE_LICENSE_AUDIT_FILES
                      if not (native_license_dir / name).is_file()]
    if missing_native:
        raise RuntimeError(f"Missing native license audit inputs: {missing_native}")
    missing_rust = [name for name in inventory.RUST_LICENSE_AUDIT_OUTPUTS
                    if not (rust_audit_root / name).is_file()]
    if missing_rust or not (rust_audit_root / "crates").is_dir():
        raise RuntimeError(f"Incomplete Rust license audit outputs: {missing_rust}")
    if summary.get("exceptions"):
        raise RuntimeError(f'Unresolved dependency license texts: {summary["exceptions"]}')
    return {
        "target": summary["target"],
        "third_party_packages": summary["counts"]["third_party_packages"],
        "license_file_records": summary["counts"]["license_file_records"],
        "exceptions": len(summary["exceptions"]),
        "warnings": len(summary["warnings"]),
    }


def manifest(stage, source, version, imports, tag, checks):
    paths = inventory.validate_payload_directory(stage, with_manifest=(stage / MARKER).exists()) - {MARKER}
    files = [{"path": name, "bytes": (stage / name).stat().st_size, "sha256": sha256(stage / name)}
             for name in sorted(paths)]
    result = {
        "product": "VirtualCapture-Rust-portable",
        "schema": 1,
        "entrypoint": inventory.ENTRYPOINT,
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
            "diagnostic_commands": [list(inventory.VCAM_PROBE_COMMAND)],
            "codec_dlls": dict(inventory.CODEC_DLLS),
            "codecs": list(inventory.CODECS),
            "sources": list(inventory.SOURCES),
            "stream_abi": inventory.STREAM_ABI,
            "legacy_cpp_product": [],
        },
        "pe_imports": imports,
        "checks": dict(checks),
        "limits": [
            "No Rust dual-PC/240fps/long-run acceptance",
            "Raw is default; PyroWave requires explicit opt-in and exact backend match",
            "GPU desktop conversion only; no CPU MJPEG backend",
            "No PyroWave dual-PC or quality acceptance",
            "No full DShow registration test",
            "Npcap driver must be installed separately",
            "PyroWave requires a Vulkan 1.3-capable driver; vulkan-1.dll is not bundled",
            "Rust OTA only; not a legacy Python update payload",
        ],
        "files": files,
        "capabilities": {
            "role_launch_args": True,
            "desktop_capture": True,
        },
    }
    (stage / MARKER).write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return result


def validate_archive(archive, name, expected_manifest):
    prefix = name + "/"
    paths = archive.namelist()
    if any(not path.startswith(prefix) for path in paths):
        raise RuntimeError("Unexpected archive root")
    actual = inventory.validate_payload_paths((path[len(prefix):] for path in paths), with_manifest=True)
    declared = inventory.validate_payload_paths(file["path"] for file in expected_manifest["files"])
    if actual != declared | {MARKER}:
        raise RuntimeError("Archive file inventory does not match manifest")
    if archive.testzip() is not None:
        raise RuntimeError("Archive CRC check failed")
    if json.loads(archive.read(prefix + MARKER)) != expected_manifest:
        raise RuntimeError("Archive manifest mismatch")
    for file in expected_manifest["files"]:
        info = archive.getinfo(prefix + file["path"])
        if info.file_size != file["bytes"]:
            raise RuntimeError(f'Archive length mismatch: {file["path"]}')
        with archive.open(info) as stream:
            if hashlib.file_digest(stream, "sha256").hexdigest() != file["sha256"]:
                raise RuntimeError(f'Archive hash mismatch: {file["path"]}')


def write_verified_archive(path, stage, expected_manifest):
    paths = inventory.validate_payload_directory(stage, with_manifest=True)
    with zipfile.ZipFile(path, "x", zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
        for name in sorted(paths):
            archive.write(stage / name, f"{stage.name}/{name}")
    with zipfile.ZipFile(path) as archive:
        validate_archive(archive, stage.name, expected_manifest)


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
        log.write("Rust 产品构建与验收；CMake 只构建第三方 PyroWave。\n\n")
        run(["cargo", "fmt", "--manifest-path", "rust/Cargo.toml", "--all", "--check"], log)
        run(["cargo", "test", "--manifest-path", "rust/Cargo.toml", "--workspace", "--locked"], log)
        run(["cargo", "clippy", "--manifest-path", "rust/Cargo.toml", "--workspace",
             "--all-targets", "--locked", "--", "-D", "warnings"], log)
        run(["cargo", "build", "--manifest-path", "rust/Cargo.toml", "--workspace",
             "--release", "--locked"], log)

        build_native(log)
        license_audit_root = work / "license-audit"
        native_license_dir = license_audit_root / "native/licenses"
        install_native(stage, license_audit_root, log)
        copy_rust_payload(stage)
        support_files(stage, native_license_dir)
        rust_audit_root = license_audit_root / "rust"
        license_summary = collect_rust_licenses(rust_audit_root)
        license_audit = validate_license_material(native_license_dir, rust_audit_root, license_summary)
        log.write(f"License audit passed (not distributed): {license_audit}\n")
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
            "gpu_only": True,
        }, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        inventory.validate_payload_directory(stage)
        imports = import_audit(stage)

        # Stage a complete OTA-valid manifest before testing the extracted
        # candidate. Pending checks are promoted only after their command passes.
        checks = {
            "rust_tests": "passed",
            "fmt": "passed",
            "clippy": "passed",
            "portable_smoke": "pending",
            "dll_stream_smoke": "pending",
            "adapter_visibility": "pending",
            "gui_smoke": "pending",
            "capture_ota": "pending",
            "raw_loopback_120_frames": "not run (requires --extended-smokes)",
            "host_controller_ota": "not run (requires --extended-smokes)",
        }
        result = manifest(stage, source, version, imports, tag, checks)

        # ZIP and extract the payload before runtime checks, including Unicode/spaces.
        candidate = work / "candidate.zip"
        write_verified_archive(candidate, stage, result)
        extract_root = work / "解压运行 smoke"
        extract_root.mkdir()
        with zipfile.ZipFile(candidate) as archive:
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
        checks["adapter_visibility"] = "passed"
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
        result = manifest(stage, source, version, imports, tag, checks)
        verified_zip = work / "release.zip"
        write_verified_archive(verified_zip, stage, result)
        digest = sha256(verified_zip)
        dist = ROOT / "dist"
        dist.mkdir(exist_ok=True)
        output = dist / name
        archive_path = dist / (name + ".zip")
        checksum = archive_path.with_suffix(".zip.sha256")
        # Unique paths: never replace a prior package, user config, evidence or legacy dist.
        if any(path.exists() for path in (output, archive_path, checksum)):
            raise RuntimeError("Output already exists")
        stage.rename(output)
        verified_zip.rename(archive_path)
        with checksum.open("x", encoding="ascii") as file:
            file.write(f"{digest}  {archive_path.name}\n")
        report = {"ok": True, "directory": str(output), "zip": str(archive_path),
                  "sha256": digest, "bytes": archive_path.stat().st_size,
                  "files": len(result["files"]) + 1, "log": str(log_path),
                  "validation_directory": str(work), "checks": result["checks"],
                  "license_audit": license_audit}
        (work / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n",
                                          encoding="utf-8")
        print(json.dumps(report, ensure_ascii=False, indent=2), flush=True)


if __name__ == "__main__":
    main()
