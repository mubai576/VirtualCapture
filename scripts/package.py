"""编译、验证并生成完整便携目录：python scripts/package.py [--allow-dirty]。"""
import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
MARKER = "package-manifest.json"
PRODUCT = "VirtualCapture-portable"
NATIVE_FILES = (
    "vc-sender.exe", "vc-receiver.exe", "vc-vcam.dll", "vc-testwin.exe",
    "vc-monitor.exe", "vc-monitor-diag.exe", "vc-vcam-verify.exe",
)
RUNTIME_FILES = ("msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll", "libpyrowave-shared-1.dll")
LICENSE_FILES = ("licenses/libjpeg-turbo-LICENSE.md", "licenses/libjpeg-turbo-README.ijg",
                 "licenses/pyrowave-LICENSE.txt", "licenses/granite-LICENSE.txt")
BUILD_ONLY_SCRIPTS = {"verify.py", "package.py", "p6_raw_verify.py", "p6_product_verify.py", "p6_codec_verify.py"}


class PackageError(Exception):
    pass


def sha256(path):
    with Path(path).open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def validate_output(root, output):
    root, output = root.resolve(), output.resolve()
    # Only dist/ may hold a package inside the source repository.
    if output == root or root.is_relative_to(output):
        raise PackageError("输出目录不能是仓库本身或仓库的上级目录")
    if output.is_relative_to(root) and (
            not output.is_relative_to(root / "dist") or output == root / "dist"):
        raise PackageError("仓库内的输出目录必须位于 dist/ 下，例如 dist/VirtualCapture")
    if output.exists():
        try:
            marker = json.loads((output / MARKER).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            raise PackageError(f"拒绝覆盖非本脚本生成的目录：{output}") from None
        if marker.get("product") != PRODUCT:
            raise PackageError(f"输出目录的包标记不匹配：{output}")
    return output


def source_snapshot(root):
    def git(*args):
        return subprocess.check_output(["git", "-C", str(root), *args])
    revision = git("rev-parse", "HEAD").decode().strip()
    status = git("status", "--porcelain=v1", "--untracked-files=all").decode("utf-8")
    names = git("ls-files", "-z", "--cached", "--others", "--exclude-standard")
    files = {}
    for name in sorted(set(os.fsdecode(n) for n in names.split(b"\0") if n)):
        p = root / name
        if p.is_file():
            files[name] = sha256(p)
    return {"revision": revision, "dirty": bool(status),
            "status": status.splitlines(), "files": files}


def copy_support_files(root, stage):
    shutil.copy2(root / "version.json", stage / "version.json")
    for name in ("Start-VirtualCapture.cmd", "Start-Capture-Worker.cmd"):
        shutil.copy2(root / "packaging" / name, stage)
    shutil.copy2(root / "docs/runbooks/package-quickstart.md", stage / "README.md")
    # Copy only source scripts/docs, never caches, test data, local config or logs.
    for folder in ("ctrl", "scripts"):
        target = stage / folder
        target.mkdir()
        for src in sorted((root / folder).iterdir()):
            if src.is_file() and src.suffix in (".py", ".ps1") and src.name not in BUILD_ONLY_SCRIPTS:
                if src.suffix == ".ps1":
                    # Windows PowerShell 5.1 treats UTF-8 without a BOM as ANSI.
                    (target / src.name).write_text(src.read_text(encoding="utf-8-sig"),
                                                  encoding="utf-8-sig", newline="\r\n")
                else:
                    shutil.copy2(src, target / src.name)
    for src in sorted((root / "docs").rglob("*.md")):
        target = stage / src.relative_to(root)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, target)


def require_payload(stage):
    required = (*NATIVE_FILES, *RUNTIME_FILES, *LICENSE_FILES, "vc-agent.exe", "version.json",
                "Start-VirtualCapture.cmd", "Start-Capture-Worker.cmd", "README.md",
                "scripts/start-agent.ps1", "scripts/start-host.ps1", "scripts/start-capture-worker.ps1")
    missing = [name for name in required if not (stage / name).is_file()]
    if missing:
        raise PackageError("成品缺少必需文件：" + ", ".join(missing))


def write_manifest(stage, source, version):
    files = []
    for p in sorted(stage.rglob("*")):
        if p.is_file() and p.name != MARKER:
            files.append({"path": p.relative_to(stage).as_posix(),
                          "bytes": p.stat().st_size, "sha256": sha256(p)})
    manifest = {"product": PRODUCT, "schema": 1, "version": version,
                "created_utc": datetime.now(timezone.utc).isoformat(),
                "platform": "windows-x64", "source": source,
                "checks": {"verify": "passed", "package_smoke": "passed"},
                "files": files}
    (stage / MARKER).write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n",
                               encoding="utf-8")
    return manifest


