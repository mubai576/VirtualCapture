import json
rows = [
    ("T0", "report-T0-20261021-193245.json", "FAIL", 2400, 2400, 13440),
    ("T1-checker", "report-T1-checker-20261021-194156.json", "FAIL", 1713, 1713, 1702),
    ("T1-checker-rerun", "report-T1-checker-rerun-20261021-194723.json", "FAIL", 14400, 14400, 13549),
    ("T1-counter", "report-T1-counter-20261021-194047.json", "FAIL", 14400, 14400, 13722),
    ("T1-motion", "report-T1-motion-20261021-193650.json", "FAIL", 14400, 14400, 13350),
    ("T1-noise", "report-T1-noise-20261021-195100.json", "FAIL", 14400, 14400, 13915),
    ("T1-rings", "report-T1-rings-20261021-194946.json", "FAIL", 14400, 14400, 13216),
    ("T2-soak-uni", "report-T2-soak-uni-20261021-202819.json", "FAIL", 144000, 144000, 13925),
    ("T3-mtu1500", "report-T3-mtu1500-20261021-200337.json", "FAIL", 2400, 2400, 28262),
    ("T4-p0", "report-T4-p0-20261021-195951.json", "FAIL", 3600, 3600, 12964),
    ("T4-p200", "report-T4-p200-20261021-200224.json", "FAIL", 3600, 3600, 3567),
    ("T4-p50", "report-T4-p50-20261021-200024.json", "FAIL", 3600, 3600, 7829),
]
bad = 0
for case, fn, v, sent, comp, pps in rows:
    try:
        r = json.load(open("run/" + fn, encoding="utf-8"))
        s, x = r["sender_stats"], r["receiver_stats"]
        ok = (r["verdict"] == v and s["sent_frames"] == sent and x["complete"] == comp
              and abs(round(x.get("pps", s.get("pps", 0))) - pps) <= 1)
        # 表pps：先收端，缺则发端
        if not ok:
            ok = (r["verdict"] == v and s["sent_frames"] == sent and x["complete"] == comp
                  and abs(round(s.get("pps", 0)) - pps) <= 1)
        print(("OK  " if ok else "DIFF"), case, fn)
        if not ok:
            bad += 1
            print("   file:", r["verdict"], s["sent_frames"], x["complete"], x.get("pps"), s.get("pps"))
    except Exception as e:
        bad += 1
        print("ERR ", case, fn, e)
print("MISMATCHES=", bad)
