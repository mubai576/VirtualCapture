"""主机/采集后台契约与虚拟卡生命周期；硬件操作全部替换为可观察的假实现。"""
import importlib.util
from pathlib import Path

import pytest


@pytest.fixture
def agent(tmp_path, monkeypatch):
    path = Path(__file__).resolve().parents[2] / "agent/vc-agent.py"
    spec = importlib.util.spec_from_file_location("gui_host_agent", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = str(tmp_path)
    module.state["role"] = "host"
    monkeypatch.setattr(module, "stream_native_errors", lambda kind: [])
    executable = tmp_path / "fake.exe"
    executable.touch()
    monkeypatch.setattr(module, "exe_path", lambda kind: str(executable))
    assert module.save_config({"adapter": r"\Device\NPF_{7406EA46-5837-40C4-8598-D3B5063E5684}",
                               "peer_url": "http://192.168.2.59:5143"})[0] == 200
    return module


@pytest.fixture
def lifecycle(agent, monkeypatch):
    events = []
    class Process:
        alive = True
        def poll(self): return None if self.alive else 0
    def register():
        events.append("register")
        return 200, {"match": True}
    def unregister():
        events.append("unregister")
        agent.state["vcam_managed"] = False
        return 200, {"reclaimed": True}
    def launch(kind, args):
        events.append(("launch", kind, args))
        agent.state[kind].update(proc=Process(), session_id="test")
        return "test", {"pid": 123}
    def stop(kind):
        events.append(("stop", kind))
        agent.state[kind]["proc"] = None
    monkeypatch.setattr(agent, "vcam_register", register)
    monkeypatch.setattr(agent, "vcam_unregister", unregister)
    monkeypatch.setattr(agent, "vcam_status", lambda: {"match": agent.state["vcam_managed"]})
    monkeypatch.setattr(agent, "launch", launch)
    monkeypatch.setattr(agent, "_stop_kind", stop)
    monkeypatch.setattr(agent.time, "sleep", lambda seconds: None)
    return events


def test_host_config_is_fixed_and_validates_ip_and_adapter(agent):
    assert agent.DEFAULT_PORT == 5143
    code, result = agent.save_config({"role": "capture", "receive": {"shm": "Local\\Other", "vcam_autoregister": False}})
    assert code == 200 and result["config"]["role"] == "receive"
    assert result["config"]["receive"] == {"shm": "Local\\VirtualCaptureVcamFrame", "mtu": 9000, "vcam_autoregister": True}
    for address in ("http://192.168.2.59:18080", "http://host:5143", "http://999.1.1.1:5143"):
        assert agent.save_config({"peer_url": address})[0] == 400
    assert agent.save_config({"adapter": r"\Device\NPF_{7406EA46-5837-40C4-8598-D3B5063E5684}"})[0] == 200
    assert agent.save_config({"adapter": "bad & calc"})[0] == 400


def test_register_receive_stop_reclaim_order(agent, lifecycle):
    assert agent.product_local_start()[0] == 200
    assert lifecycle[0] == "register" and lifecycle[1][0:2] == ("launch", "receiver")
    assert lifecycle[1][2]["shm"] == "Local\\VirtualCaptureVcamFrame"
    assert agent.product_local_stop()[0] == 200
    assert lifecycle[-2:] == [("stop", "receiver"), "unregister"]


def test_receiver_launch_failure_reclaims_registration(agent, lifecycle, monkeypatch):
    def fail(*args): raise OSError("launch denied")
    monkeypatch.setattr(agent, "launch", fail)
    assert agent.product_local_start()[0] == 500
    assert lifecycle == ["register", "unregister"]
    assert not agent.state["vcam_managed"]


def test_partial_registration_failure_is_reclaimed(agent, lifecycle, monkeypatch):
    monkeypatch.setattr(agent, "vcam_register", lambda: (500, {"match": True, "error": "partial registration"}))
    monkeypatch.setattr(agent, "vcam_status", lambda: {"match": True})
    assert agent.product_local_start()[0] == 500
    assert lifecycle == ["unregister"]
    assert not agent.state["vcam_managed"]


def test_unexpected_receiver_exit_reclaims_without_browser(agent, lifecycle):
    agent.product_local_start()
    agent.state["receiver"]["proc"].alive = False
    agent.reap_vcam()
    assert lifecycle[-1] == "unregister" and not agent.state["vcam_managed"]


def test_capture_failure_rolls_back_and_stop_survives_offline_peer(agent, lifecycle, monkeypatch):
    calls = []
    def peer(path, body=None, **kwargs):
        calls.append((path, body))
        if path == "/health": return 200, {"product_revision": 2, "product_role": "capture", "stream_revision": 2}
        if path == "/config": return 200, {"role": "capture"}
        if path == "/version": return 200, {"version": agent.read_version()}
        if path == "/stream/capabilities": return 200, {"revision": 2, "native": {"variable_resolution": True, "dda_fps_limit": True}}
        return 0, {"error": "peer unreachable"}
    monkeypatch.setattr(agent, "peer_call", peer)
    monkeypatch.setattr(agent, "get_adapters", lambda: (200, {"items": []}))
    code, result = agent.product_start()
    assert code == 502 and not result["ok"]
    start = next(body for path, body in calls if path == "/product/local/start")
    assert start["auto_adapter"] is True
    assert start["capture"]["mode"] == "dda"
    assert lifecycle[-2:] == [("stop", "receiver"), "unregister"]
    agent.product_local_start()
    code, result = agent.product_stop()
    assert code == 200 and not result["ok"]  # 对端不可达，但本机已停止并回收。
    assert lifecycle[-2:] == [("stop", "receiver"), "unregister"]


def test_capture_worker_uses_control_route_without_saved_config(agent, lifecycle, monkeypatch):
    agent.state["role"] = "capture"
    monkeypatch.setattr(agent, "_choose_capture_adapter", lambda ip: (200, {"name": "Realtek"}))
    code, result = agent.product_local_start({"auto_adapter": True, "capture": {"mode": "pattern", "batch": 17}, "stream": agent.stream_session(agent.load_config()["capture"])}, "192.168.2.100")
    assert code == 200
    assert lifecycle[0][0:2] == ("launch", "sender")
    assert lifecycle[0][2]["adapter"] == "Realtek" and lifecycle[0][2]["batch"] == 17
    assert "register" not in lifecycle
    assert agent.product_start()[0] == 403


@pytest.mark.parametrize("echo", [None, {"revision": 1}])
def test_capture_success_without_exact_session_echo_rolls_back(agent, lifecycle, monkeypatch, echo):
    calls = []
    def peer(path, body=None, **kwargs):
        calls.append(path)
        if path == "/health": return 200, {"product_revision": 2, "product_role": "capture", "stream_revision": 2}
        if path == "/config": return 200, {"role": "capture"}
        if path == "/version": return 200, {"version": agent.read_version()}
        if path == "/stream/capabilities": return 200, {"revision": 2, "native": {"variable_resolution": True, "dda_fps_limit": True}}
        return 200, {"stream": echo}
    monkeypatch.setattr(agent, "peer_call", peer)
    monkeypatch.setattr(agent, "get_adapters", lambda: (200, {"items": []}))
    code, result = agent.product_start()
    assert code == 502 and not result["ok"]
    assert calls[-1] == "/product/local/stop"
    assert lifecycle[-2:] == [("stop", "receiver"), "unregister"]
    assert result["steps"][1]["code"] == 409


def test_capture_worker_does_not_serve_ui(agent):
    agent.state["role"] = "capture"
    class Handler:
        path = "/ui"
        response = None
        def _j(self, body, code=200): self.response = code, body
    handler = Handler()
    assert agent._p5_get(handler)
    assert handler.response[0] == 404


def test_capture_adapter_matches_route_instead_of_first_device(agent, monkeypatch):
    class Probe:
        def __enter__(self): return self
        def __exit__(self, *args): pass
        def connect(self, address): assert address == ("192.168.2.100", 5143)
        def getsockname(self): return "192.168.2.59", 12345
    monkeypatch.setattr(agent.socket, "socket", lambda *args: Probe())
    adapters = [{"name": "WrongNIC", "addresses": [{"ip": "192.168.1.2"}]},
                {"name": "DirectNIC", "addresses": [{"ip": "192.168.2.59"}]}]
    monkeypatch.setattr(agent, "get_adapters", lambda: (200, {"items": adapters}))
    assert agent._choose_capture_adapter("192.168.2.100")[1]["name"] == "DirectNIC"
    adapters.append(dict(adapters[1], name="AmbiguousNIC"))
    assert agent._choose_capture_adapter("192.168.2.100")[0] == 409


def test_unregister_checks_actual_result_and_preserves_foreign_registration(agent, monkeypatch):
    agent.state["vcam_managed"] = True
    monkeypatch.setattr(agent, "vcam_status", lambda: {"registered": "other.dll", "match": False, "dll": "ours.dll"})
    monkeypatch.setattr(agent.subprocess, "run", lambda *args, **kwargs: pytest.fail("must not unregister foreign DLL"))
    assert agent.vcam_unregister()[0] == 409 and agent.state["vcam_managed"]
    states = iter([{"registered": "ours.dll", "match": True, "dll": "ours.dll"},
                   {"registered": "", "match": False, "dll": "ours.dll"}])
    monkeypatch.setattr(agent, "vcam_status", lambda: next(states))
    calls = []
    def run(argv, **kwargs):
        calls.append(argv)
        return type("Result", (), {"returncode": 0})()
    monkeypatch.setattr(agent.subprocess, "run", run)
    assert agent.vcam_unregister()[0] == 200
    assert calls[0][-3:] == ["/s", "/u", "ours.dll"]
    assert not agent.state["vcam_managed"]


def test_native_startup_exit_is_failure_with_diagnostics(agent, lifecycle, monkeypatch, tmp_path):
    agent.state["role"] = "capture"
    log = tmp_path / "native.stdout.log"
    log.write_text("adapter missing\n", encoding="utf-8")
    class FailedProcess:
        pid = 123
        def poll(self): return 2
    def launch(kind, args):
        agent.state[kind].update(proc=FailedProcess(), stdout=str(log), session_id="failed")
        return "failed", {"cmd": ["vc-sender.exe"]}
    monkeypatch.setattr(agent, "launch", launch)
    code, result = agent.product_local_start({"stream": agent.stream_session(agent.load_config()["capture"])})
    assert code == 500 and "adapter missing" in result["error"]
    assert result["diagnostics"]["exit_code"] == 2
    assert result["diagnostics"]["stdout"] == "native.stdout.log"
    assert not result["diagnostics"]["running"]


def test_control_loopback_selects_exact_npcap_loopback(agent, monkeypatch):
    class Probe:
        def __enter__(self): return self
        def __exit__(self, *args): pass
        def connect(self, address): pass
        def getsockname(self): return "127.0.0.1", 12345
    monkeypatch.setattr(agent.socket, "socket", lambda *args: Probe())
    monkeypatch.setattr(agent, "get_adapters", lambda: (200, {"items": [
        {"name": "WrongNIC", "addresses": [{"ip": "127.0.0.1"}]},
        {"name": r"\Device\NPF_Loopback"}]}))
    assert agent._choose_capture_adapter("127.0.0.1")[1]["name"] == r"\Device\NPF_Loopback"


def test_process_details_report_actual_capture_source(agent):
    agent.state["sender"].update(mode="pattern", pattern="motion", adapter="Loopback")
    details = agent.slot_stats("sender")
    assert details["mode"] == "pattern" and details["pattern"] == "motion"
    assert details["adapter"] == "Loopback"
