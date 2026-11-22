"""End-to-end OTA smoke test on a private temporary root and random port.

Stages synthetic `unified-v1` ZIPs (and a real candidate when supplied) through
`VirtualCapture.exe capture` and its token-protected REST API. Exercises the real
updater, activation, rollback, and failed activation. No registry, Npcap capture
or deployed installation is touched.
"""
import argparse
import ctypes
import hashlib
import io
import json
from pathlib import Path
import signal
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
ENTRYPOINT = "unified-v1"
APP_EXE = "VirtualCapture.exe"
UPDATER_EXE = "vc-updater.exe"
MANIFEST_NAME = "package-manifest.json"


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


def manifest_entries(files):
    return [{"path": name, "bytes": len(data), "sha256": sha(data)} for name, data in files.items()]


def package_manifest(files, build_id, version, manifest_files=None, entrypoint=ENTRYPOINT):
    manifest = {
        "product": PRODUCT,
        "schema": 1,
        "capabilities": {"role_launch_args": True, "desktop_capture": True},
        "version": version,
        "build_id": build_id,
        "files": manifest_entries(files) if manifest_files is None else manifest_files,
    }
    if entrypoint is not None:
        manifest["entrypoint"] = entrypoint
    return manifest


def package_path(root_name, files, build_id, version, manifest_files=None, entrypoint=ENTRYPOINT):
    manifest = package_manifest(files, build_id, version, manifest_files, entrypoint)
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(f"{root_name}/{MANIFEST_NAME}", json.dumps(manifest))
        for name, data in files.items():
            archive.writestr(f"{root_name}/{name}", data)
    return buffer.getvalue()


def candidate_package(zip_path):
    """Check the candidate's original inventory and hashes, then stage it unchanged."""
    data = Path(zip_path).read_bytes()
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        names = [item.filename for item in archive.infolist() if not item.is_dir()]
        manifests = [name for name in names if name.rsplit("/", 1)[-1] == MANIFEST_NAME]
        assert len(manifests) == 1, "candidate ZIP needs exactly one package-manifest.json"
        manifest_name = manifests[0]
        assert manifest_name.count("/") <= 1, f"unexpected candidate ZIP root: {manifest_name}"
        for name in names:
            parts = name.split("/")
            assert not any(part in ("", ".", "..") for part in parts), f"unsafe candidate entry: {name}"
            assert not any(char in name for char in "\\:"), f"unsafe candidate entry: {name}"
            assert parts[-1].lower() not in {"update-token.json", "update-peer-token.json"}, (
                f"credential file in candidate: {name}")
        manifest = json.loads(archive.read(manifest_name))
        assert manifest["product"] == PRODUCT and manifest["schema"] == 1, manifest_name
        assert manifest["entrypoint"] == ENTRYPOINT, manifest_name
        assert manifest["capabilities"] == {"role_launch_args": True, "desktop_capture": True}, manifest_name
        prefix = manifest_name[:-len(MANIFEST_NAME)]
        declared = [item["path"] for item in manifest["files"]]
        assert len({name.lower() for name in declared}) == len(declared), "duplicate candidate manifest path"
        assert {name for name in declared if name.lower().endswith(".exe")} == {APP_EXE, UPDATER_EXE}, declared
        assert not any(name.lower().startswith("scripts/") or name.lower().endswith((".cmd", ".ps1", ".bat", ".py"))
                       for name in declared), declared
        wanted = {manifest_name, *(prefix + name for name in declared)}
        assert len(names) == len(wanted) and set(names) == wanted, "candidate ZIP inventory differs from manifest"
        for item in manifest["files"]:
            info = archive.getinfo(prefix + item["path"])
            assert info.file_size == item["bytes"], f"candidate size mismatch: {item['path']}"
            with archive.open(info) as stream:
                assert hashlib.file_digest(stream, "sha256").hexdigest() == item["sha256"], (
                    f"candidate hash mismatch: {item['path']}")
    return data, manifest["build_id"], manifest["version"], len(declared)


def capture_payload(version, binaries):
    return {**binaries, "version.json": json.dumps({"product": "VirtualCapture-Rust", "version": version}).encode()}


def capture_package(build_id, version, binaries):
    return package_path(f"VirtualCapture-Rust-{version}-windows-x64-{build_id}",
                        capture_payload(version, binaries), build_id, version)


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


def expect_capture(value, root, build_id, pid=None):
    assert value["ok"] is True and value["implementation"] == "virtualcapture-rust", value
    assert value["role"] == "capture" and value["api_revision"] == 2, value
    assert value["build"] == build_id and value["layout"] == ("managed" if build_id else "legacy"), value
    assert Path(value["root"]).samefile(root), value
    if pid is not None:
        assert value["pid"] == pid, value