def publish(stage, output):
    """Archive a previous managed package and restore it if the final rename fails."""
    backup = None
    if output.exists():
        backup = output.parent / (output.name + ".archive") / (
            output.name + "-" + datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + uuid.uuid4().hex[:8])
        backup.parent.mkdir(parents=True, exist_ok=True)
        output.rename(backup)
    try:
        stage.rename(output)
    except OSError:
        if backup is not None:
            backup.rename(output)
        raise
    return backup


def run(argv, log, cwd=ROOT):
    argv = [str(a) for a in argv]
    title = "== " + subprocess.list2cmdline(argv)
    print(title, flush=True)
    log.write(title + "\n"); log.flush()
    env = dict(os.environ, PYTHONUTF8="1", PYTHONUNBUFFERED="1", VSLANG="1033")
    with subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, text=True, encoding="utf-8", errors="replace") as p:
        for line in p.stdout:
            print(line, end="", flush=True)
            log.write(line)
        code = p.wait()
    log.flush()
    if code:
        raise PackageError(f"命令失败（退出码 {code}）：{subprocess.list2cmdline(argv)}")


@contextmanager
def smoke_directory():
    directory = tempfile.TemporaryDirectory(prefix="vc-package-smoke-")
    try:
        yield Path(directory.name)
    finally:
        # Windows can retain the onefile image briefly after its process tree exits.
        deadline = time.monotonic() + 10
        while True:
            try:
                directory.cleanup()
                break
            except PermissionError:
                if time.monotonic() >= deadline: raise
                time.sleep(0.2)


def smoke(stage, version, log):
    powershell = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    parser = r"""
$bad = $false
foreach ($f in Get-ChildItem -LiteralPath (Join-Path (Get-Location) 'scripts') -Filter '*.ps1') {
  $tokens = $null; $parseErrors = $null
  [System.Management.Automation.Language.Parser]::ParseFile($f.FullName, [ref]$tokens, [ref]$parseErrors) | Out-Null
  if ($parseErrors) { $parseErrors | Format-List; $bad = $true }
}
if ($bad) { exit 1 }
. (Join-Path (Get-Location) 'scripts/runtime-paths.ps1')
foreach ($name in @('vc-testwin.exe', 'vc-sender.exe', 'vc-monitor.exe')) {
  if ((Get-VcBinaryPath $name) -ne (Join-Path (Get-Location) $name)) { exit 1 }
}
Write-Output 'PASS PowerShell 5.1 scripts and portable binary lookup'
"""
    run([powershell, "-NoProfile", "-NonInteractive", "-Command", parser], log, cwd=stage)
    # An isolated runtime root checks the distributed files without touching the live agent.
    with smoke_directory() as sandbox:
        for p in stage.iterdir():
            if p.is_file() and (p.suffix in (".exe", ".dll") or p.name == "version.json"):
                shutil.copy2(p, sandbox / p.name)
        for name in ("vc-sender.exe", "vc-receiver.exe"):
            run([sandbox / name, "--selftest"], log, cwd=sandbox)
        run([sandbox / "vc-agent.exe", "--help"], log, cwd=sandbox)
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        proc = subprocess.Popen([str(sandbox / "vc-agent.exe"), "--port", str(port)],
                                cwd=sandbox, stdout=log, stderr=subprocess.STDOUT,
                                creationflags=subprocess.CREATE_NO_WINDOW)
        try:
            base = f"http://127.0.0.1:{port}"
            deadline = time.monotonic() + 30
            while True:
                if proc.poll() is not None:
                    raise PackageError("成品 agent 提前退出，详见打包日志")
                try:
                    with urllib.request.urlopen(base + "/health", timeout=1) as r:
                        health = json.load(r)
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise PackageError("成品 agent 启动超时") from None
                    time.sleep(0.2)
            if (not health.get("ok") or health.get("version") != version
                    or health.get("product_role") != "receive" or health.get("product_revision") != 2
                    or Path(health.get("root", "")) != sandbox):
                raise PackageError(f"成品 agent 健康检查/版本不符：{health}")
            # The GUI is built into the agent; verify it survives freezing.
            with urllib.request.urlopen(base + "/ui", timeout=5) as r:
                if "VirtualCapture" not in r.read().decode("utf-8"):
                    raise PackageError("成品 GUI 页面内容异常")
            with urllib.request.urlopen(base + "/vcam/status", timeout=5) as r:
                dll = Path(json.load(r)["dll"])
            if dll != sandbox / "vc-vcam.dll":
                raise PackageError("成品 agent 未正确定位同目录 vc-vcam.dll")
            print("PASS 成品自检、agent /health、/ui、vcam 路径", flush=True)
            log.write("PASS package selftests + health + GUI + vcam path\n")
        finally:
            if proc.poll() is None:
                # PyInstaller onefile has a bootloader parent and a server child.
                subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            proc.wait(timeout=10)
        # The capture worker must expose the control API without serving a GUI.
        proc = subprocess.Popen([str(sandbox / "vc-agent.exe"), "--role", "capture", "--port", str(port)],
                                cwd=sandbox, stdout=log, stderr=subprocess.STDOUT,
                                creationflags=subprocess.CREATE_NO_WINDOW)
        try:
            deadline = time.monotonic() + 30
            while True:
                if proc.poll() is not None:
                    raise PackageError("成品采集后台提前退出")
                try:
                    with urllib.request.urlopen(base + "/health", timeout=1) as r:
                        health = json.load(r)
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise PackageError("成品采集后台启动超时") from None
                    time.sleep(0.2)
            if (health.get("product_role") != "capture" or health.get("version") != version
                    or Path(health.get("root", "")) != sandbox):
                raise PackageError(f"成品采集后台角色/版本/路径不符：{health}")
            for path in ("/", "/ui", "/ui/"):
                try:
                    urllib.request.urlopen(base + path, timeout=5).close()
                except urllib.error.HTTPError as error:
                    if error.code == 404: continue
                    raise
                raise PackageError("采集后台不应提供 GUI：" + path)
            print("PASS 成品采集后台 /health 与 UI 404", flush=True)
            log.write("PASS frozen capture worker health + no GUI\n")
        finally:
            if proc.poll() is None:
                subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            proc.wait(timeout=10)


