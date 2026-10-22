"""vc-ctrl.py - orchestrator (T-04 ready): version compare -> arm rx -> start tx -> poll -> stop -> verdict+report.
Usage (loopback, no 2nd PC):
  python ctrl/vc-ctrl.py --sender http://127.0.0.1:18081 --receiver http://127.0.0.1:18081 --seconds 10
Real dual-host T1:
  python ctrl/vc-ctrl.py --sender http://192.168.2.100:18080 --receiver http://192.168.2.59:18080 --seconds 60 --pattern motion --case T1
Thresholds per references/test-matrix.md (initial redlines, tighten after 3 rounds).
Report: run/report-<case>-<timestamp>.json with versions/sessions/stats/verdict.
"""
import argparse, json, time, sys, os, datetime
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUN_DIR = os.path.join(ROOT, "run")

def get(base, path, timeout=10):
    with urllib.request.urlopen(base + path, timeout=timeout) as r:
        return json.loads(r.read().decode())

def post(base, path, args=None, timeout=10):
    data = json.dumps(args or {}).encode()
    req = urllib.request.Request(base + path, data=data, headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode())

def verdict_t1(sstats, rstats):
    """T1 linked verdict per test-matrix.md. Returns (pass:bool, reasons:list). Stub data must FAIL, not crash."""
    reasons = []
    try:
        sent = sstats.get("sent_frames", sstats.get("sent", 0)) or 0
        comp = rstats.get("complete", 0) or 0
        dropped = rstats.get("dropped", 0) or 0
        mm = rstats.get("hash_mismatch", 0) or 0
        if dropped != 0: reasons.append("dropped=%s != 0" % dropped)
        if mm != 0: reasons.append("hash_mismatch=%s != 0" % mm)
        # complete==sent (allow receiver file-mode unknown? strict for T1)
        if comp != sent: reasons.append("complete(%s)!=sent(%s)" % (comp, sent))
        p99 = rstats.get("inter_arrival_p99_ms", None)
        if p99 is not None:
            try:
                if float(p99) >= 8.3: reasons.append("inter_arrival_p99 %.3f >= 8.3ms (2 frames)" % float(p99))
            except Exception: pass
        avg = rstats.get("inter_arrival_avg_ms", None)
        if avg is not None:
            try:
                if abs(float(avg) - 4.166) > 0.3: reasons.append("inter_arrival_avg %.3f outside 4.166+-0.3" % float(avg))
            except Exception: pass
        # no frames at all -> fail (stub case)
        if (sent or 0) == 0 and (comp or 0) == 0:
            reasons.append("no frames (stub?)")
    except Exception as e:
        reasons.append("verdict-error: %s" % e)
    return (len(reasons) == 0), reasons

def sender_cadence_ms(hashlog_path):
    try:
        if not hashlog_path or not os.path.isfile(hashlog_path):
            return {"frames": 0, "cadence_ms": None}
        first = None
        last = None
        n = 0
        f = open(hashlog_path, "r", encoding="utf-8")
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                ove = json.loads(line)
            except Exception:
                continue
            q = ove.get("qpc_ns")
            if not isinstance(q, (int, float)):
                continue
            if first is None:
                first = q
            last = q
            n = n + 1
        f.close()
        if n < 2 or first is None or last is None or last <= first:
            return {"frames": n, "cadence_ms": None}
        return {"frames": n, "cadence_ms": (last - first) / (n - 1) / 1e6}
    except Exception:
        return {"frames": 0, "cadence_ms": None}