def check_updater_launch(updater, root, build_id, log, pids, timeout):
    # The foreground helper and its capture child must share this new console group.
    port = free_port()
    launcher = subprocess.Popen(
        [str(updater), "launch", "--root", str(root), "--role", "capture",
         "--bind", "127.0.0.1", "--port", str(port)],
        stdout=log, stderr=log, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
    )
    try:
        launched = wait_for(lambda: health_at(port), timeout, label="updater-launched capture")
        expect_capture(launched, root, build_id)
        pids.add(launched["pid"])
        assert launched["pid"] != launcher.pid and launcher.poll() is None, launched
        launcher.send_signal(signal.CTRL_BREAK_EVENT)
        assert launcher.wait(timeout=15) == 0, "vc-updater did not return the capture's graceful exit"
        wait_for(lambda: health_at(port) is None, 15, label="updater-launched capture exit")
        pids.discard(launched["pid"])
    finally:
        if launcher.poll() is None:
            kill_tree(launcher.pid)
            launcher.wait(timeout=10)


def check_copied_helper(root, release, log_path):
    log_path = Path(log_path)
    assert log_path.parent.samefile(root / "updates/worker"), log_path
    assert log_path.name.startswith("updater-") and log_path.suffix == ".log", log_path
    helper_dir = log_path.parent / log_path.stem.replace("updater-", "helper-", 1)
    for name in (UPDATER_EXE, "vcruntime140.dll", "vcruntime140_1.dll"):
        bundled = release / name
        copied = helper_dir / name
        assert bundled.is_file() and copied.is_file(), (bundled, copied)
        assert copied.read_bytes() == bundled.read_bytes(), f"activation helper payload mismatch: {copied}"


