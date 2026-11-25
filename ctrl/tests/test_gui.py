"""P5 GUI（D16）：配置校验、/ui、产品模式双 agent 编排（无二进制 -> agent 桩进程，无需网卡）"""
import json, os, socket, subprocess, sys, tempfile, time, urllib.request, urllib.error
import pytest

AGENT = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "agent", "vc-agent.py")


def _free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def _req(base, path, body=None, timeout=60):
    data = None if body is None else json.dumps(body).encode()
    r = urllib.request.Request(base + path, data=data, method="GET" if body is None else "POST",
                               headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            raw = resp.read()
            ct = resp.headers.get("Content-Type", "")
            return resp.status, (json.loads(raw) if "json" in ct else raw.decode("utf-8"))
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def _spawn(root):
    port = _free_port()
    p = subprocess.Popen([sys.executable, AGENT, "--role", "x", "--port", str(port), "--root", root],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = "http://127.0.0.1:%d" % port
    for _ in range(100):
        try:
            if _req(base, "/health", timeout=1)[0] == 200: return p, base
        except Exception: time.sleep(0.1)
    p.kill(); raise RuntimeError("agent did not start")


@pytest.fixture
def pair():
    tmp = [tempfile.mkdtemp(prefix="vcgui-a-"), tempfile.mkdtemp(prefix="vcgui-b-")]
    procs = [_spawn(t) for t in tmp]
    yield procs
    for p, b in procs:
        try: _req(b, "/product/local/stop", {}, timeout=10)
        except Exception: pass
        p.kill(); p.wait(timeout=10)


def test_config_default_validate_and_persist(pair):
    (_, a), _ = pair
    c, cfg = _req(a, "/config")
    assert c == 200 and cfg["role"] == "" and cfg["capture"]["fps"] == 240
    bad = [{"role": "boss"}, {"peer_url": "http://x:1/;rm"}, {"adapter": "I226 & calc"},
           {"capture": {"dst_mac": "zz"}}, {"capture": {"fps": "240"}}, {"receive": {"shm": "Local\\a b"}}]
    for b in bad:
        code, r = _req(a, "/config", b)
        assert code == 400 and r["details"], b
    code, r = _req(a, "/config", {"role": "receive", "adapter": "I226", "unknown_key": 1,
                                  "receive": {"vcam_autoregister": False}})
    assert code == 200 and "unknown_key" not in r["config"]
    assert _req(a, "/config")[1]["receive"]["vcam_autoregister"] is False


def test_ui_served(pair):
    (_, a), _ = pair
    c, html = _req(a, "/ui")
    assert c == 200 and "VirtualCapture 控制台" in html and "/product/status" in html
    for field in ("c_capture_mode", "c_output", "c_wgc_target", "c_wgc_window", "bWindows"):
        assert 'id="' + field + '"' in html
    assert '<select id="c_wgc_window"' in html
    for removed in ("c_bitrate", "c_gop", "c_encoder_hw", "c_decoder_hw"):
        assert removed not in html


def test_wgc_source_config_persists_without_starting(pair):
    (_, a), _ = pair
    capture = {"mode": "wgc", "output": 2, "wgc_window": "Example Game"}
    code, result = _req(a, "/config", {"capture": capture})
    assert code == 200
    saved = _req(a, "/config")[1]["capture"]
    assert all(saved[key] == value for key, value in capture.items())
    assert not _req(a, "/sender/stats")[1]["running"]
    for invalid in ({"output": 16}, {"output": -1}, {"output": 1.5},
                    {"wgc_window": "x" * 129}, {"wgc_window": "bad\nname"}):
        assert _req(a, "/config", {"capture": invalid})[0] == 400
    assert _req(a, "/config", {"capture": {"mode": "dda", "wgc_window": ""}})[0] == 200
    assert _req(a, "/config")[1]["capture"]["output"] == 2


def test_product_start_requires_roles(pair):
    (_, a), _ = pair
    c, r = _req(a, "/product/start", {})
    assert c == 409 and not r["ok"]


def test_product_start_stop_two_agents(pair):
    (_, a), (_, b) = pair
    assert _req(a, "/config", {"role": "capture", "adapter": "Realtek", "peer_url": b,
                               "capture": {"mode": "pattern", "dst_mac": "34:5a:60:4f:f2:33"}})[0] == 200
    assert _req(b, "/config", {"role": "receive", "adapter": "I226", "peer_url": a,
                               "receive": {"vcam_autoregister": False}})[0] == 200
    # 发起方是接收机（任一台都可发起）
    c, r = _req(b, "/product/start", {})
    assert c == 200 and r["ok"], r
    assert [s["side"] for s in r["steps"]] == ["receive", "capture"]
    st = _req(b, "/product/status")[1]
    assert st["peer"]["online"] and st["local"]["receiver"]["running"] and st["peer"]["sender"]["running"]
    assert isinstance(st["local"]["vcam"], dict)          # 接收机才报 vcam 状态
    assert st["peer"]["config"]["role"] == "capture"
    # 停播：发端先停
    c, r = _req(a, "/product/stop", {})
    assert c == 200 and r["ok"] and [s["side"] for s in r["steps"]] == ["capture", "receive"]
    assert not _req(a, "/sender/stats")[1]["running"] and not _req(b, "/receiver/stats")[1]["running"]


def test_product_start_rejects_wrong_peer_role(pair):
    (_, a), (_, b) = pair
    _req(a, "/config", {"role": "capture", "adapter": "Realtek", "peer_url": b})
    _req(b, "/config", {"role": "capture", "adapter": "I226", "peer_url": a})
    c, r = _req(a, "/product/start", {})
    assert c == 409 and "peer role" in r["error"]
    assert not _req(a, "/sender/stats")[1].get("running") and not _req(b, "/receiver/stats")[1].get("running")


def test_stream_plan_http_is_read_only_and_pending_settings_cannot_launch(pair):
    (_, a), _ = pair
    assert _req(a, "/health")[1]["stream_revision"] == 2
    code, capabilities = _req(a, "/stream/capabilities")
    assert code == 200 and capabilities["native"]["codecs"] == []
    assert capabilities["hardware"]["encoder"] == "unprobed"
    code, result = _req(a, "/stream/plan", {"config": {"capture": {"width": 416, "height": 416}}})
    assert code == 200 and result["network"]["status"] == "unknown"
    assert not result["can_start"]
    assert _req(a, "/config")[1]["capture"]["width"] == 640
    assert _req(a, "/stream/plan", {"config": {"capture": {"width": 417}}})[0] == 400
    assert _req(a, "/config", {"role": "receive", "adapter": "Loopback",
                               "capture": {"width": 416, "height": 416}})[0] == 200
    assert _req(a, "/product/local/start", {"stream": {}})[0] == 409
    assert _req(a, "/product/start", {})[0] == 409
    for kind in ("sender", "receiver"):
        assert _req(a, "/" + kind + "/start", {"adapter": "Loopback", "width": 416})[0] == 409
        assert not _req(a, "/" + kind + "/stats")[1]["running"]
