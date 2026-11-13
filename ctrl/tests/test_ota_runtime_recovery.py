"""Failure recovery is isolated to a private fake OTA root, never the live agent."""
import importlib.util
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture
def agent(tmp_path, monkeypatch):
    spec = importlib.util.spec_from_file_location("ota_runtime_recovery", ROOT / "agent/vc-agent.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = str(tmp_path)
    module.BUILD_DIR = str(tmp_path / "build")
    module.RUN_DIR = str(tmp_path / "run")
    module.VERSION_FILE = str(tmp_path / "version.json")
    (tmp_path / "build").mkdir()
    stage = tmp_path / "staging/new"
    stage.mkdir(parents=True)
    (stage / "version.json").write_text('{"version":"new"}', encoding="utf-8")
    module.STAGED.update(version="new", dir=str(stage))
    monkeypatch.setattr(module, "kill_slot", lambda slot: None)
    module.state["vcam_managed"] = False
    return module


@pytest.mark.parametrize("present", [False, True])
def test_failed_activation_restores_runtime_bytes_or_absence(agent, tmp_path, monkeypatch, present):
    dll = tmp_path / "build/libpyrowave-shared-1.dll"
    if present:
        dll.write_bytes(b"old-runtime")

    def partial_apply(*_):
        dll.write_bytes(b"partial-new-runtime")
        raise OSError("injected after runtime copy")

    monkeypatch.setattr(agent, "_apply_staged_to_live", partial_apply)
    code, result = agent._do_activate("new", False)
    assert code == 500 and "live files restored" in result["error"]
    if present:
        assert dll.read_bytes() == b"old-runtime"
    else:
        assert not dll.exists()


def test_restore_skips_unchanged_locked_image(agent, tmp_path, monkeypatch):
    import shutil
    image = tmp_path / "build/vc-sender.exe"
    image.write_bytes(b"locked-running-image")
    snapshot = str(tmp_path / "prev/private-recovery")
    before = agent._snapshot_live_managed(snapshot)
    original = shutil.copy2

    def copy(src, dst, *args, **kwargs):
        if Path(dst) == image:
            pytest.fail("unchanged locked image must not be rewritten")
        return original(src, dst, *args, **kwargs)

    monkeypatch.setattr(shutil, "copy2", copy)
    agent._restore_live_managed(snapshot, before)
    assert image.read_bytes() == b"locked-running-image"


def test_restore_failure_retains_recovery_snapshot(agent, tmp_path, monkeypatch):
    dll = tmp_path / "build/libpyrowave-shared-1.dll"
    dll.write_bytes(b"old-runtime")

    def partial_apply(*_):
        dll.write_bytes(b"new-runtime")
        raise OSError("copy failed")

    def restore_failure(*_):
        raise OSError("restore also failed")

    monkeypatch.setattr(agent, "_apply_staged_to_live", partial_apply)
    monkeypatch.setattr(agent, "_restore_live_managed", restore_failure)
    code, result = agent._do_activate("new", False)
    assert code == 500 and "snapshot retained" in result["error"]
    snapshots = list((tmp_path / "prev").glob("activate-live-*/libpyrowave-shared-1.dll"))
    assert len(snapshots) == 1 and snapshots[0].read_bytes() == b"old-runtime"