def main(package_dir=None, candidate=None):
    global PORT, BIN
    if package_dir:
        BIN = Path(package_dir).resolve()
    PORT = free_port()
    capture_exe = BIN / APP_EXE
    updater_exe = BIN / UPDATER_EXE
    required = (APP_EXE, UPDATER_EXE, "vc_stream.dll")
    missing = [name for name in required if not (BIN / name).is_file()]
    assert not missing, f"Missing {missing} in {BIN}; build the workspace or pass --package"
    runtime = ("libpyrowave-shared-1.dll", "msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll")
    binaries = {name: (BIN / name).read_bytes() for name in required}
    binaries.update({name: (BIN / name).read_bytes() for name in runtime if (BIN / name).is_file()})
    kernel = ctypes.windll.kernel32
    if not kernel.GetConsoleCP():
        assert kernel.AllocConsole(), "Could not allocate a console for the updater Ctrl-Break check"
        kernel.GetConsoleWindow.restype = ctypes.c_void_p
        ctypes.windll.user32.ShowWindow(ctypes.c_void_p(kernel.GetConsoleWindow()), 0)
    legacy_config = {"version": 1, "peer_url": "", "capture": {"fps": 77, "mode": "pattern", "gpu_convert": False}}
    (RUN / "vc-rust-config.json").write_text(json.dumps(legacy_config), encoding="utf-8")
    checks = []
    pids = set()
    launch_root = RUN.parent / (RUN.name + "-launch")
    log = (RUN / "capture.log").open("wb")
    try:
        proc = subprocess.Popen(
            [str(capture_exe), "capture", "--root", str(RUN), "--native-dir", str(BIN),
             "--bind", "127.0.0.1", "--port", str(PORT)],
            stdout=log, stderr=log, creationflags=CREATE_NO_WINDOW,
        )
        pids.add(proc.pid)
        first = wait_for(health, 15, label="first capture health")
        expect_capture(first, RUN, None, proc.pid)
        checks.append("unified capture starts directly in a flat ROOT (legacy data layout)")

        status, _ = request("/update/status")
        assert status == 401, status
        token_path = RUN / "update-token.json"
        token = token_from(token_path)
        status, value = request("/update/status", token=token)
        assert status == 200 and value["layout"] == "legacy" and value["active"] is None, value
        checks.append("update token required")

        printed = subprocess.run(
            [str(capture_exe), "capture", "--root", str(RUN), "--native-dir", str(BIN), "--print-update-token"],
            capture_output=True, encoding="utf-8", timeout=20, check=True,
        ).stdout.splitlines()
        label = "update token file: "
        assert len(printed) == 2 and printed[0].startswith(label) and printed[1] == token, (
            "print-update-token did not return the fixture's token")
        assert Path(printed[0][len(label):]).samefile(token_path)
        log.flush()
        assert token not in (RUN / "capture.log").read_text(errors="replace")
        checks.append("pairing token explicitly printable, never logged")

        launch_root.mkdir()
        flat_files = capture_payload("0.1.0", binaries)
        for name, data in flat_files.items():
            (launch_root / name).write_bytes(data)
        (launch_root / MANIFEST_NAME).write_text(
            json.dumps(package_manifest(flat_files, "ota-flat", "0.1.0")), encoding="utf-8")
        check_updater_launch(updater_exe, launch_root, None, log, pids, timeout=15)
        checks.append("vc-updater launch dispatches declared flat unified capture, waits and exits on Ctrl-Break")

        if candidate:
            valid1, b1, candidate_version, candidate_files = candidate_package(candidate)
            checks.append(
                f"real candidate marker, inventory and original hashes verified ({candidate_files} files, {candidate_version})"
            )
        else:
            b1 = "ota-b1-" + uuid.uuid4().hex[:6]
            valid1 = capture_package(b1, "0.1.0", binaries)

        status, _ = stage(valid1, token, declared_hash="0" * 64)
        assert status == 400, status
        leftovers = list((RUN / "updates/incoming").glob("upload-*.zip"))
        assert not leftovers, leftovers
        checks.append("wrong upload digest rejected and upload file removed")

        # The rejection fixtures have correct per-file hashes unless the case
        # specifically tests hash validation. None of these images are executed.
        stub_files = capture_payload("0.1.0", {APP_EXE: b"MZ", UPDATER_EXE: b"MZ"})
        traversal = io.BytesIO()
        with zipfile.ZipFile(traversal, "w") as archive:
            archive.writestr(f"pkg/{MANIFEST_NAME}", json.dumps(package_manifest(stub_files, "evil", "0.1.0")))
            for name, data in stub_files.items():
                archive.writestr(f"pkg/{name}", data)
            archive.writestr("pkg/../evil.txt", b"x")
        status, _ = stage(traversal.getvalue(), token)
        assert status == 400, status
        checks.append("zip path traversal rejected")

        mismatch_files = {**stub_files, "version.json": b"{}"}
        manifest_files = manifest_entries(mismatch_files)
        next(item for item in manifest_files if item["path"] == "version.json")["sha256"] = "0" * 64
        mismatch = package_path("pkg", mismatch_files, "ota-bad-hash", "0.1.0", manifest_files)
        status, _ = stage(mismatch, token)
        assert status == 400, status
        assert not (RUN / "updates/staged/ota-bad-hash").exists()
        checks.append("per-file hash mismatch rejected without staging")

        for build_id, files, entrypoint in (
            ("no-entrypoint", stub_files, None),
            ("unknown-entrypoint", stub_files, "unified-v2"),
            ("third-exe", {**stub_files, "vc-vcam-verify.exe": b"MZ"}, ENTRYPOINT),
            ("nested-exe", {UPDATER_EXE: b"MZ", f"bin/{APP_EXE}": b"MZ",
                            "version.json": stub_files["version.json"]}, ENTRYPOINT),
            ("legacy-cmd", {**stub_files, "Start-Capture.cmd": b"rem fixture"}, ENTRYPOINT),
            ("legacy-ps1", {**stub_files, "scripts/Start-Capture.ps1": b"# fixture"}, ENTRYPOINT),
        ):
            bad_package = package_path("pkg", files, build_id, "0.1.0", entrypoint=entrypoint)
            status, value = stage(bad_package, token)
            assert status == 400, (build_id, status, value)
            assert not (RUN / "updates/staged" / build_id).exists(), build_id
        checks.append("missing/unknown unified marker, extra/nested EXE and legacy scripts rejected")

        status, value = stage(valid1, token)
        assert status == 200 and value["staged"]["build_id"] == b1, value
        checks.append("valid package staged")

        # D34 stages new manifests without copying their capability snapshot.
        staged_meta_path = RUN / "updates/staged" / f"{b1}.json"
        staged_meta = json.loads(staged_meta_path.read_text(encoding="utf-8"))
        staged_meta.pop("capabilities", None)
        staged_meta_path.write_text(json.dumps(staged_meta), encoding="utf-8")

        status, _ = activate("ota-missing-" + uuid.uuid4().hex[:4], token)
        assert status == 409, status
        status, value = activate(b1, token)
        assert status == 202 and value["activating"]["build_id"] == b1, value
        first_activation_log = value["activating"]["log"]
        baseline = json.loads((RUN / "updates/legacy-baseline.json").read_text(encoding="utf-8"))
        names = {entry["name"] for entry in baseline["entries"]}
        assert "vc-rust-config.json" in names, names
        assert not names & {"app", "data", "updates", "run"}, names
        assert baseline["exe"] and baseline["native_dir"], baseline
        assert Path(baseline["exe"]).samefile(capture_exe), baseline
        baseline_native = Path(baseline["native_dir"])
        assert baseline_native.samefile(BIN) and (baseline_native / "vc_stream.dll").is_file(), baseline
        checks.append("legacy rollback baseline native_dir carries vc_stream.dll")
        checks.append("legacy rollback baseline recorded without copying data/config/run")
        checks.append("activation accepted")

        second = wait_for(lambda: (lambda h: h if h and h.get("build") == b1 else None)(health()),
                          60, label="first activated capture")
        expect_capture(second, RUN, b1)
        pids.add(second["pid"])
        assert proc.wait(timeout=5) == 0, "old capture should exit gracefully after activation"
        pids.discard(proc.pid)
        pointer = json.loads((RUN / "app/current.json").read_text(encoding="utf-8"))
        assert pointer["schema"] == 1 and pointer["build_id"] == b1, pointer
        release = RUN / "app/releases" / b1
        assert {item.relative_to(release).as_posix() for item in release.rglob("*.exe")} == {APP_EXE, UPDATER_EXE}
        assert (release / "vc_stream.dll").is_file(), "activated release must carry vc_stream.dll"
        migrated = json.loads((RUN / "data/vc-rust-config.json").read_text(encoding="utf-8"))
        assert migrated["capture"]["fps"] == 77, migrated
        assert token_from(RUN / "data/update-token.json") == token
        _, config = request("/config", token=token)
        assert config["capture"]["fps"] == 77, config
        checks.append("atomic switch, config and token migration")
        checks.append("activated release contains vc_stream.dll")
        assert "capabilities" not in json.loads(staged_meta_path.read_text(encoding="utf-8"))
        checks.append("D34 staged metadata without capabilities activates using the verified new manifest")

        wait_for(lambda: any(entry["build_id"] == b1 and entry["state"] == "active"
                             for entry in history_at(RUN)), 40, label="first updater success")
        if candidate:
            check_copied_helper(RUN, release, first_activation_log)
            checks.append("isolated activation helper and both CRT DLLs match the installed candidate")
        root_entry = RUN / APP_EXE
        assert root_entry.read_bytes() == (release / APP_EXE).read_bytes(), "OTA must install the unified ROOT entry"
        forwarded = subprocess.run(
            [str(root_entry), "capture", "--root", str(RUN), "--print-config"],
            capture_output=True, encoding="utf-8", timeout=20, check=True,
        )
        assert json.loads(forwarded.stdout) == config, "ROOT entry did not forward capture diagnostic arguments"
        checks.append("installed ROOT EXE forwards capture diagnostics to the active release")

        check_updater_launch(updater_exe, RUN, b1, log, pids, timeout=20)
        checks.append("vc-updater launch resolves active unified capture, waits and exits on Ctrl-Break")

        b2 = "ota-b2-" + uuid.uuid4().hex[:6]
        valid2 = capture_package(b2, "0.1.1", binaries)
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
        expect_capture(third, RUN, b2)
        pids.add(third["pid"])
        checks.append("activation succeeds after staged tamper removed")

        stale = subprocess.run(
            [str(release / APP_EXE), "capture", "--root", str(RUN), "--native-dir", str(release),
             "--bind", "127.0.0.1", "--port", str(free_port()), "--print-config"],
            capture_output=True, encoding="utf-8", timeout=20, creationflags=CREATE_NO_WINDOW,
        )
        assert stale.returncode == 2 and not stale.stdout.strip() and "当前版本" in stale.stderr, stale
        checks.append("direct stale release launch rejects the active pointer mismatch")

        assert activate(b1, token, "/update/rollback")[0] == 202
        fourth = wait_for(lambda: (lambda h: h if h and h.get("build") == b1 else None)(health()),
                          60, label="explicit rollback capture")
        pids.add(fourth["pid"])
        checks.append("explicit rollback to previous release")

        failing_binaries = {**binaries, APP_EXE: b"not a windows executable"}
        b3 = "ota-b3-" + uuid.uuid4().hex[:6]
        valid3 = capture_package(b3, "0.1.2", failing_binaries)
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
            b4 = "ota-b4-" + uuid.uuid4().hex[:6]
            valid4 = capture_package(b4, "0.1.3", failing_binaries)
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
            "limits": [
                "No PC2 deployment or DShow registration",
                "Flat data layout migration does not exercise an old split-binary release",
                "Synthetic failure packages; --candidate stages the original verified portable ZIP",
            ],
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
        help="含 VirtualCapture.exe、vc-updater.exe 和 vc_stream.dll 的目录（默认 rust/target/debug）",
    )
    parser.add_argument(
        "--candidate",
        type=Path,
        default=None,
        help="真实候选便携 ZIP：验证原清单、文件长度和 SHA-256 后，原样暂存并首次激活",
    )
    args = parser.parse_args()
    main(args.package, args.candidate)
