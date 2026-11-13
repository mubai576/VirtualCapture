"""MJPEG opt-in, exact session agreement and bounded native CPU backend."""
import importlib.util
import json
from pathlib import Path
import subprocess
from types import SimpleNamespace

import pytest

ROOT = Path(__file__).resolve().parents[2]
IDENTITY = "turbojpeg:cpu"
MFT = "{00000001-0002-0003-0405-060708090a0b}"


@pytest.fixture
def agent(tmp_path, monkeypatch):
    spec = importlib.util.spec_from_file_location("mjpeg_test_agent", ROOT / "agent/vc-agent.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = str(tmp_path)
    module.state["role"] = "host"
    monkeypatch.setattr(module, "stream_native_errors", lambda kind: [])
    return module


def capture(agent):
    cp = agent.load_config()["capture"]
    cp.update(codec="mjpeg", codec_experimental=True, encoder_backend=IDENTITY, decoder_backend=IDENTITY,
              encoder_hardware=False, decoder_hardware=False, jpeg_quality=83, gop_frames=1, decode_in_flight=1)
    return cp


def report(cp):
    return {"revision": 1, "configured": True, "frames_tested": 0, "stream_validated": False,
            "mjpeg_revision": 1, "encoder_configuration": {
                "family": "turbojpeg", "identity": IDENTITY, "version": "3.1.2", "quality": cp["jpeg_quality"],
                "width": cp["width"], "height": cp["height"], "fps": cp["fps"], "subsampling": "420",
                "dct": "fast_integer", "rate_control": "constant_quality", "target_bitrate_applied": False,
                "encoder_in_flight": 1, "config_submitted": True}}


@pytest.mark.parametrize("patch", [
    {"jpeg_quality": 0}, {"jpeg_quality": 101}, {"jpeg_quality": True}, {"jpeg_quality": 80.0},
    {"encoder_backend": MFT}, {"decoder_backend": MFT}, {"encoder_hardware": True},
    {"decoder_hardware": True}, {"gop_frames": 60}, {"codec": "h264"},
])
def test_invalid_mjpeg_configuration_does_not_persist(agent, tmp_path, patch):
    assert agent.save_config({"capture": dict(capture(agent), **patch)})[0] == 400
    assert not (tmp_path / "vc-config.json").exists()


def test_mjpeg_descriptor_requires_revision_and_exact_quality(agent):
    cp = capture(agent)
    session = agent.stream_session(cp)
    assert not agent.validate_stream_session(session, cp)
    assert session["mjpeg_revision"] == 1 and session["jpeg_quality"] == 83
    for key in ("mjpeg_revision", "jpeg_quality"):
        missing = dict(session)
        del missing[key]
        assert agent.validate_stream_session(missing, cp)
    for key, value in (("mjpeg_revision", True), ("jpeg_quality", 80), ("jpeg_quality", 83.0)):
        assert agent.validate_stream_session(dict(session, **{key: value}), cp)
    assert "mjpeg_revision" not in agent.codec_payload(cp)
    for kind in ("sender", "receiver"):
        cli = agent.codec_cli(cp, kind)
        assert cli[cli.index("--jpeg-quality")+1] == "83"
        assert cli[cli.index("--codec-hardware")+1] == "0"
    cp["codec"] = "raw"
    assert agent.codec_config(cp) == {}


@pytest.mark.parametrize("kind", ["sender", "receiver"])
def test_old_binary_rejected_before_backend_activation(agent, monkeypatch, kind):
    cp = capture(agent)
    monkeypatch.setattr(agent, "native_stream_capabilities", lambda side: {"codec_revision": 1, "codecs": ["raw"]})
    monkeypatch.setattr(agent, "native_codec_request", lambda *args: pytest.fail("old binary must fail before activation"))
    assert any("MJPEG" in error for error in agent.low_level_stream_errors(kind, {"codec": "mjpeg", **agent.codec_payload(cp)}))


@pytest.mark.parametrize("kind", ["sender", "receiver"])
@pytest.mark.parametrize("key,value", [("quality", 80), ("quality", 83.0), ("identity", MFT),
    ("fps", 60), ("width", 416), ("subsampling", "444"), ("rate_control", "cbr"),
    ("target_bitrate_applied", True), ("config_submitted", False)])
def test_native_check_rejects_ignored_or_changed_parameters(agent, monkeypatch, kind, key, value):
    cp = capture(agent)
    response = report(cp)
    response["encoder_configuration"][key] = value
    monkeypatch.setattr(agent, "exe_path", lambda side: str(ROOT / "agent/vc-agent.py"))
    monkeypatch.setattr(agent.subprocess, "run", lambda *args, **kwargs: SimpleNamespace(returncode=0, stdout=json.dumps(response)))
    assert "error" in agent.native_codec_request(kind, cp)


@pytest.mark.parametrize("peer", ["old", "mismatch", "ok"])
def test_peer_mjpeg_capability_and_quality_must_match(agent, monkeypatch, peer):
    cp = capture(agent)
    caps = {"codec_revision": 1, "codecs": ["raw", "mjpeg"], **({} if peer == "old" else {"mjpeg_revision": 1})}
    agent.PEER_CODEC_CAPABILITIES[(agent.ROOT, "")] = {"native": caps}
    monkeypatch.setattr(agent, "_stream_discovery", lambda *args: (None, None, [], []))
    monkeypatch.setattr(agent, "native_stream_capabilities", lambda kind: dict(caps, mjpeg_revision=1))
    monkeypatch.setattr(agent, "native_codec_request", lambda *args: report(cp))
    monkeypatch.setattr(agent, "peer_call", lambda *args, **kwargs: (200, report(cp) if peer == "ok" else {"configured": True}))
    code, result = agent.stream_plan({"capture": cp})
    assert code == 200
    assert result["can_start"] is (peer == "ok")


def native(kind, args):
    exe = ROOT / "build/Release" / f"vc-{kind}.exe"
    if not exe.exists(): pytest.skip("native build required")
    return subprocess.run([str(exe), *args], capture_output=True, timeout=12,
                          **({"creationflags": subprocess.CREATE_NO_WINDOW} if hasattr(subprocess, "CREATE_NO_WINDOW") else {}))


@pytest.mark.parametrize("kind", ["sender", "receiver"])
@pytest.mark.parametrize("quality", [1, 80, 100])
def test_native_mjpeg_is_explicit_and_reports_exact_parameters(kind, quality):
    response = native(kind, ["--codec-check", "--codec", "mjpeg", "--codec-backend", IDENTITY,
                            "--codec-hardware", "0", "--gop", "1", "--jpeg-quality", str(quality)])
    assert response.returncode == 0, response.stderr
    parsed = json.loads(response.stdout)
    assert parsed["mjpeg_revision"] == 1 and parsed["frames_tested"] == 0 and parsed["stream_validated"] is False
    assert parsed["encoder_configuration"]["quality"] == quality


@pytest.mark.parametrize("kind", ["sender", "receiver"])
@pytest.mark.parametrize("extra", [["--jpeg-quality", "0"], ["--jpeg-quality", "101"],
    ["--jpeg-quality", "80.5"], ["--codec-hardware", "1"], ["--codec-backend", MFT],
    ["--gop", "60"], ["--decode-in-flight", "9"]])
def test_invalid_native_mjpeg_fails_before_nic(kind, extra):
    response = native(kind, ["--codec", "mjpeg", "--codec-backend", IDENTITY, "--gop", "1", *extra])
    assert response.returncode == 2, response.stdout + response.stderr


@pytest.mark.parametrize("kind", ["sender", "receiver"])
@pytest.mark.parametrize("codec", ["raw", "av1"])
def test_other_codecs_reject_jpeg_quality(kind, codec):
    response = native(kind, ["--codec", codec, "--codec-backend", MFT, "--jpeg-quality", "80"])
    assert response.returncode == 2


@pytest.mark.parametrize("kind", ["sender", "receiver"])
def test_native_mjpeg_nv12_and_corrupt_input_selftest(kind):
    response = native(kind, ["--mjpeg-selftest"])
    assert response.returncode == 0, response.stdout + response.stderr
    assert b"bounded output" in response.stdout
    caps = json.loads(native(kind, ["--capabilities"]).stdout)
    assert caps["mjpeg_revision"] == 1 and "mjpeg" in caps["codecs"]


def test_mjpeg_diagnostic_options_are_not_silently_ignored():
    spec = importlib.util.spec_from_file_location("mjpeg_diagnostic", ROOT / "scripts/p6_codec_verify.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    assert module.mjpeg_options("mjpeg", IDENTITY, IDENTITY, False, False, 1, None) == 80
    for args in [("av1", IDENTITY, IDENTITY, False, False, 1, 80),
                 ("mjpeg", IDENTITY, IDENTITY, True, False, 1, 80),
                 ("mjpeg", IDENTITY, IDENTITY, False, False, 15, 80),
                 ("mjpeg", IDENTITY, IDENTITY, False, False, 0, 1),
                 ("mjpeg", IDENTITY, IDENTITY, False, False, 1, 101)]:
        with pytest.raises(ValueError): module.mjpeg_options(*args)
