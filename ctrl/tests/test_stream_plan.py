"""P6-1: configuration drafts, exact L2 budgets and fail-closed startup guards."""
import importlib.util
import io
import json
from pathlib import Path

import pytest


@pytest.fixture
def agent(tmp_path):
    path = Path(__file__).resolve().parents[2] / "agent/vc-agent.py"
    spec = importlib.util.spec_from_file_location("stream_plan_agent", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = str(tmp_path)
    module.state["role"] = "host"
    return module


def nic(speed=2500000000, **extra):
    return {"name": "DirectNIC", "status": "Up", "receive_link_bps": speed,
            "transmit_link_bps": speed, **extra}


def config(agent, **capture):
    return agent._merge(agent.CONFIG_DEFAULT, {"capture": capture})


def plan(agent, **capture):
    return agent.calculate_stream_plan(config(agent, **capture), nic(), nic())


def test_presets_and_old_config_migration(agent, tmp_path):
    (tmp_path / "vc-config.json").write_text(json.dumps({"capture": {"fps": 120, "batch": 23}}))
    loaded = agent.load_config()
    assert loaded["capture"]["width"] == loaded["capture"]["height"] == 640
    assert loaded["capture"]["fps"] == 120 and loaded["capture"]["batch"] == 23
    assert loaded["capture"]["codec"] == "raw" and loaded["capture"]["transform"] == "crop"
    assert loaded["transport"]["headroom_pct"] == 20
    presets = agent.stream_capabilities()["presets"]
    assert [(p["width"], p["height"]) for p in presets] == [
        (416, 416), (640, 640), (1280, 720), (1920, 1080), (2560, 1440)]


@pytest.mark.parametrize("patch", [
    {"width": 1}, {"height": 8194}, {"width": 417}, {"width": "640"}, {"height": True},
    {"width": None}, {"height": 416.0}, {"codec": "av1"}, {"transform": "automatic"},
    {"bitrate_mbps": 0}, {"max_bitrate_mbps": False}, {"bitrate_mbps": 81, "max_bitrate_mbps": 80},
])
def test_invalid_capture_parameters_do_not_persist(agent, patch, tmp_path):
    assert agent.save_config({"capture": patch})[0] == 400
    assert not (tmp_path / "vc-config.json").exists()


@pytest.mark.parametrize("headroom", [-1, 51, True, "20", 1.5])
def test_invalid_headroom(agent, headroom):
    assert agent.save_config({"transport": {"headroom_pct": headroom}})[0] == 400


def test_pending_settings_persist_without_claiming_support(agent):
    draft = {"capture": {"width": 1024, "height": 768, "codec": "hevc", "transform": "fit",
                         "bitrate_mbps": 80, "max_bitrate_mbps": 120}, "transport": {"headroom_pct": 25}}
    assert agent.save_config(draft)[0] == 200
    loaded = agent.load_config()
    assert loaded["capture"]["width"] == 1024 and loaded["capture"]["codec"] == "hevc"
    assert loaded["capture"]["batch"] == 69 and loaded["capture"]["gpu_convert"] is True
    result = agent.calculate_stream_plan(loaded, nic(), nic())
    assert result["network"]["status"] == "fits"
    assert result["network"]["is_estimate"]
    assert not result["implementation"]["supported"] and not result["can_start"]
    assert len(result["implementation"]["reasons"]) == 3
    assert result["implementation"]["hardware_verified"] is False
    assert any("IDR" in message for message in result["warnings"])


def test_exact_640_l2_budget_and_baseline_support(agent):
    result = plan(agent)
    net = result["network"]
    assert net["payload_bytes"] == 8958
    assert net["packets_per_frame"] == 69
    assert net["payload_bps"] == 1179648000
    assert net["wire_bps"] == (614400 + 69 * 66) * 8 * 240
    assert net["usable_bps"] == 2000000000 and net["max_raw_fps"] == 403
    assert net["status"] == "fits" and result["can_start"]
    assert any("尚未按目标帧率硬限速" in message for message in result["warnings"])


@pytest.mark.parametrize("width,height,fps,status", [
    (416, 416, 240, "fits"), (1280, 720, 240, "exceeds"),
    (1920, 1080, 60, "fits"), (1920, 1080, 240, "exceeds"),
    (2560, 1440, 240, "exceeds"),
])
def test_preset_budgets_do_not_confuse_network_with_implementation(agent, width, height, fps, status):
    result = plan(agent, width=width, height=height, fps=fps)
    assert result["network"]["status"] == status
    assert result["video"]["raw_frame_bytes"] == width * height * 3 // 2
    assert not result["implementation"]["supported"] and not result["can_start"]


def test_padding_and_mtu_math(agent):
    assert plan(agent, width=2, height=2, fps=1)["network"]["wire_bps"] == 84 * 8
    smaller = config(agent, mtu=1500)
    smaller["receive"]["mtu"] = 1500
    assert agent.calculate_stream_plan(smaller)["network"]["packets_per_frame"] == 422
    assert not plan(agent, mtu=1500)["can_start"]  # receiver remains mtu9000


def test_selected_directions_and_slowest_link(agent):
    host = nic(receive_link_bps=1000000000, transmit_link_bps=10000000000)
    sender = nic(receive_link_bps=10000000000, transmit_link_bps=2500000000)
    result = agent.calculate_stream_plan(config(agent), host, sender)
    assert result["network"]["link_bps"] == 1000000000
    assert result["network"]["status"] == "exceeds" and not result["can_start"]


def test_headroom_is_advice_not_an_acceptance_threshold(agent):
    c = config(agent)
    c["transport"]["headroom_pct"] = 50
    result = agent.calculate_stream_plan(c, nic(1500000000), nic(1500000000))
    assert result["network"]["status"] == "margin" and result["can_start"]


@pytest.mark.parametrize("bad_nic", [None, {}, nic(0), nic("2500000000"), nic(True),
                                    nic(name=r"\Device\NPF_Loopback"), nic(virtual=True),
                                    nic(hardware_interface=False)])
def test_unknown_speeds_never_assume_2_5g(agent, bad_nic):
    result = agent.calculate_stream_plan(config(agent), nic(), bad_nic)
    assert result["network"]["status"] == "unknown"
    assert result["network"]["link_bps"] is None
    assert result["network"]["max_raw_fps"] is None


def test_one_known_slow_endpoint_is_enough_to_prove_impossible(agent):
    result = agent.calculate_stream_plan(config(agent), nic(1000000000), None)
    assert result["network"]["link_bps"] is None
    assert result["network"]["status"] == "exceeds" and not result["can_start"]


def test_disconnected_adapter_cannot_start(agent):
    result = agent.calculate_stream_plan(config(agent), nic(status="Disconnected"), nic())
    assert result["network"]["status"] == "down" and not result["can_start"]


def test_exact_selection_and_ambiguous_substrings(agent):
    items = [nic(name="NPF-1", description="Intel Controller"), nic(name="NPF-2", description="USB Controller")]
    assert agent._stream_selected_adapter(items, "NPF-2") is items[1]
    assert agent._stream_selected_adapter(items, "intel") is items[0]
    assert agent._stream_selected_adapter(items, "Controller") is None
    assert agent._stream_selected_adapter(items, "") is None


def test_discovery_is_read_only_cached_and_refreshable(agent, monkeypatch, tmp_path):
    calls = []
    monkeypatch.setattr(agent, "get_adapters", lambda: (calls.append("local") or 200, {"items": [nic()]}))
    def peer(path, **kwargs):
        assert path == "/stream/capabilities" and kwargs["url"] == "http://192.168.2.59:5143"
        calls.append("peer")
        return 200, {"revision": 1, "adapter": nic(1000000000)}
    monkeypatch.setattr(agent, "peer_call", peer)
    draft = {"adapter": "DirectNIC", "peer_url": "http://192.168.2.59:5143"}
    assert agent.stream_plan(draft)[1]["network"]["status"] == "exceeds"
    assert agent.stream_plan(draft)[0] == 200 and calls == ["local", "peer"]
    assert agent.stream_plan(draft, refresh=True)[0] == 200
    assert calls == ["local", "peer", "local", "peer"]
    assert not list(tmp_path.iterdir())  # no config, process, SHM or registration writes


def test_old_peer_does_not_invent_link_speed(agent, monkeypatch):
    monkeypatch.setattr(agent, "get_adapters", lambda: (200, {"items": [nic()]}))
    monkeypatch.setattr(agent, "peer_call", lambda *a, **k: (404, {"error": "unknown"}))
    code, result = agent.stream_plan({"adapter": "DirectNIC", "peer_url": "http://192.168.2.59:5143"})
    assert code == 200 and result["network"]["status"] == "unknown"
    assert result["can_start"]  # retain legacy raw startup when speed is unknown
    assert any("新版速率探测" in message for message in result["warnings"])


def test_malformed_peer_capabilities_do_not_claim_feasible(agent, monkeypatch):
    monkeypatch.setattr(agent, "get_adapters", lambda: (200, {"items": [nic()]}))
    monkeypatch.setattr(agent, "peer_call", lambda *a, **k: (200, ["invalid", "capabilities"]))
    code, result = agent.stream_plan({"adapter": "DirectNIC", "peer_url": "http://192.168.2.59:5143"})
    assert code == 200 and result["network"]["status"] == "unknown"


@pytest.mark.parametrize("args", [{"config": []}, {"refresh": 1}, {"config": {"capture": {"width": 417}}}])
def test_bad_plan_requests_fail_before_network_access(agent, monkeypatch, args):
    monkeypatch.setattr(agent, "_stream_discovery", lambda *a, **k: pytest.fail("invalid draft must not probe"))
    assert agent.stream_plan_request(args)[0] == 400


def test_plan_url_validation_and_capture_role(agent, monkeypatch):
    monkeypatch.setattr(agent, "_stream_discovery", lambda *a, **k: pytest.fail("must not probe"))
    for address in ("http://host:5143", "http://999.1.1.1:5143", "http://192.168.2.59:18080"):
        assert agent.stream_plan({"peer_url": address})[0] == 400
    agent.state["role"] = "capture"
    assert agent.stream_plan()[0] == 403


@pytest.mark.parametrize("capture", [{"width": 416, "height": 416}, {"codec": "h264"}, {"transform": "fit"}])
def test_unimplemented_product_start_has_no_side_effects(agent, monkeypatch, capture):
    assert agent.save_config({"capture": capture})[0] == 200
    monkeypatch.setattr(agent, "peer_call", lambda *a, **k: pytest.fail("must reject before remote calls"))
    monkeypatch.setattr(agent, "vcam_register", lambda: pytest.fail("must not register"))
    monkeypatch.setattr(agent, "launch_product", lambda *a, **k: pytest.fail("must not launch"))
    assert agent.product_start()[0] == 409
    assert agent.product_local_start()[0] == 409
    agent.state["role"] = "capture"
    assert agent.product_local_start({"capture": capture})[0] == 409


def test_physical_budget_rejection_precedes_registration(agent, monkeypatch):
    agent.save_config({"adapter": "DirectNIC", "peer_url": "http://192.168.2.59:5143"})
    def peer(path, **kwargs):
        if path == "/health": return 200, {"product_revision": 2, "product_role": "capture"}
        if path == "/config": return 200, {"role": "capture"}
        if path == "/version": return 200, {"version": agent.read_version()}
        return 200, {"revision": 1, "adapter": nic(1000000000)}
    monkeypatch.setattr(agent, "peer_call", peer)
    monkeypatch.setattr(agent, "get_adapters", lambda: (200, {"items": [nic()]}))
    monkeypatch.setattr(agent, "vcam_register", lambda: pytest.fail("must not register on impossible link"))
    assert agent.product_start()[0] == 409


@pytest.mark.parametrize("kind", ["sender", "receiver"])
def test_low_level_does_not_silently_ignore_new_options(agent, monkeypatch, kind):
    class Handler:
        path = "/" + kind + "/start"
        client_address = ("127.0.0.1", 1)
        rfile = io.BytesIO(b'{"adapter":"Loopback","width":416}')
        headers = {"Content-Length": str(len(rfile.getvalue()))}
        def _j(self, body, code=200): self.response = code, body
    monkeypatch.setattr(agent, "launch", lambda *a, **k: pytest.fail("must not launch"))
    handler = Handler()
    agent.H.do_POST(handler)
    assert handler.response[0] == 409


def test_capture_capabilities_use_control_route_without_starting(agent, monkeypatch):
    agent.state["role"] = "capture"
    def choose(ip):
        assert ip == "192.168.2.100"
        return 200, nic()
    monkeypatch.setattr(agent, "_choose_capture_adapter", choose)
    capabilities = agent.stream_capabilities("192.168.2.100")
    assert capabilities["adapter"]["transmit_link_bps"] == 2500000000
    assert capabilities["native"]["codecs"] == ["raw"]
    assert not capabilities["native"]["variable_resolution"]
    assert capabilities["hardware"] == {"encoder": "unprobed", "decoder": "unprobed"}
