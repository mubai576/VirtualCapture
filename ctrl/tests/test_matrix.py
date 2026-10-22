"""test_matrix.py — T-04 pytest matrix T0→T1→T2→T4→T5 (T3 opt-in --run-t3).
Uses vc-ctrl.run_case against stub agents: stub data must FAIL cleanly (no crash).
Thresholds frozen per references/test-matrix.md (judge at lower bounds, never relax).
"""
import json, os, subprocess, sys, time, socket
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "ctrl"))
from fnv1a import fnv1a64, fnv1a64_hex
import pattern_ref
import importlib.util
_spec = importlib.util.spec_from_file_location('vc_ctrl', os.path.join(ROOT, 'ctrl', 'vc-ctrl.py'))
vc_ctrl = importlib.util.module_from_spec(_spec); _spec.loader.exec_module(vc_ctrl)
verdict_t1, run_case = vc_ctrl.verdict_t1, vc_ctrl.run_case

def _free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p

def _wait_health(base, timeout=15):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            with urllib.request.urlopen(base + "/health", timeout=3) as r:
                json.loads(r.read().decode()); return True
        except Exception:
            time.sleep(0.5)
    return False

def test_fnv_vectors():
    assert fnv1a64(b"") == 14695981039346656037
    assert fnv1a64(b"a") == 0xAF63DC4C8601EC8C
    assert fnv1a64_hex(b"hello") != fnv1a64_hex(b"helln")

def test_pattern_determinism_and_tiers():
    for p, g in pattern_ref.GENS.items():
        Y1, UV1 = g(0); Y2, UV2 = g(0)
        assert Y1 == Y2 and UV1 == UV2, p
    n0 = pattern_ref.gen_noise_y(0); n1 = pattern_ref.gen_noise_y(1)
    c0 = pattern_ref.gen_counter_y(0); c1 = pattern_ref.gen_counter_y(1)
    nd = sum(abs(a-b) for a, b in zip(n0, n1))/len(n0)
    cd = sum(abs(a-b) for a, b in zip(c0, c1))/len(c0)
    assert nd > 50 and cd < 5

def test_reconcile_unit(tmp_path):
    import reconcile
    sp = tmp_path / "s.hash.jsonl"; rp = tmp_path / "r.hash.jsonl"
    sp.write_text("{\"frame_id\": 1, \"hash\": \"aaaaaaaaaaaaaaaa\"}\n{\"frame_id\": 2, \"hash\": \"bbbbbbbbbbbbbbbb\"}\n", encoding="utf-8")
    rp.write_text("{\"frame_id\": 1, \"hash\": \"aaaaaaaaaaaaaaaa\"}\n{\"frame_id\": 2, \"hash\": \"ffffffffffffffff\"}\n", encoding="utf-8")
    s, _ = reconcile.load(str(sp)); r, _ = reconcile.load(str(rp))
    mm = sorted([f for f in (set(s) & set(r)) if s[f] != r[f]])
    assert mm == [2], "mismatch detect"
    # missing-frame case
    rp.write_text("{\"frame_id\": 1, \"hash\": \"aaaaaaaaaaaaaaaa\"}\n", encoding="utf-8")
    s, _ = reconcile.load(str(sp)); r, _ = reconcile.load(str(rp))
    assert sorted(set(s) - set(r)) == [2], "missing detect"

def test_interlock_cpp_vectors():
    exe = os.path.join(ROOT, "build", "vc-sender.exe")
    if not os.path.exists(exe):
        import pytest; pytest.skip("build/vc-sender.exe missing (pre-build)")
    out = subprocess.run([exe, "--selftest"], capture_output=True, text=True, timeout=120)
    assert out.returncode == 0 and "PASS" in (out.stdout or "")
    vec = {}
    for line in (out.stdout or "").splitlines():
        p = line.strip().split()
        if len(p) == 5 and p[0] == "VEC" and p[1] == "small":
            vec[(p[2], int(p[3]))] = p[4]
    assert len(vec) >= 20, "expect 5x4 small vectors"
    for pat in pattern_ref.PATS:
        for fid in (0, 1, 2, 3):
            Y, UV = pattern_ref.GENS[pat](fid)
            assert vec[(pat, fid)] == pattern_ref.frame_hash_yuv(Y, UV), pat

