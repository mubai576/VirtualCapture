"""bringup.py - T-06 dual-host bringup runner: T0 -> T1(x5 patterns) -> T2 soak -> T4 pacing -> T3.
Writes docs/bringup-<date>.md with verdict tables + auto-triage per references/test-matrix.md.
Usage:
  python ctrl/bringup.py --sender http://192.168.2.100:18080 --receiver http://192.168.2.59:18080 --adapter I226
  python ctrl/bringup.py --sender ... --receiver ... --adapter X --quick        # smoke: T0 + short T1-motion
  python ctrl/bringup.py --sender ... --receiver ... --adapter X --skip-soak
  python ctrl/bringup.py --selftest    # offline: render sample doc to temp, no network
Exit 0 iff all gating cases PASS (T3 is informational only).
"""
import argparse, datetime, json, os, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import importlib.util as _ilu
_spec = _ilu.spec_from_file_location("vc_ctrl", os.path.join(ROOT, "ctrl", "vc-ctrl.py"))
vc_ctrl = _ilu.module_from_spec(_spec); _spec.loader.exec_module(vc_ctrl)
import reconcile as _rec

CASES_FULL = [
    ("T0", dict(seconds=10, pattern="motion", fps=240, mtu=9000, pacing_us=0)),
    ("T1-counter", dict(seconds=60, pattern="counter", fps=240, mtu=9000, pacing_us=0)),
    ("T1-checker", dict(seconds=60, pattern="checker", fps=240, mtu=9000, pacing_us=0)),
    ("T1-rings", dict(seconds=60, pattern="rings", fps=240, mtu=9000, pacing_us=0)),
    ("T1-noise", dict(seconds=60, pattern="noise", fps=240, mtu=9000, pacing_us=0)),
    ("T1-motion", dict(seconds=60, pattern="motion", fps=240, mtu=9000, pacing_us=0)),
    ("T2-soak", dict(seconds=600, pattern="motion", fps=240, mtu=9000, pacing_us=0)),
    ("T4-p0", dict(seconds=15, pattern="motion", fps=240, mtu=9000, pacing_us=0)),
    ("T4-p50", dict(seconds=15, pattern="motion", fps=240, mtu=9000, pacing_us=50)),
    ("T4-p200", dict(seconds=15, pattern="motion", fps=240, mtu=9000, pacing_us=200)),
    ("T3-mtu1500", dict(seconds=10, pattern="motion", fps=240, mtu=1500, pacing_us=0)),
]
CASES_QUICK = [
    ("T0", dict(seconds=10, pattern="motion", fps=240, mtu=9000, pacing_us=0)),
    ("T1-motion", dict(seconds=15, pattern="motion", fps=240, mtu=9000, pacing_us=0)),
]

def triage(rep):
    """Map a FAIL report to one of the four test-matrix buckets + next step."""
    rs = rep.get("receiver_stats", {}) or {}
    ss = rep.get("sender_stats", {}) or {}
    out = []
    if (rs.get("dropped", 0) or 0) > 0:
        out.append("dropped>0 -> NIC class: check MTU equal both ends? EEE/interrupt-moderation off? "
                   "run T4 to separate burst-loss vs sustained-loss; check 2.5G negotiation (1.18Gbps > 1G link).")
    if (rs.get("hash_mismatch", 0) or 0) > 0:
        out.append("hash_mismatch>0 -> CRC-error means transport damage; CRC-clean but frame-hash wrong means "
                   "sender-generate/receiver-reassemble bug (see packet header payload_crc).")
    sent = ss.get("sent_frames", 0) or 0
    comp = rs.get("complete", 0) or 0
    if sent and comp and comp < sent / 2:
        out.append("pps halved -> compare sent vs recv: sent short = sender generate bottleneck; "
                   "sent ok recv short = capture loss.")
    if not rep.get("version_ok", True):
        out.append("version skew -> compare /version on both agents before any matrix; refuse on mismatch.")
    if not out:
        out.append("no bucket matched (jitter-only?) -> inspect inter_arrival_p99 + host contention (Defender/E-cores).")
    return out

def local_reconcile(rep):
    """Full hashlog compare when both files are local (loopback); else note for manual transfer."""
    try:
        ssid = (rep.get("sender_session") or {}).get("session_id", "")
        rsid = (rep.get("receiver_session") or {}).get("session_id", "")
        sp = os.path.join(ROOT, "run", ssid + ".sender.hash.jsonl") if ssid else ""
        rp = os.path.join(ROOT, "run", rsid + ".receiver.hash.jsonl") if rsid else ""
        if sp and rp and os.path.exists(sp) and os.path.exists(rp):
            s, _ = _rec.load(sp); r, _ = _rec.load(rp)
            mm = sorted(f for f in (set(s) & set(r)) if s[f] != r[f])
            return {"mode": "local-full", "sender_frames": len(s), "receiver_frames": len(r),
                    "mismatch": len(mm), "missing": sorted(set(s) - set(r))[:10]}
    except Exception as e:
        return {"mode": "error", "error": str(e)}
    return {"mode": "remote-files-unavailable",
            "note": "hashlogs live on the agents; transfer run/*.hash.jsonl off both hosts for offline reconcile.py"}

