"""End-to-end OTA smoke test on a private temporary root and random port.

Copies the debug `vc-capture.exe`/`vc-updater.exe` into synthetic portable ZIPs,
stages them through the real REST API (token protected), activates, rolls back and
forces a failed activation to verify automatic rollback. No registry, Npcap capture
or deployed installation is touched.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import shutil
import socket
import stat
import subprocess
import time
import urllib.error
import urllib.request
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "rust/target/debug"
RUN = ROOT / "run" / ("rust-ota-smoke-" + uuid.uuid4().hex[:10])
RUN.mkdir(parents=True)
CREATE_NO_WINDOW = 0x08000000
PRODUCT = "VirtualCapture-Rust-portable"


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def sha(data):
    return hashlib.sha256(data).hexdigest()


def request(path, method="GET", token=None, body=None, headers=None, timeout=10):
    url = path if path.startswith("http") else f"http://127.0.0.1:{PORT}{path}"
    req = urllib.request.Request(url, data=body, method=method)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    for key, value in (headers or {}).items():
        req.add_header(key, value)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            return response.status, json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            return error.code, json.loads(raw or b"{}")
        except json.JSONDecodeError:
            return error.code, {"raw": raw.decode(errors="replace")}


def health_at(port, timeout=1):
    try:
        status, value = request(f"http://127.0.0.1:{port}/health", timeout=timeout)
        return value if status == 200 else None
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        return None


def health(timeout=1):
    return health_at(PORT, timeout)


def wait_for(check, timeout, interval=0.25, label="condition"):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = check()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"timed out waiting for {label}; last={last!r}")


def package_path(root_name, files, build_id, version, manifest_files=None):
    manifest = {
        "product": PRODUCT,
        "schema": 1,
        "version": version,
        "build_id": build_id,
        "files": manifest_files
        if manifest_files is not None
        else [
            {"path": name, "bytes": len(data), "sha256": sha(data)}
            for name, data in files.items()
        ],
    }
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(f"{root_name}/package-manifest.json", json.dumps(manifest))
        for name, data in files.items():
            archive.writestr(f"{root_name}/{name}", data)
    return buffer.getvalue()


def candidate_package(zip_path, work_root):
    """Extract a real package, recompute the manifest from the declared tree, re-zip.

    Only `manifest.files` plus the manifest are read, so a package root that has
    accumulated runtime files (update-token.json, data/, run/) never leaks into the
    candidate. The candidate ZIP written by package.py may still carry placeholder
    checks; hashes/bytes are recomputed while product/schema/version/build_id stay.
    """
    extract = Path(work_root) / "candidate-extract"
    if extract.exists():
        shutil.rmtree(extract)
    extract.mkdir(parents=True)
    with zipfile.ZipFile(zip_path) as archive:
        names = [name for name in archive.namelist() if not name.endswith("/")]
        manifest_name = next(
            (name for name in names if name.rsplit("/", 1)[-1] == "package-manifest.json"),
            None,
        )
        assert manifest_name, "candidate zip lacks package-manifest.json"
        for name in names:
            parts = Path(name).parts
            if Path(name).is_absolute() or ".." in parts or "\\" in name:
                raise AssertionError(f"unsafe candidate entry: {name}")
        manifest = json.loads(archive.read(manifest_name))
        prefix = manifest_name[: -len("package-manifest.json")].rstrip("/")
        declared = manifest.get("files") or []
        assert declared, "candidate manifest has no files"
        wanted = {manifest_name}
        for item in declared:
            wanted.add(f"{prefix}/{item['path']}" if prefix else item["path"])
        missing = sorted(wanted - set(names))
        assert not missing, f"candidate manifest references missing entries: {missing[:5]}"
        for name in wanted:
            archive.extract(name, extract)
    files = []
    for item in declared:
        name = f"{prefix}/{item['path']}" if prefix else item["path"]
        data = (extract / Path(name)).read_bytes()
        files.append({"path": item["path"], "bytes": len(data), "sha256": sha(data)})
    manifest["files"] = sorted(files, key=lambda item: item["path"])
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(manifest_name, json.dumps(manifest))
        for item in manifest["files"]:
            name = f"{prefix}/{item['path']}" if prefix else item["path"]
            archive.writestr(name, (extract / Path(name)).read_bytes())
    return (
        buffer.getvalue(),
        manifest["build_id"],
        manifest.get("version", ""),
        len(manifest["files"]),
    )


def capture_package(build_id, version, capture_bytes, updater_bytes):
    root_name = f"VirtualCapture-Rust-{version}-windows-x64-{build_id}"
    return package_path(
        root_name,
        {
            "vc-capture.exe": capture_bytes,
            "vc-updater.exe": updater_bytes,
            "version.json": json.dumps({"product": "VirtualCapture-Rust", "version": version}).encode(),
        },
        build_id,
        version,
    )


def stage(zip_bytes, token, declared_hash=None):
    digest = declared_hash or sha(zip_bytes)
    return request(
        "/update/stage",
        method="POST",
        token=token,
        body=zip_bytes,
        headers={"Content-Type": "application/zip", "X-VC-SHA256": digest},
        timeout=120,
    )


def activate(build_id, token, path="/update/activate"):
    return request(path, method="POST", token=token, body=json.dumps({"build_id": build_id}).encode(), headers={"Content-Type": "application/json"}, timeout=30)


def wait_log(root, needle, timeout=40):
    def found():
        for path in (Path(root) / "updates/worker").glob("updater-*.log"):
            if needle in path.read_text(errors="replace"):
                return path
        return None

    return wait_for(found, timeout, label=f"helper log {needle}")


def history_at(root):
    path = Path(root) / "updates/history.jsonl"
    try:
        return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    except FileNotFoundError:
        return []


def token_from(path):
    return json.loads(path.read_text(encoding="utf-8"))["token"]


def kill_tree(pid):
    if pid:
        subprocess.run(["taskkill", "/PID", str(pid), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def kill_under(root):
    """Kill every process whose command line mentions this private root (helper-spawned ones included)."""
    root = str(root)
    script = (
        "$self=$PID; Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -ne $self -and $_.CommandLine -and $_.CommandLine.Contains('"
        + root.replace("'", "''")
        + "') } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }"
    )
    subprocess.run(
        ["powershell", "-NoProfile", "-NonInteractive", "-Command", script],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def main(package_dir=None, candidate=None):
    global PORT, BIN
    if package_dir:
        BIN = Path(package_dir).resolve()
    PORT = free_port()
    capture_exe = BIN / "vc-capture.exe"
    updater_exe = BIN / "vc-updater.exe"
    assert capture_exe.is_file() and updater_exe.is_file(), "build the workspace first"
    capture_bytes = capture_exe.read_bytes()
    updater_bytes = updater_exe.read_bytes()
    legacy_config = {"version": 1, "peer_url": "", "capture": {"fps": 77, "mode": "pattern"}}
    (RUN / "vc-rust-config.json").write_text(json.dumps(legacy_config), encoding="utf-8")
    checks = []
    pids = set()
    launch_root = RUN.parent / (RUN.name + "-launch")
    log = (RUN / "capture.log").open("wb")
    try:
        proc = subprocess.Popen(
            [str(capture_exe), "--root", str(RUN), "--native-dir", str(BIN), "--bind", "127.0.0.1", "--port", str(PORT)],
            stdout=log, stderr=log, creationflags=CREATE_NO_WINDOW,
        )
        pids.add(proc.pid)
        first = wait_for(health, 15, label="first capture health")
        assert first["build"] is None and first["layout"] == "legacy", first
        pids.add(first["pid"])
        checks.append("legacy layout health")

        status, _ = request("/update/status")
        assert status == 401, status
        token_path = RUN / "update-token.json"
        token = token_from(token_path)
        status, value = request("/update/status", token=token)
        assert status == 200 and value["layout"] == "legacy" and value["active"] is None, value
        checks.append("update token required")

        printed = subprocess.run(
            [str(capture_exe), "--root", str(RUN), "--print-update-token"],
            capture_output=True, encoding="utf-8", timeout=20, check=True,
        )
        assert token in printed.stdout and str(token_path) in printed.stdout, printed.stdout
        log.flush()
        assert token not in (RUN / "capture.log").read_text(errors="replace")
        checks.append("pairing token explicitly printable, never logged")

        launch_root.mkdir()
        shutil.copy2(capture_exe, launch_root / "vc-capture.exe")
        shutil.copy2(updater_exe, launch_root / "vc-updater.exe")
        launch_port = free_port()
        launcher = subprocess.Popen(
            [str(updater_exe), "launch", "--root", str(launch_root), "--role", "capture",
             "--bind", "127.0.0.1", "--port", str(launch_port)],
            stdout=log, stderr=log, creationflags=CREATE_NO_WINDOW,
        )
        launched = wait_for(lambda: health_at(launch_port), 15, label="launcher-spawned flat capture")
        assert launched["layout"] == "legacy", launched
        kill_tree(launcher.pid)
        wait_for(lambda: health_at(launch_port) is None, 15, label="flat launcher exit")
        checks.append("vc-updater launch resolves flat root and waits")

        if candidate:
            valid1, b1, candidate_version, candidate_files = candidate_package(candidate, RUN)
            checks.append(
                f"real candidate re-zipped from extracted tree ({candidate_files} files, {candidate_version})"
            )
        else:
            valid1 = capture_package("ota-b1-" + uuid.uuid4().hex[:6], "0.1.0", capture_bytes, updater_bytes)
            b1 = json.loads(zipfile.ZipFile(io.BytesIO(valid1)).read(
                [n for n in zipfile.ZipFile(io.BytesIO(valid1)).namelist() if n.endswith("package-manifest.json")][0]
            ))["build_id"]

        status, _ = stage(valid1, token, declared_hash="0" * 64)
        assert status == 400, status
        leftovers = list((RUN / "updates/incoming").glob("upload-*.zip"))
        assert not leftovers, leftovers
        checks.append("wrong upload digest rejected and upload file removed")

        traversal = io.BytesIO()
        with zipfile.ZipFile(traversal, "w") as archive:
            archive.writestr("pkg/package-manifest.json", json.dumps({
                "product": PRODUCT, "schema": 1, "version": "0.1.0", "build_id": "evil",
                "files": [{"path": "a.txt", "bytes": 1, "sha256": "0" * 64}],
            }))
            archive.writestr("pkg/../evil.txt", b"x")
        status, _ = stage(traversal.getvalue(), token)
        assert status == 400, status
        checks.append("zip path traversal rejected")

        manifest_files = [
            {"path": "vc-capture.exe", "bytes": len(capture_bytes), "sha256": sha(capture_bytes)},
            {"path": "vc-updater.exe", "bytes": len(updater_bytes), "sha256": sha(updater_bytes)},
            {"path": "version.json", "bytes": 2, "sha256": "0" * 64},
        ]
        mismatch = package_path(
            "pkg", {
                "vc-capture.exe": capture_bytes,
                "vc-updater.exe": updater_bytes,
                "version.json": b"{}",
            },
            "ota-bad-hash", "0.1.0", manifest_files,
        )
        status, _ = stage(mismatch, token)
        assert status == 400, status
        assert not (RUN / "updates/staged/ota-bad-hash").exists()
        checks.append("per-file hash mismatch rejected without staging")

        status, value = stage(valid1, token)
        assert status == 200 and value["staged"]["build_id"] == b1, value
        checks.append("valid package staged")

        status, _ = activate("ota-missing-" + uuid.uuid4().hex[:4], token)
        assert status == 409, status
        status, value = activate(b1, token)
        assert status == 202, value
        baseline = json.loads((RUN / "updates/legacy-baseline.json").read_text(encoding="utf-8"))
        names = {entry["name"] for entry in baseline["entries"]}
        assert "vc-rust-config.json" in names, names
        assert not names & {"app", "data", "updates", "run"}, names
        assert baseline["exe"] and baseline["native_dir"], baseline
        checks.append("legacy rollback baseline recorded without copying data/config/run")
        checks.append("activation accepted")

        second = wait_for(lambda: (lambda h: h if h and h.get("build") == b1 else None)(health()),
                          60, label="first activated capture")
        pids.add(second["pid"])
        assert proc.poll() is not None, "old capture should exit after activation"
        assert json.loads((RUN / "app/current.json").read_text(encoding="utf-8"))["build_id"] == b1
        migrated = json.loads((RUN / "data/vc-rust-config.json").read_text(encoding="utf-8"))
        assert migrated["capture"]["fps"] == 77, migrated
        assert token_from(RUN / "data/update-token.json") == token
        _, config = request("/config", token=token)
        assert config["capture"]["fps"] == 77, config
        checks.append("atomic switch, config and token migration")

        managed_port = free_port()
        managed_launcher = subprocess.Popen(
            [str(updater_exe), "launch", "--root", str(RUN), "--role", "capture",
             "--bind", "127.0.0.1", "--port", str(managed_port)],
            stdout=log, stderr=log, creationflags=CREATE_NO_WINDOW,
        )
        managed = wait_for(lambda: health_at(managed_port), 20, label="launcher-spawned active release")
        assert managed["build"] == b1 and managed["layout"] == "managed", managed
        kill_tree(managed_launcher.pid)
        wait_for(lambda: health_at(managed_port) is None, 15, label="managed launcher exit")
        checks.append("vc-updater launch resolves active release")

        valid2 = capture_package("ota-b2-" + uuid.uuid4().hex[:6], "0.1.1", capture_bytes, updater_bytes)
        b2 = json.loads(zipfile.ZipFile(io.BytesIO(valid2)).read(
            [n for n in zipfile.ZipFile(io.BytesIO(valid2)).namelist() if n.endswith("package-manifest.json")][0]
        ))["build_id"]
        assert stage(valid2, token)[0] == 200
        staged_b2 = RUN / "updates/staged" / b2
        assert activate(b2, token)[0] == 202
        # 202 之后、helper 复核之前塞入清单外 DLL：必须被激活前复核拒绝并重启旧版。
        (staged_b2 / "evil.dll").write_bytes(b"MZ")
        wait_for(lambda: any(entry["build_id"] == b2 and entry["state"] == "rolled_back"
                             for entry in history_at(RUN)),
                 40, label="staged tamper rejection history")
        restarted = wait_for(lambda: (lambda h: h if h and h.get("build") == b1
                                      and h.get("pid") != second["pid"] else None)(health()),
                             60, label="old build restarted after staged tamper")
        pids.add(restarted["pid"])
        assert not (RUN / "app/releases" / b2).exists(), "tampered build must not install"
        wait_log(RUN, "failed code=4")
        checks.append("staged extra file rejected; old build restarted")

        (staged_b2 / "evil.dll").unlink()
        assert activate(b2, token)[0] == 202
        third = wait_for(lambda: (lambda h: h if h and h.get("build") == b2 else None)(health()),
                         60, label="second activated capture")
        pids.add(third["pid"])
        checks.append("activation succeeds after staged tamper removed")

        assert activate(b1, token, "/update/rollback")[0] == 202
        fourth = wait_for(lambda: (lambda h: h if h and h.get("build") == b1 else None)(health()),
                          60, label="explicit rollback capture")
        pids.add(fourth["pid"])
        checks.append("explicit rollback to previous release")

        valid3 = capture_package("ota-b3-" + uuid.uuid4().hex[:6], "0.1.2", b"not a windows executable", updater_bytes)
        b3 = json.loads(zipfile.ZipFile(io.BytesIO(valid3)).read(
            [n for n in zipfile.ZipFile(io.BytesIO(valid3)).namelist() if n.endswith("package-manifest.json")][0]
        ))["build_id"]
        assert stage(valid3, token)[0] == 200
        assert activate(b3, token)[0] == 202
        wait_for(lambda: any(entry["build_id"] == b3 and entry["state"] == "rolled_back"
                             for entry in history_at(RUN)),
                 40, label="failing build rollback history")
        fifth = wait_for(lambda: (lambda h: h if h and h.get("build") == b1 and h.get("pid") != fourth["pid"] else None)(health()),
                         60, label="old build restarted after failed activation")
        pids.add(fifth["pid"])
        pointer = json.loads((RUN / "app/current.json").read_text(encoding="utf-8"))
        assert pointer["build_id"] == b1, pointer
        wait_log(RUN, "failed code=5")
        checks.append("failed new build rolled back and old build restarted")

        # code 6：把指针设为只读，使恢复指针失败（重启旧版仍成功），确认报 6 并保留旧版本运行。
        assert activate(b2, token)[0] == 202
        sixth = wait_for(lambda: (lambda h: h if h and h.get("build") == b2 else None)(health()),
                         60, label="b2 active before code6 case")
        pids.add(sixth["pid"])
        pointer_path = RUN / "app/current.json"
        pointer_path.chmod(stat.S_IREAD)
        try:
            valid4 = capture_package("ota-b4-" + uuid.uuid4().hex[:6], "0.1.3", b"not a windows executable", updater_bytes)
            b4 = json.loads(zipfile.ZipFile(io.BytesIO(valid4)).read(
                [n for n in zipfile.ZipFile(io.BytesIO(valid4)).namelist() if n.endswith("package-manifest.json")][0]
            ))["build_id"]
            assert stage(valid4, token)[0] == 200
            assert activate(b4, token)[0] == 202
            wait_log(RUN, "failed code=6", 60)
            entry = wait_for(lambda: next((item for item in history_at(RUN)
                                           if item["build_id"] == b4 and item["state"] == "rollback_failed"), None),
                             30, label="code6 rollback_failed history")
            assert "恢复版本指针失败" in (entry["error"] or ""), entry
            assert json.loads(pointer_path.read_text(encoding="utf-8"))["build_id"] == b2
        finally:
            pointer_path.chmod(stat.S_IWRITE | stat.S_IREAD)
        restored = wait_for(lambda: (lambda h: h if h and h.get("build") == b2
                                     and h.get("pid") != sixth["pid"] else None)(health()),
                            60, label="old build restarted after pointer restore failure")
        pids.add(restored["pid"])
        checks.append("pointer restore failure reported as code 6 with old build restarted")

        report = {
            "ok": True,
            "scope": "private root, actual capture/updater processes, local Loopback-free control API",
            "candidate": str(candidate) if candidate else None,
            "layout": "legacy -> managed",
            "builds": {"first": b1, "second": b2, "failing": b3, "rollback_fail": b4},
            "checks": checks,
            "limits": ["No PC2 deployment", "No DShow registration", "Synthetic failure packages plus full candidate when --candidate is specified; binaries from --package or debug output"],
        }
        (RUN / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps({"run": str(RUN), **report}, ensure_ascii=False))
    finally:
        log.close()
        for pid in pids:
            kill_tree(pid)
        kill_under(RUN)
        kill_under(launch_root)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--package",
        type=Path,
        default=None,
        help="含 vc-capture.exe/vc-updater.exe 的目录（默认 rust/target/debug）",
    )
    parser.add_argument(
        "--candidate",
        type=Path,
        default=None,
        help="真实候选便携 ZIP：按 manifest.files 重算哈希并重打包，用作首个激活包",
    )
    args = parser.parse_args()
    main(args.package, args.candidate)