def test_t1_threshold_logic():
    ok, _ = verdict_t1({"sent_frames": 100}, {"complete": 100, "dropped": 0, "hash_mismatch": 0, "inter_arrival_avg_ms": 4.166})
    assert ok, "synthetic green must PASS"
    ok, rs = verdict_t1({"sent_frames": 100}, {"complete": 99, "dropped": 1, "hash_mismatch": 0, "inter_arrival_avg_ms": 4.166})
    assert not ok and any("dropped" in r for r in rs)
    ok, rs = verdict_t1({"sent_frames": 100}, {"complete": 100, "dropped": 0, "hash_mismatch": 2, "inter_arrival_avg_ms": 4.166})
    assert not ok and any("hash_mismatch" in r for r in rs)
    ok, _ = verdict_t1({"sent_frames": 100}, {"complete": 100, "dropped": 0, "hash_mismatch": 0,
                                             "inter_arrival_avg_ms": 4.166, "inter_arrival_p99_ms": 4.8})
    assert ok, "p99<8.3 must PASS"
    ok, rs = verdict_t1({"sent_frames": 100}, {"complete": 100, "dropped": 0, "hash_mismatch": 0,
                                             "inter_arrival_avg_ms": 4.166, "inter_arrival_p99_ms": 9.1})
    assert not ok and any("p99" in r for r in rs)

def _post_start(base, path, body):
    import urllib.error as _e
    data = json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data,
                                 headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, json.loads(r.read().decode())
    except _e.HTTPError as e:
        try: return e.code, json.loads(e.read().decode())
        except Exception: return e.code, {}

