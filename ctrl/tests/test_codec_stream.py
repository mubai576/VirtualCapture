"""P6-3 explicit codec sessions and payload integrity; no registry, OTA or live capture."""
import importlib.util
import json
from pathlib import Path
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[2]
MISSING = "{00000001-0002-0003-0405-060708090a0b}"


@pytest.fixture
def agent(tmp_path, monkeypatch):
    spec = importlib.util.spec_from_file_location("codec_stream_agent", ROOT / "agent/vc-agent.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = str(tmp_path)
    module.state["role"] = "host"
    monkeypatch.setattr(module, "stream_native_errors", lambda kind: [])
    return module


def configured(agent):
    capture = agent.load_config()["capture"]
    capture.update(codec="hevc", codec_experimental=True, encoder_backend=MISSING,
                   decoder_backend="name:explicit-decoder", max_bitrate_mbps=80)
    return capture


def test_codec_descriptors_echo_every_explicit_choice(agent):
    capture = configured(agent)
    descriptor = agent.stream_session(capture)
    assert not agent.validate_stream_session(descriptor, capture)
    assert descriptor["codec_revision"] == 1
    for key in agent.CODEC_FIELDS:
        changed = dict(descriptor)
        del changed[key]
        assert agent.validate_stream_session(changed, capture), key
    for key, value in (("decode_in_flight", 1), ("encoder_hardware", 1), ("gop_frames", 30),
                       ("max_au_bytes", 4096), ("max_bitrate_mbps", 100), ("decoder_backend", MISSING)):
        assert agent.validate_stream_session(dict(descriptor, **{key: value}), capture)
    raw = agent.load_config()["capture"]
    assert "codec_revision" not in agent.stream_session(raw)


def test_drafts_require_opt_in_and_both_backend_identities(agent):
    capture = configured(agent)
    assert not agent.stream_implementation_errors(capture)
    for key, value in (("codec_experimental", False), ("encoder_backend", ""), ("decoder_backend", "")):
        assert agent.stream_implementation_errors(dict(capture, **{key: value}))
    assert agent.stream_implementation_errors(dict(capture, max_bitrate_mbps=2001))


@pytest.mark.parametrize("patch", [{"encoder_hardware": 1}, {"codec_experimental": "true"},
    {"decode_in_flight": 0}, {"decode_in_flight": 9}, {"gop_frames": 0},
    {"max_au_bytes": 16777217}, {"encoder_backend": "name:"}, {"decoder_backend": "name:bad\nvalue"}])
def test_bad_codec_config_never_persists(agent, patch, tmp_path):
    assert agent.save_config({"capture": patch})[0] == 400
    assert not (tmp_path / "vc-config.json").exists()


def test_unsupported_backend_rejected_before_registration_or_stopping(agent, monkeypatch):
    capture = configured(agent)
    assert agent.save_config({"adapter": "Loopback", "capture": capture})[0] == 200
    monkeypatch.setattr(agent, "native_codec_request", lambda *args: {"error": "unsupported explicit MFT"})
    monkeypatch.setattr(agent, "vcam_register", lambda: pytest.fail("must not register"))
    monkeypatch.setattr(agent, "launch_product", lambda *args: pytest.fail("must not stop current slot"))
    code, result = agent.product_local_start()
    assert code == 409 and "unsupported explicit MFT" in result["error"]


def test_read_only_codec_check_is_not_frame_validation(agent, monkeypatch, tmp_path):
    capture = configured(agent)
    report = {"revision": 1, "configured": True, "frames_tested": 0, "stream_validated": False}
    monkeypatch.setattr(agent, "native_codec_request", lambda kind, params: report)
    monkeypatch.setattr(agent, "launch_product", lambda *args: pytest.fail("read-only check"))
    code, result = agent.codec_check_request({"capture": capture})
    assert code == 200 and result is report and not list(tmp_path.iterdir())


def test_codec_arguments_are_explicit_and_not_shell_text(agent):
    capture = configured(agent)
    command = agent.codec_cli(capture, "receiver")
    assert command[command.index("--codec-backend") + 1] == "name:explicit-decoder"
    assert command[command.index("--codec-hardware") + 1] == "0"
    assert command[command.index("--decode-in-flight") + 1] == "2"
    assert command[command.index("--max-au-bytes") + 1] == "8388608"


def test_invalid_peer_codec_check_fails_closed(agent, monkeypatch):
    capture = configured(agent)
    url = "http://192.168.2.59:5143"
    agent.PEER_CODEC_CAPABILITIES[(agent.ROOT, url)] = {"native": {"codec_revision": 1}}
    monkeypatch.setattr(agent, "_stream_discovery", lambda *args: (None, None, [], []))
    monkeypatch.setattr(agent, "native_codec_request", lambda *args: {"configured": True})
    monkeypatch.setattr(agent, "peer_call", lambda *args, **kwargs: (200, ["invalid", "response"]))
    code, result = agent.stream_plan({"capture": capture, "peer_url": url})
    assert code == 200 and not result["can_start"]
    assert any("无效响应" in error for error in result["implementation"]["reasons"])


@pytest.mark.parametrize("kind", ["sender", "receiver"])
def test_native_access_unit_envelope_and_reassembly(kind):
    executable = ROOT / f"build/Release/vc-{kind}.exe"
    if not executable.exists(): pytest.skip("native build required")
    result = subprocess.run([str(executable), "--au-selftest"], capture_output=True, timeout=10)
    assert result.returncode == 0, result.stdout + result.stderr


@pytest.mark.parametrize("kind", ["sender", "receiver"])
@pytest.mark.parametrize("args", [["--codec", "h264"], ["--codec", "av1"], ["--bitrate", "20000000"],
    ["--codec", "hevc", "--codec-backend", MISSING, "--decode-in-flight", "9"],
    ["--codec", "h264", "--codec-backend", MISSING, "--codec-hardware", "2"],
    ["--codec-check", "--codec", "h264", "--codec-backend", MISSING, "--mtu", "99999"],
    ["--codec", "h264", "--codec-backend", MISSING, "--max-au-bytes", "999999999"]])
def test_native_rejects_invalid_codec_options_without_opening_nic(kind, args):
    executable = ROOT / f"build/Release/vc-{kind}.exe"
    if not executable.exists(): pytest.skip("native build required")
    result = subprocess.run([str(executable), *args], capture_output=True, timeout=5)
    assert result.returncode == 2, result.stdout + result.stderr


@pytest.mark.parametrize("kind", ["sender", "receiver"])
def test_missing_native_mft_has_no_codec_or_device_fallback(kind):
    executable = ROOT / f"build/Release/vc-{kind}.exe"
    if not executable.exists(): pytest.skip("native build required")
    result = subprocess.run([str(executable), "--codec-check", "--codec", "h264", "--codec-backend", MISSING],
                            capture_output=True, timeout=10)
    assert result.returncode == 1 and b"no fallback" in result.stderr
