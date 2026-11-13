"""Package failure/rollback and portable-layout regression checks; no build or device access."""
import hashlib
import importlib.util
import json
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("vc_package", ROOT / "scripts/package.py")
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)
paths_spec = importlib.util.spec_from_file_location("vc_runtime_paths", ROOT / "ctrl/runtime_paths.py")
paths = importlib.util.module_from_spec(paths_spec)
paths_spec.loader.exec_module(paths)


def put(root, name, text="data"):
    p = root / name
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")
    return p


def managed(root):
    put(root, package.MARKER, json.dumps({"product": package.PRODUCT}))


@pytest.mark.parametrize("relative", [".", "..", "run", "staging", "prev", "sender/output", "dist"])
def test_package_rejects_source_and_live_directories(tmp_path, relative):
    root = tmp_path / "repo"
    root.mkdir()
    with pytest.raises(package.PackageError):
        package.validate_output(root, root / relative)


def test_package_refuses_unmanaged_output_without_touching_it(tmp_path):
    root = tmp_path / "repo"
    output = root / "dist/VirtualCapture"
    evidence = put(output, "user-data.txt", "keep me")
    with pytest.raises(package.PackageError, match="拒绝覆盖"):
        package.validate_output(root, output)
    assert evidence.read_text() == "keep me"


def test_package_support_files_exclude_caches_tests_and_build_scripts(tmp_path):
    root, stage = tmp_path / "repo", tmp_path / "payload"
    stage.mkdir()
    for name in ("version.json", "packaging/Start-VirtualCapture.cmd", "packaging/Start-Capture-Worker.cmd", "docs/runbooks/package-quickstart.md", "docs/runbooks/mjpeg.md",
                 "ctrl/run_remote.py", "ctrl/tests/test_private.py", "ctrl/__pycache__/x.pyc",
                 "scripts/runtime-paths.ps1", "scripts/verify.py", "scripts/package.py", "scripts/p6_raw_verify.py", "scripts/p6_product_verify.py",
                 "vc-config.json", "run/customer.hash.jsonl"):
        put(root, name)
    package.copy_support_files(root, stage)
    assert (stage / "ctrl/run_remote.py").is_file()
    assert (stage / "docs/runbooks/mjpeg.md").is_file(), "Static MJPEG attribution must ship with the portable package"
    assert (stage / "scripts/runtime-paths.ps1").is_file()
    # A BOM is required for Windows PowerShell 5.1 to parse non-ASCII scripts.
    assert (stage / "scripts/runtime-paths.ps1").read_bytes().startswith(b"\xef\xbb\xbf")
    for excluded in ("ctrl/tests", "ctrl/__pycache__", "scripts/package.py", "scripts/verify.py",
                     "scripts/p6_raw_verify.py", "scripts/p6_product_verify.py", "vc-config.json", "run"):
        assert not (stage / excluded).exists()


def test_package_rejects_incomplete_payload(tmp_path):
    for name in (*package.NATIVE_FILES, *package.RUNTIME_FILES, "vc-agent.exe", "version.json",
                 "Start-VirtualCapture.cmd", "README.md"):
        if name != "vc-vcam.dll":
            put(tmp_path, name)
    with pytest.raises(package.PackageError, match="vc-vcam.dll"):
        package.require_payload(tmp_path)


def test_package_requires_static_mjpeg_license_notices(tmp_path):
    for name in (*package.NATIVE_FILES, *package.RUNTIME_FILES, "vc-agent.exe", "version.json",
                 "Start-VirtualCapture.cmd", "Start-Capture-Worker.cmd", "README.md",
                 "scripts/start-agent.ps1", "scripts/start-host.ps1", "scripts/start-capture-worker.ps1"):
        put(tmp_path, name)
    with pytest.raises(package.PackageError, match="libjpeg-turbo"):
        package.require_payload(tmp_path)
    for name in package.LICENSE_FILES:
        put(tmp_path, name)
    package.require_payload(tmp_path)


def test_package_manifest_records_content_and_dirty_source(tmp_path):
    put(tmp_path, "vc-agent.exe", "built binary")
    source = {"revision": "abc", "dirty": True, "files": {"agent/vc-agent.py": "source hash"}}
    package.write_manifest(tmp_path, source, "0.4.1")
    manifest = json.loads((tmp_path / package.MARKER).read_text(encoding="utf-8"))
    assert manifest["source"] == source
    assert manifest["files"] == [{"path": "vc-agent.exe", "bytes": 12,
                                   "sha256": hashlib.sha256(b"built binary").hexdigest()}]


def test_package_archives_previous_payload_and_runtime_evidence(tmp_path):
    output, stage = tmp_path / "product", tmp_path / "new"
    managed(output)
    put(output, "run/evidence.jsonl", "old evidence")
    put(stage, "vc-agent.exe", "new binary")
    backup = package.publish(stage, output)
    assert (backup / "run/evidence.jsonl").read_text() == "old evidence"
    assert (output / "vc-agent.exe").read_text() == "new binary"


def test_package_failed_publish_restores_previous_payload(tmp_path, monkeypatch):
    output, stage = tmp_path / "product", tmp_path / "new"
    managed(output)
    put(output, "run/evidence.jsonl", "keep evidence")
    put(stage, "vc-agent.exe", "new binary")
    original = Path.rename

    def blocked(self, target):
        if self == stage:
            raise PermissionError("simulated file lock")
        return original(self, target)

    monkeypatch.setattr(Path, "rename", blocked)
    with pytest.raises(PermissionError):
        package.publish(stage, output)
    assert (output / "run/evidence.jsonl").read_text() == "keep evidence"
    assert not (output / "vc-agent.exe").exists()


def test_binary_lookup_supports_package_and_ota_layouts(tmp_path):
    root_binary = put(tmp_path, "vc-testwin.exe")
    assert paths.find_binary("vc-testwin.exe", tmp_path) == str(root_binary)
    release_binary = put(tmp_path, "build/Release/vc-testwin.exe")
    assert paths.find_binary("vc-testwin.exe", tmp_path) == str(release_binary)
    ota_binary = put(tmp_path, "build/vc-testwin.exe")
    assert paths.find_binary("vc-testwin.exe", tmp_path) == str(ota_binary)
