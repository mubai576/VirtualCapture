"""P6-2 session negotiation and native variable-geometry regression (no registration/OTA)."""
import importlib.util
import json
from pathlib import Path
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture
def agent(tmp_path, monkeypatch):
    spec = importlib.util.spec_from_file_location("p6_native_agent", ROOT / "agent/vc-agent.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = str(tmp_path)
    module.state["role"] = "host"
    return module


def test_sessions_are_explicit_exact_and_unique(agent):
    capture = agent.load_config()["capture"]
    descriptor = agent.stream_session(capture)
    assert not agent.validate_stream_session(descriptor, capture)
    assert agent.stream_session(capture)["source_mac"] != descriptor["source_mac"]
    assert agent.validate_stream_session(None, capture)
    for field, value in (("width", 416), ("height", 416), ("fps", 60), ("mtu", 1500),
                         ("codec", "h264"), ("pixel_format", "rgb24"), ("revision", True),
                         ("transform", "fit"), ("source_mac", "ff:ff:ff:ff:ff:ff")):
        assert agent.validate_stream_session(dict(descriptor, **{field: value}), capture), field
    assert agent.validate_stream_session(dict(descriptor, unexpected=1), capture)


def test_native_probe_rejects_old_binary_and_invalid_json(agent, monkeypatch, tmp_path):
    executable = tmp_path / "old.exe"
    executable.touch()
    monkeypatch.setattr(agent, "exe_path", lambda kind: str(executable))
    class Result:
        returncode = 0
        stdout = '{"stream_revision":1}'
    monkeypatch.setattr(agent.subprocess, "run", lambda *a, **k: Result())
    assert "error" in agent.native_stream_capabilities("sender")
    Result.stdout = "not json"
    assert "error" in agent.native_stream_capabilities("sender")
    Result.stdout = json.dumps(dict(stream_revision=2, shm_version=3, variable_resolution=True,
                                    dda_fps_limit=True, codecs=["raw"]))
    assert agent.native_stream_capabilities("sender")["dda_fps_limit"] is True
    monkeypatch.setattr(agent.subprocess, "run", lambda *a, **k: pytest.fail("unchanged executable must use cache"))
    assert agent.native_stream_capabilities("sender")["shm_version"] == 3


def test_rejected_session_never_registers_or_stops_existing_process(agent, monkeypatch):
    monkeypatch.setattr(agent, "stream_native_errors", lambda kind: [])
    monkeypatch.setattr(agent, "vcam_register", lambda: pytest.fail("must reject before registration"))
    monkeypatch.setattr(agent, "launch_product", lambda *a: pytest.fail("must reject before launch/kill"))
    agent.save_config({"adapter": "Loopback"})
    descriptor = agent.stream_session(agent.load_config()["capture"])
    assert agent.product_local_start({"stream": dict(descriptor, width=416)})[0] == 409
    agent.state["role"] = "capture"
    assert agent.product_local_start({"capture": {"width": 416, "height": 416}})[0] == 409


def test_new_options_forward_only_after_capability_confirmation(agent, monkeypatch, tmp_path):
    errors = ["old sender"]
    monkeypatch.setattr(agent, "stream_native_errors", lambda kind: errors[:])
    assert agent.low_level_stream_errors("sender", {"width": 416, "height": 416}) == errors
    errors.clear()
    assert not agent.low_level_stream_errors("sender", {"width": 416, "height": 416, "transform": "fit"})
    assert agent.low_level_stream_errors("sender", {"width": 416})
    assert agent.low_level_stream_errors("receiver", {"width": 416, "height": 416, "transform": "fill"})
    assert agent.low_level_stream_errors("sender", {"codec": "h264"})
    exe = tmp_path / "native.exe"
    exe.touch()
    monkeypatch.setattr(agent, "exe_path", lambda kind: str(exe))
    monkeypatch.setattr(agent, "RUN_DIR", str(tmp_path / "run"))
    calls = []
    class Process:
        pid = 123
    monkeypatch.setattr(agent.subprocess, "Popen", lambda cmd, **kwargs: (calls.append(cmd) or Process()))
    for kind in ("sender", "receiver"):
        args = {"adapter": "Loopback", "width": 418, "height": 282, "fps": 120,
                "source_mac": "02:12:34:56:78:ab"}
        if kind == "sender": args["transform"] = "fill"
        agent.launch(kind, args)
        command = calls[-1]
        for flag, value in (("--width", "418"), ("--height", "282"), ("--fps", "120"),
                            ("--source-mac", args["source_mac"])):
            assert command[command.index(flag)+1] == value


def test_old_peer_is_rejected_before_registration(agent, monkeypatch):
    agent.save_config({"adapter": "Loopback", "peer_url": "http://192.168.2.59:5143"})
    monkeypatch.setattr(agent, "peer_call", lambda *a, **k: (200, {"product_revision": 2, "product_role": "capture"}))
    monkeypatch.setattr(agent, "vcam_register", lambda: pytest.fail("old peer must never reach registration"))
    assert agent.product_start()[0] == 409


@pytest.fixture(scope="module")
def native_vectors():
    exe = ROOT / "build/Release/vc-sender.exe"
    if not exe.exists(): pytest.skip("native build required")
    result = subprocess.run([str(exe), "--selftest"], capture_output=True, text=True, encoding="utf-8", timeout=60)
    assert result.returncode == 0, result.stdout + result.stderr
    return result.stdout


def test_variable_native_patterns_match_independent_python_reference(native_vectors, monkeypatch):
    monkeypatch.syspath_prepend(str(ROOT / "ctrl"))
    from pattern_ref import gen_native, frame_hash_yuv
    vectors = [line.split() for line in native_vectors.splitlines() if line.startswith("VEC variable ")]
    assert len(vectors) == 40
    for _, _, geometry, pattern, fid, expected in vectors:
        width, height = map(int, geometry.split("x"))
        assert frame_hash_yuv(*gen_native(pattern, int(fid), width, height)) == expected
    assert "DDA-GPU: PASS 16 cases bit-exact" in native_vectors


@pytest.mark.parametrize("kind", ["sender", "receiver"])
@pytest.mark.parametrize("args", [["--width"], ["--width", "416.5"], ["--width", "2147483650"],
                                  ["--width", "417"], ["--fps", "0"], ["--height", "-2"]])
def test_bad_native_geometry_exits_without_opening_adapter(kind, args):
    exe = ROOT / f"build/Release/vc-{kind}.exe"
    if not exe.exists(): pytest.skip("native build required")
    result = subprocess.run([str(exe), *args], capture_output=True, timeout=5)
    assert result.returncode == 2, result.stdout + result.stderr


@pytest.mark.parametrize("mac", ["02:00:00:00:00:100", "02:00:00:00:00:01suffix"])
def test_bad_destination_mac_is_not_truncated(mac):
    exe = ROOT / "build/Release/vc-sender.exe"
    if not exe.exists(): pytest.skip("native build required")
    result = subprocess.run([str(exe), "--dst-mac", mac], capture_output=True, timeout=5)
    assert result.returncode == 2