def main(argv=None):
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--output", type=Path, default=Path("dist/VirtualCapture"),
                    help="成品目录；相对路径从仓库根解析（默认 dist/VirtualCapture）")
    ap.add_argument("--allow-dirty", action="store_true",
                    help="已确认可包含当前未提交改动；变更清单及源文件哈希写入成品清单")
    args = ap.parse_args(argv)
    try:
        if os.name != "nt" or struct.calcsize("P") != 8:
            raise PackageError("请使用 Windows x64 与 64 位 Python 执行打包")
        for name in ("cmake", "git"):
            if not shutil.which(name):
                raise PackageError(f"缺少 {name}，请先安装并加入 PATH")
        for name in ("PyInstaller", "pytest"):
            try:
                importlib.metadata.version(name)
            except importlib.metadata.PackageNotFoundError:
                raise PackageError(f"缺少 {name}；执行 python -m pip install {name} 后重试") from None
        output = validate_output(ROOT, ROOT / args.output)
        source = source_snapshot(ROOT)
        if source["dirty"] and not args.allow_dirty:
            raise PackageError("工作区有未提交改动。确认均可打包后加 --allow-dirty；避免混入其他会话的工作")
        version = json.loads((ROOT / "version.json").read_text(encoding="utf-8"))["version"]
        work = ROOT / "build/package"
        work.mkdir(parents=True, exist_ok=True)
        log_path = work / ("package-" + datetime.now().strftime("%Y%m%d-%H%M%S") + ".log")
        output.parent.mkdir(parents=True, exist_ok=True)
        print(f"打包日志：{log_path}", flush=True)
        with log_path.open("w", encoding="utf-8") as log:
            # Keep the previous output untouched until every build and smoke check succeeds.
            with tempfile.TemporaryDirectory(prefix=".vc-package-", dir=output.parent) as tmp:
                stage = Path(tmp) / "payload"
                stage.mkdir()
                run([sys.executable, ROOT / "scripts/verify.py"], log)
                run(["cmake", "--install", ROOT / "build", "--config", "Release",
                     "--component", "Runtime", "--prefix", stage], log)
                run([sys.executable, "-m", "PyInstaller", "--noconfirm", "--clean",
                     "--distpath", stage, "--workpath", work / "pyinstaller",
                     ROOT / "packaging/vc-agent.spec"], log)
                copy_support_files(ROOT, stage)
                require_payload(stage)
                smoke(stage, version, log)
                if source_snapshot(ROOT) != source:
                    raise PackageError("构建期间源码发生变化，未发布成品；请等待其他编辑结束后重试")
                manifest = write_manifest(stage, source, version)
                validate_output(ROOT, output)
                backup = publish(stage, output)
        total = sum(f["bytes"] for f in manifest["files"])
        print(f"\n打包完成：{output}\n版本：{version}；{len(manifest['files'])} 个文件；{total / 1024**2:.1f} MiB")
        if backup:
            print(f"上一份成品已归档：{backup}")
        print(f"日志：{log_path}\n双击 Start-VirtualCapture.cmd，按 README.md 打开控制台。")
        return 0
    except (PackageError, OSError, ValueError, KeyError, subprocess.SubprocessError) as e:
        print(f"打包失败：{e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