def run_case(sender, receiver, seconds=10, pattern="motion", fps=240, mtu=9000, pacing_us=0, case="T0", expect_frames=0, adapter="", dst_mac="", mode="pattern"):
    os.makedirs(RUN_DIR, exist_ok=True)
    print("== health ==", flush=True)
    sh = get(sender, "/health"); rh = get(receiver, "/health")
    print("sender:", sh, flush=True); print("receiver:", rh, flush=True)
    sv = get(sender, "/version"); rv = get(receiver, "/version")
    print("versions:", sv, rv, flush=True)
    version_ok = sv.get("version") == rv.get("version")
    if not version_ok:
        print("FAIL: version skew %s vs %s" % (sv.get("version"), rv.get("version")), flush=True)
    # frame-count boxing: sender emits exactly `frames` full frames then exits cleanly
    # (no mid-frame SIGTERM -> no stop-edge partial -> complete==sent exactly).
    frames = expect_frames if (expect_frames and expect_frames > 0) else (seconds * fps if (fps and fps > 0) else 0)
    rx_args = {"ethertype": "0x88B5", "mtu": mtu, "verify_hash": True,
               "timeout_s": seconds + 120, "expect_frames": frames}  # generous: pacing/soak stretch sender duration
    if adapter: rx_args["adapter"] = adapter
    rs = post(receiver, "/receiver/start", rx_args)
    tx_args = {"mode": mode, "pattern": pattern, "fps": fps, "frames": frames,
               "ethertype": "0x88B5", "mtu": mtu, "pacing_us": pacing_us}
    if adapter: tx_args["adapter"] = adapter
    if dst_mac: tx_args["dst_mac"] = dst_mac
    ss = post(sender, "/sender/start", tx_args)
    print("sessions:", ss, rs, flush=True)
    t0 = time.time()
    last_s, last_r = {}, {}
    steady_s, steady_r = None, None
    while time.time() - t0 < seconds:
        time.sleep(2)
        try: last_s = get(sender, "/sender/stats"); print("sender:", last_s, flush=True)
        except Exception as e: print("sender stats err:", e, flush=True)
        try: last_r = get(receiver, "/receiver/stats"); print("receiver:", last_r, flush=True)
        except Exception as e: print("receiver stats err:", e, flush=True)
        try:
            _sn = (last_s.get("sent_frames", 0) or 0)
            if last_s.get("running") and frames and _sn < frames:
                steady_s, steady_r = dict(last_s), dict(last_r)
        except Exception:
            pass

    if frames and frames > 0:
        deadline = time.time() + seconds + 60
        last_sent, last_prog = -1, time.time()
        while time.time() < deadline:
            try:
                _st = get(sender, "/sender/stats")
                if not _st.get("running", True):
                    break
                _sent = _st.get("sent_frames", 0) or 0
                if _sent >= frames:
                    break
                if _sent != last_sent:
                    last_sent, last_prog = _sent, time.time()
                elif time.time() - last_prog > 5:
                    break  # stalled (stub agents never advance) -> fall through to stop
            except Exception:
                pass
            time.sleep(1)
    try: sstop = post(sender, "/sender/stop", {})
    except Exception as e: sstop = {"err": str(e)}
    time.sleep(1.5)  # drain: let in-flight packets reach the receiver before stopping it
    try: rstop = post(receiver, "/receiver/stop", {})
    except Exception as e: rstop = {"err": str(e)}
    print("stop:", sstop, rstop, flush=True)
    try: last_s = get(sender, "/sender/stats")
    except Exception: pass
    try: last_r = get(receiver, "/receiver/stats")
    except Exception: pass
    try:
        _hl = ((ss.get("args", {}) or {}).get("hashlog", "") or "")
        sender_cad = sender_cadence_ms(_hl)
    except Exception:
        sender_cad = {"frames": 0, "cadence_ms": None}
    if steady_r is not None:
        steady_rep = {"s_sent": (steady_s or {}).get("sent_frames"), "s_up": (steady_s or {}).get("uptime_s"), "r_comp": steady_r.get("complete"), "r_avg": steady_r.get("inter_arrival_avg_ms"), "r_p99": steady_r.get("inter_arrival_p99_ms"), "r_up": steady_r.get("uptime_s")}
    else:
        steady_rep = None
    ok, reasons = verdict_t1(last_s, last_r)
    if not version_ok:
        ok = False; reasons = ["version-skew"] + reasons
    ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    report = {"case": case, "time": ts, "sender_base": sender, "receiver_base": receiver,
              "sender_version": sv, "receiver_version": rv, "version_ok": version_ok,
              "sender_session": ss, "receiver_session": rs,
              "sender_stats": last_s, "receiver_stats": last_r,
              "steady_state": steady_rep,
              "sender_cadence": sender_cad,
              "stop": {"sender": sstop, "receiver": rstop},
              "verdict": "PASS" if ok else "FAIL", "reasons": reasons,
              "params": {"seconds": seconds, "pattern": pattern, "fps": fps, "mtu": mtu, "pacing_us": pacing_us, "adapter": adapter, "dst_mac": dst_mac}}
    rp = os.path.join(RUN_DIR, "report-%s-%s.json" % (case, ts))
    with open(rp, "w", encoding="utf-8") as f: json.dump(report, f, indent=1)
    print(("PASS" if ok else "FAIL") + ": " + "; ".join(reasons) + " | report=" + rp, flush=True)
    print("DONE: thresholds per references/test-matrix.md", flush=True)
    return report

if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--sender", required=True); ap.add_argument("--receiver", required=True)
    ap.add_argument("--seconds", type=int, default=10)
    ap.add_argument("--pattern", default="motion"); ap.add_argument("--fps", type=int, default=240)
    ap.add_argument("--mtu", type=int, default=9000); ap.add_argument("--pacing-us", type=int, default=0)
    ap.add_argument("--case", default="T0"); ap.add_argument("--expect-frames", type=int, default=0)
    ap.add_argument("--adapter", default="")
    ap.add_argument("--dst-mac", default="")
    ap.add_argument("--mode", default="pattern")
    a = ap.parse_args()
    # P1-1: --adapter 必填（缺省 exit 2，提示先 GET /adapters，不产生报告）
    if not str(a.adapter or "").strip():
        print("missing --adapter; GET /adapters first (e.g. curl http://127.0.0.1:18080/adapters), then pass --adapter <substr>", flush=True)
        print("no report generated (adapter required per PLAN P1-1)", flush=True)
        sys.exit(2)
    r = run_case(a.sender, a.receiver, a.seconds, a.pattern, a.fps, a.mtu, a.pacing_us, a.case, a.expect_frames, a.adapter, a.dst_mac, a.mode)
    sys.exit(0 if r["verdict"] == "PASS" else 1)

