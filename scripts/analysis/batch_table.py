# 扫表：批量跑不同batch看pps
import json, os
def load(p):
    with open(p, encoding="utf-8") as f: return json.load(f)
def cadence(p):
    first = last = None; n = 0
    with open(p, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line: continue
            try: q = json.loads(line).get("qpc_ns")
            except Exception: continue
            if not isinstance(q, (int, float)): continue
            if first is None: first = q
            last = q; n += 1
    if n < 2 or last <= first: return n, None
    return n, (last - first) / (n - 1) / 1e6
print("| batch | fps_tx | pps_tx | bps_G | send_err | rx_complete | dropped | lost | mm | cadence_ms | rx_avg | rx_p99 |")
print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
for b in ("1", "1b", "3", "3b", "23", "69"):
    s = load("run/q%s.sender.stats.json" % b)
    r = load("run/q%s.receiver.stats.json" % b)
    n, cad = cadence("run/q%s.sender.hash.jsonl" % b)
    fps = s["sent_frames"] / s["uptime_s"]
    print("| %s | %.1f | %.0f | %.2f | %d | %d/%d | %d | %d | %d | %s | %.3f | %.3f |" % (
        b, fps, s["pps"], s["bps"] / 1e9, s["send_errors"],
        r["complete"], s["sent_frames"], r["dropped"], r["packets_lost"], r["hash_mismatch"],
        ("%.3f" % cad) if cad else "None", r["inter_arrival_avg_ms"], r["inter_arrival_p99_ms"]))