def stats_row(rep):
    ss = rep.get("sender_stats", {}) or {}
    rs = rep.get("receiver_stats", {}) or {}
    rec = rep.get("reconcile", {}) or {}
    return {"case": rep.get("case"), "verdict": rep.get("verdict"),
            "sent": ss.get("sent_frames"), "complete": rs.get("complete"),
            "dropped": rs.get("dropped"), "mm": rs.get("hash_mismatch"),
            "avg": rs.get("inter_arrival_avg_ms"), "p99": rs.get("inter_arrival_p99_ms"),
            "pps": round(rs.get("pps", 0) or 0), "reconcile": rec, "triage": triage(rep)}

def render(path, meta, rows):
    L = []
    L.append("# Bringup %s" % meta["date"])
    L.append("")
    L.append("- sender=%s receiver=%s adapter=%s" % (meta["sender"], meta["receiver"], meta["adapter"]))
    L.append("- sender_version=%s receiver_version=%s version_ok=%s" % (
        meta["sv"], meta["rv"], meta["version_ok"]))
    L.append("")
    L.append("| case | verdict | sent | complete | dropped | mm | avg_ms | p99_ms | pps | reconcile |")
    L.append("|---|---|---|---|---|---|---|---|---|---|")
    for r in rows:
        rec = r["reconcile"]
        rec_s = "%s:%s" % (rec.get("mode"), rec.get("mismatch", rec.get("note", "")))
        L.append("| %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |" % (
            r["case"], r["verdict"], r["sent"], r["complete"], r["dropped"], r["mm"],
            r["avg"], r["p99"], r["pps"], rec_s))
    L.append("")
    L.append("## Triage")
    for r in rows:
        if r["verdict"] != "PASS":
            L.append("- %s: %s" % (r["case"], " / ".join(r["triage"])))
    if all(r["verdict"] == "PASS" or r["case"].startswith("T3") for r in rows):
        L.append("- all gating cases green.")
    L.append("")
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(L) + "\n")
    return path

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sender", default=""); ap.add_argument("--receiver", default="")
    ap.add_argument("--adapter", default="")
    ap.add_argument("--dst-mac", default="")
    ap.add_argument("--batch", type=int, default=0)
    ap.add_argument("--quick", action="store_true"); ap.add_argument("--skip-soak", action="store_true")
    ap.add_argument("--dry-run", action="store_true"); ap.add_argument("--selftest", action="store_true")
    ap.add_argument("--out", default="")
    a = ap.parse_args()
    if a.selftest:
        import tempfile
        fake_pass = {"case": "T0", "verdict": "PASS",
                     "sender_stats": {"sent_frames": 2400}, "receiver_stats": {
                         "complete": 2400, "dropped": 0, "hash_mismatch": 0,
                         "inter_arrival_avg_ms": 4.166, "inter_arrival_p99_ms": 4.8, "pps": 16560},
                     "sender_session": {}, "receiver_session": {}, "version_ok": True}
        fake_fail = {"case": "T1-motion", "verdict": "FAIL", "reasons": ["dropped=2 != 0"],
                     "sender_stats": {"sent_frames": 100}, "receiver_stats": {
                         "complete": 98, "dropped": 2, "hash_mismatch": 0,
                         "inter_arrival_avg_ms": 4.2, "inter_arrival_p99_ms": 5.0, "pps": 16500},
                     "sender_session": {}, "receiver_session": {}, "version_ok": False}
        rows = []
        for rep in (fake_pass, fake_fail):
            rep["reconcile"] = local_reconcile(rep)
            rows.append(stats_row(rep))
        assert rows[0]["verdict"] == "PASS" and "NIC class" in rows[1]["triage"][0]
        assert "version skew" in rows[1]["triage"][-1]
        d = tempfile.mkdtemp(prefix="bringup-st-")
        p = render(os.path.join(d, "bringup-st.md"),
                   {"date": "selftest", "sender": "s", "receiver": "r", "adapter": "a",
                    "sv": "0.3.1", "rv": "0.3.1", "version_ok": True}, rows)
        txt = open(p, encoding="utf-8").read()
        assert "T0" in txt and "T1-motion" in txt and "Triage" in txt
        print("PASS: bringup selftest (%s)" % p)
        return 0
    cases = list(CASES_QUICK if a.quick else CASES_FULL)
    if a.skip_soak:
        cases = [c for c in cases if not c[0].startswith("T2")]
    if a.dry_run or not (a.sender and a.receiver):
        for name, kw in cases:
            print("%-12s %s" % (name, kw))
        if not (a.sender and a.receiver):
            print("(need --sender/--receiver/--adapter to run)"); return 2
        return 0
    rows, sv, rv, vok = [], {}, {}, True
    for name, kw in cases:
        rep = vc_ctrl.run_case(a.sender, a.receiver, adapter=a.adapter, dst_mac=a.dst_mac, batch=a.batch, case=name, **kw)
        sv, rv = rep.get("sender_version", {}), rep.get("receiver_version", {})
        vok = vok and rep.get("version_ok", True)
        rep["reconcile"] = local_reconcile(rep)
        rows.append(stats_row(rep))
        print("== %s: %s ==" % (name, rep["verdict"]), flush=True)
    date = datetime.datetime.now().strftime("%Y%m%d")
    out = a.out or os.path.join(ROOT, "docs", "bringup-%s.md" % date)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    render(out, {"date": date, "sender": a.sender, "receiver": a.receiver, "adapter": a.adapter,
                 "sv": (sv or {}).get("version"), "rv": (rv or {}).get("version"), "version_ok": vok}, rows)
    print("wrote " + out)
    bad = [r for r in rows if r["verdict"] != "PASS" and not r["case"].startswith("T3")]
    return 1 if bad else 0

if __name__ == "__main__":
    sys.exit(main())