def _run_stub_case(seconds=2, adapter="NoSuchNic-xyz-bad"):
    # P1-1: stub path now requires explicit bad adapter; missing adapter is 400, bad exe is exit 2.
    ps = _free_port(); pr = _free_port()
    agent = os.path.join(ROOT, "agent", "vc-agent.py")
    sproc = subprocess.Popen([sys.executable, agent, "--role", "sender", "--port", str(ps)],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    rproc = subprocess.Popen([sys.executable, agent, "--role", "receiver", "--port", str(pr)],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        assert _wait_health("http://127.0.0.1:%d" % ps), "sender agent up"
        assert _wait_health("http://127.0.0.1:%d" % pr), "receiver agent up"
        # missing adapter -> 400, no process launched
        c1, _ = _post_start("http://127.0.0.1:%d" % ps, "/sender/start", {})
        c2, _ = _post_start("http://127.0.0.1:%d" % pr, "/receiver/start", {})
        assert c1 == 400 and c2 == 400, (c1, c2)
        return {"verdict": "FAIL", "time": "stub-400", "codes": (c1, c2)}
    finally:
        for p in (sproc, rproc):
            try: p.terminate()
            except Exception: pass
        for p in (sproc, rproc):
            try: p.wait(timeout=5)
            except Exception:
                try: p.kill()
                except Exception: pass

def test_T0_stub_structure_fails_cleanly():
    """P1-1: missing adapter -> 400; bad adapter exe -> exit 2; vc-ctrl missing adapter -> exit 2 no report."""
    rep = _run_stub_case(seconds=2)
    assert rep["verdict"] == "FAIL" and rep["codes"] == (400, 400)
    # exe bad adapter must exit 2 with card list
    exe = os.path.join(ROOT, "build", "vc-sender.exe")
    if os.path.exists(exe):
        out = subprocess.run([exe, "--adapter", "NoSuchNic-xyz-bad", "--frames", "5",
                              "--session", "t0-bad", "--stats", "run/t0-bad.stats.json",
                              "--hashlog", "run/t0-bad.hash.jsonl"],
                             capture_output=True, text=True, timeout=30)
        assert out.returncode == 2, (out.returncode, out.stdout, out.stderr)
        assert "not found" in (out.stderr or "") or "adapter" in (out.stderr or "").lower()
    # vc-ctrl without --adapter must exit 2 and create no report
    import glob as _g
    before = set(_g.glob(os.path.join(ROOT, "run", "report-*.json")))
    out = subprocess.run([sys.executable, os.path.join(ROOT, "ctrl", "vc-ctrl.py"),
                          "--sender", "http://127.0.0.1:1", "--receiver", "http://127.0.0.1:1",
                          "--seconds", "1"],
                         capture_output=True, text=True, timeout=30)
    assert out.returncode == 2, (out.returncode, out.stdout, out.stderr)
    assert "GET /adapters" in (out.stdout or "") + (out.stderr or "")
    after = set(_g.glob(os.path.join(ROOT, "run", "report-*.json")))
    assert before == after, "no report on missing adapter"

def _live_case(case, seconds=2):
    import pytest as _pt
    ad = os.environ.get("VC_ADAPTER", "").strip()
    if not ad:
        _pt.skip("VC_ADAPTER not set (GET /adapters first)")
    ps = _free_port(); pr = _free_port()
    agent = os.path.join(ROOT, "agent", "vc-agent.py")
    sproc = subprocess.Popen([sys.executable, agent, "--role", "sender", "--port", str(ps)],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    rproc = subprocess.Popen([sys.executable, agent, "--role", "receiver", "--port", str(pr)],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        assert _wait_health("http://127.0.0.1:%d" % ps), "sender agent up"
        assert _wait_health("http://127.0.0.1:%d" % pr), "receiver agent up"
        rep = run_case("http://127.0.0.1:%d" % ps, "http://127.0.0.1:%d" % pr,
                       seconds=seconds, pattern="motion", fps=240, mtu=9000, pacing_us=0,
        # loopback uses batch 1; batching wedges Npcap loopback capture
                       case=case, expect_frames=480, adapter=ad, batch=1)
        return rep
    finally:
        for p in (sproc, rproc):
            try: p.terminate()
            except Exception: pass
        for p in (sproc, rproc):
            try: p.wait(timeout=5)
            except Exception:
                try: p.kill()
                except Exception: pass

def test_T0_live_loopback():
    """P1-1 live: VC_ADAPTER set -> motion 240Hz frames=480, zero drop/mismatch, p99<8.3."""
    import pytest as _pt
    ad = os.environ.get("VC_ADAPTER", "").strip()
    if not ad:
        _pt.skip("VC_ADAPTER not set")
    rep = _live_case("T0", seconds=5)
    rs = rep.get("receiver_stats", {}) or {}
    ss = rep.get("sender_stats", {}) or {}
    assert rs.get("dropped", 1) == 0, rs
    assert rs.get("hash_mismatch", 1) == 0, rs
    assert ss.get("send_errors", 1) == 0, ss
    p99 = rs.get("inter_arrival_p99_ms")
    if p99 is not None:
        assert float(p99) < 8.3, rs
    st=rep.get("steady_state") or {}; p99=(st.get("r_p99") if st else rs.get("inter_arrival_p99_ms")); assert p99 is not None and float(p99) < 8.3, (st, rs)

def test_T1_live_loopback():
    """P1-1 live T1 shape on loopback (same asserts as T0; dual-host covered in P2)."""
    import pytest as _pt
    ad = os.environ.get("VC_ADAPTER", "").strip()
    if not ad:
        _pt.skip("VC_ADAPTER not set")
    rep = _live_case("T1", seconds=5)
    rs = rep.get("receiver_stats", {}) or {}
    ss = rep.get("sender_stats", {}) or {}
    assert rs.get("dropped", 1) == 0, rs
    assert rs.get("hash_mismatch", 1) == 0, rs
    assert ss.get("send_errors", 1) == 0, ss
    st=rep.get("steady_state") or {}; p99=(st.get("r_p99") if st else rs.get("inter_arrival_p99_ms")); assert p99 is not None and float(p99) < 8.3, (st, rs)

def test_T3_mtu1500_counterexample(request):
    if not request.config.getoption("--run-t3"):
        import pytest; pytest.skip("T3 default skip (use --run-t3)")
    # doc expectation: mtu1500 -> ~422 packets/frame, ~101k pps, must drop. Unit-level: frag formula only.
    payload = 1500 - 42; total = (614400 + payload - 1) // payload
    assert total == 422
    assert total * 240 > 90000, "pps ~101k"
import json as _json
import urllib.request as _url
import urllib.error as _urlerr

def _start_one_agent():
    import subprocess as _sp
    port = _free_port()
    agent = os.path.join(ROOT, "agent", "vc-agent.py")
    proc = _sp.Popen([sys.executable, agent, "--role", "sender", "--port", str(port)],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    assert _wait_health("http://127.0.0.1:%d" % port), "agent up"
    return proc, "http://127.0.0.1:%d" % port

def _stop_proc(proc):
    try: proc.terminate()
    except Exception: pass
    try: proc.wait(timeout=5)
    except Exception:
        try: proc.kill()
        except Exception: pass

def _exec(base, argv, timeout=15):
    data = _json.dumps({"argv": argv, "timeout_s": timeout}).encode()
    req = _url.Request(base + "/exec", data=data,
                       headers={"Content-Type": "application/json"}, method="POST")
    try:
        with _url.urlopen(req, timeout=timeout + 15) as r:
            return r.status, _json.loads(r.read().decode())
    except _urlerr.HTTPError as e:
        try: return e.code, _json.loads(e.read().decode())
        except Exception: return e.code, {}

def test_exec_allows_python_version():
    proc, base = _start_one_agent()
    try:
        code, res = _exec(base, ["python", "--version"])
        assert code == 200, res
        assert res["exit_code"] == 0 and "Python" in res["stdout"], res
    finally:
        _stop_proc(proc)

def test_exec_denies_traversal_shell_inline():
    proc, base = _start_one_agent()
    try:
        for bad in (["../evil"], ["..\\evil"], ["C:/Windows/System32/whoami.exe"],
                    ["powershell", "-Command", "Get-Process"],
                    ["pwsh", "-NoProfile", "-Command", "x"],
                    ["python", "-c", "import os"],
                    ["no-such-tool-xyz"]):
            code, res = _exec(base, bad)
            assert code in (403, 500), (bad, code, res)
            if code == 403:
                assert "denied" in res or "not found" in str(res), (bad, res)
    finally:
        _stop_proc(proc)

def test_exec_repo_script_and_timeout(tmp_path):
    _ = tmp_path
    probe = os.path.join(ROOT, "run", "__probe_hello.py")
    with open(probe, "w", encoding="utf-8") as f:
        f.write("print(\'hello-exec\')\n")
    sleeper = os.path.join(ROOT, "run", "__probe_sleep.py")
    with open(sleeper, "w", encoding="utf-8") as f:
        f.write("import time\ntime.sleep(30)\nprint(\'never\')\n")
    proc, base = _start_one_agent()
    try:
        code, res = _exec(base, ["python", probe])
        assert code == 200 and res["exit_code"] == 0, res
        assert "hello-exec" in res["stdout"], res
        code, res = _exec(base, ["python", sleeper], timeout=2)
        assert code == 200 and res["timed_out"] is True, res
        with _url.urlopen(base + "/exec/log?lines=5", timeout=10) as r:
            entries = _json.loads(r.read().decode())["entries"]
        assert len(entries) >= 2, entries
    finally:
        _stop_proc(proc)
        for p in (probe, sleeper):
            try: os.remove(p)
            except Exception: pass
import io as _io
import shutil as _sh
import tempfile as _tf
import zipfile as _zf

def _mkzip(path, files):
    with _zf.ZipFile(path, "w") as z:
        for name, data in files.items():
            z.writestr(name, data)

def test_update_roundtrip_and_rollback():
    import pytest as _pt
    if os.name != "nt":
        _pt.skip("restart helper is Windows .bat")
    troot = _tf.mkdtemp(prefix="vc-upd-")
    with open(os.path.join(troot, "version.json"), "w", encoding="utf-8") as f:
        f.write(json.dumps({"version": "0.2.0"}))
    port = _free_port()
    agent = os.path.join(ROOT, "agent", "vc-agent.py")
    proc = subprocess.Popen([sys.executable, agent, "--role", "sender", "--port", str(port),
                             "--root", troot],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = "http://127.0.0.1:%d" % port
    dead = []
    try:
        assert _wait_health(base, timeout=15), "agent up"
        zp = os.path.join(troot, "up.zip")
        _mkzip(zp, {"version.json": json.dumps({"version": "0.9.9-test"}),
                    "scripts/ok.ps1": "Write-Host ok\n"})
        with open(zp, "rb") as f:
            data = f.read()
        req = urllib.request.Request(base + "/update", data=data,
                                     headers={"Content-Type": "application/zip"}, method="POST")
        with urllib.request.urlopen(req, timeout=20) as r:
            res = json.loads(r.read().decode())
        assert res.get("updated") is True and res.get("version") == "0.9.9-test", res
        # agent exits itself; helper restarts it with same root/port
        ver = ""
        for _ in range(30):
            time.sleep(2)
            try:
                with urllib.request.urlopen(base + "/version", timeout=5) as r:
                    ver = json.loads(r.read().decode()).get("version", "")
                if ver == "0.9.9-test":
                    break
            except Exception:
                pass
        assert ver == "0.9.9-test", "restarted agent serves new version, got %r" % ver
        assert json.loads(open(os.path.join(troot, "version.json"), encoding="utf-8").read())["version"] == "0.9.9-test"
        assert json.loads(open(os.path.join(troot, "prev", "mirror", "version.json"), encoding="utf-8").read())["version"] == "0.2.0"
        assert os.path.isfile(os.path.join(troot, "scripts", "ok.ps1"))
        # rollback
        req = urllib.request.Request(base + "/update",
                                     data=json.dumps({"rollback": True}).encode(),
                                     headers={"Content-Type": "application/json"}, method="POST")
        with urllib.request.urlopen(req, timeout=20) as r:
            res = json.loads(r.read().decode())
        assert res.get("rollback") is True, res
        ver = ""
        for _ in range(30):
            time.sleep(2)
            try:
                with urllib.request.urlopen(base + "/version", timeout=5) as r:
                    ver = json.loads(r.read().decode()).get("version", "")
                if ver == "0.2.0":
                    break
            except Exception:
                pass
        assert ver == "0.2.0", "rollback restored, got %r" % ver
    finally:
        try:
            c = None
            import subprocess as _sp2
            out = _sp2.run(["powershell", "-NoProfile", "-Command",
                            "(Get-NetTCPConnection -LocalPort %d -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1).OwningProcess" % port],
                           capture_output=True, text=True, timeout=15)
            pid = (out.stdout or "").strip()
            if pid.isdigit():
                _sp2.run(["taskkill", "/PID", pid, "/F"], capture_output=True, timeout=15)
        except Exception:
            pass
        try: proc.terminate()
        except Exception: pass
        time.sleep(1)
        _sh.rmtree(troot, ignore_errors=True)
import importlib.util as _ilu2
import os as _os
_spec2 = _ilu2.spec_from_file_location("bringup", _os.path.join(ROOT, "ctrl", "bringup.py"))
bringup = _ilu2.module_from_spec(_spec2); _spec2.loader.exec_module(bringup)

def test_bringup_selftest():
    assert bringup.main.__name__ == "main"
    import sys as _sys
    _argv = _sys.argv
    _sys.argv = ["bringup.py", "--selftest"]
    try:
        assert bringup.main() == 0
    finally:
        _sys.argv = _argv

def test_bringup_triage_buckets():
    rep = {"receiver_stats": {"dropped": 2, "complete": 5, "hash_mismatch": 0,
                              "inter_arrival_avg_ms": 4.2, "inter_arrival_p99_ms": 5.0, "pps": 1},
           "sender_stats": {"sent_frames": 100}, "version_ok": True}
    assert any("NIC class" in t for t in bringup.triage(rep))
    rep["receiver_stats"]["hash_mismatch"] = 1
    assert any("payload_crc" in t for t in bringup.triage(rep))
    rep["receiver_stats"] = {"dropped": 0, "complete": 10, "hash_mismatch": 0,
                             "inter_arrival_avg_ms": 4.2, "inter_arrival_p99_ms": 5.0, "pps": 1}
    rep["sender_stats"] = {"sent_frames": 100}
    assert any("pps halved" in t or "sender generate" in t for t in bringup.triage(rep))
def _wait_version(base, want, tries=30):
    for _ in range(tries):
        time.sleep(2)
        try:
            with urllib.request.urlopen(base + "/version", timeout=5) as r:
                if json.loads(r.read().decode()).get("version", "") == want:
                    return True
        except Exception:
            pass
    return False

def test_update_same_version_keeps_prev():
    import pytest as _pt
    if os.name != "nt":
        _pt.skip("restart helper is Windows .vbs")
    troot = _tf.mkdtemp(prefix="vc-updsame-")
    with open(os.path.join(troot, "version.json"), "w", encoding="utf-8") as f:
        f.write(json.dumps({"version": "0.3.1"}))
    port = _free_port()
    agent = os.path.join(ROOT, "agent", "vc-agent.py")
    proc = subprocess.Popen([sys.executable, agent, "--role", "sender", "--port", str(port),
                             "--root", troot],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = "http://127.0.0.1:%d" % port
    try:
        assert _wait_health(base, timeout=15), "agent up"
        zp = os.path.join(troot, "up.zip")
        _mkzip(zp, {"version.json": json.dumps({"version": "0.9.9A"})})
        with open(zp, "rb") as f:
            data = f.read()
        for _ in range(2):
            req = urllib.request.Request(base + "/update", data=data,
                                         headers={"Content-Type": "application/zip"}, method="POST")
            with urllib.request.urlopen(req, timeout=20) as r:
                res = json.loads(r.read().decode())
            assert res.get("updated") is True, res
            assert _wait_version(base, "0.9.9A"), "reached 0.9.9A"
        pv = os.path.join(troot, "prev", "mirror", "version.json")
        assert os.path.isfile(pv), "prev backup exists"
        assert json.loads(open(pv, encoding="utf-8").read())["version"] == "0.3.1", \
            "same-version re-apply must not clobber prev"
    finally:
        try:
            out = subprocess.run(["powershell", "-NoProfile", "-Command",
                                  "(Get-NetTCPConnection -LocalPort %d -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1).OwningProcess" % port],
                                 capture_output=True, text=True, timeout=15)
            pid = (out.stdout or "").strip()
            if pid.isdigit():
                subprocess.run(["taskkill", "/PID", pid, "/F"], capture_output=True, timeout=15)
        except Exception:
            pass
        try: proc.terminate()
        except Exception: pass
        time.sleep(1)
        _sh.rmtree(troot, ignore_errors=True)
def test_sender_cadence_ms(tmp_path):
    p = tmp_path / "s.hash.jsonl"
    p.write_text("{\"frame_id\": 0, \"qpc_ns\": 1000000000}\n{\"frame_id\": 1, \"qpc_ns\": 1005000000}\n{\"frame_id\": 2, \"qpc_ns\": 1010000000}\n", encoding="utf-8")
    r = vc_ctrl.sender_cadence_ms(str(p))
    assert r["frames"] == 3 and abs(r["cadence_ms"] - 5.0) < 1e-6
    assert vc_ctrl.sender_cadence_ms(str(tmp_path / "nope.jsonl")) == {"frames": 0, "cadence_ms": None}
    e = tmp_path / "empty.jsonl"; e.write_text("", encoding="utf-8")
    assert vc_ctrl.sender_cadence_ms(str(e))["cadence_ms"] is None
def test_verdict_d1_final_avg_ignored():
    ok, _ = verdict_t1({"sent_frames": 14400}, {"complete": 14400, "dropped": 0, "hash_mismatch": 0, "inter_arrival_avg_ms": 13.5})
    assert ok is True
def test_verdict_d1_steady_and_cadence():
    base_tx = {"sent_frames": 480}
    base_rx = {"complete": 480, "dropped": 0, "hash_mismatch": 0, "inter_arrival_p99_ms": 5.3}
    ok, _ = verdict_t1(base_tx, base_rx, {"r_avg": 4.17}, {"cadence_ms": 4.166})
    assert ok is True
    ok, rs = verdict_t1(base_tx, base_rx, {"r_avg": 5.0}, {"cadence_ms": 4.166})
    assert ok is False and any("steady_avg" in r for r in rs)
    ok, rs = verdict_t1(base_tx, base_rx, {"r_avg": 4.17}, {"cadence_ms": 5.0})
    assert ok is False and any("sender_cadence" in r for r in rs)
    assert verdict_t1(base_tx, base_rx, {"r_avg": 4.17}, {"cadence_ms": None})[0] is True
def test_stub_report_has_steady_keys():
    # P1-1: missing adapter gives 400 without report; steady keys covered by live runs when VC_ADAPTER set.
    rep = _run_stub_case(seconds=2)
    assert rep["verdict"] == "FAIL" and rep.get("codes") == (400, 400)
def test_update_no_restart_keeps_pid():
    import pytest as _pt
    if os.name != "nt":
        _pt.skip("update paths are Windows-only")
    troot = _tf.mkdtemp(prefix="vc-updnorestart-")
    with open(os.path.join(troot, "version.json"), "w", encoding="utf-8") as f:
        f.write(json.dumps({"version": "0.1.0"}))
    port = _free_port()
    agent = os.path.join(ROOT, "agent", "vc-agent.py")
    proc = subprocess.Popen([sys.executable, agent, "--role", "sender", "--port", str(port), "--root", troot], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = "http://127.0.0.1:%d" % port
    try:
        assert _wait_health(base, timeout=15), "agent up"
        zp = os.path.join(troot, "up.zip")
        zf = _zf.ZipFile(zp, "w", _zf.ZIP_DEFLATED)
        zf.writestr("version.json", json.dumps({"version": "0.9.9-nr"}))
        zf.writestr("scripts/ok-nr.ps1", "Write-Host ok-nr\n")
        zf.close()
        with open(zp, "rb") as f:
            data = f.read()
        req = urllib.request.Request(base + "/update", data=data, headers={"Content-Type": "application/zip"}, method="POST")
        with urllib.request.urlopen(req, timeout=20) as r:
            res = json.loads(r.read().decode())
        assert res.get("updated") is True and res.get("restart") == "none", res
        assert proc.poll() is None, "agent must not exit"
        assert _wait_version(base, "0.9.9-nr"), "serves new version without restart"
        assert os.path.isfile(os.path.join(troot, "scripts", "ok-nr.ps1"))
    finally:
        try: proc.terminate()
        except Exception: pass
        try: proc.wait(timeout=5)
        except Exception:
            try: proc.kill()
            except Exception: pass
        _sh.rmtree(troot, ignore_errors=True)


