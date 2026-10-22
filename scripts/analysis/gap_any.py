import json, sys
for p in sys.argv[1:]:
    qs = []
    for line in open(p, encoding="utf-8"):
        line = line.strip()
        if line:
            q = json.loads(line).get("qpc_ns")
            if isinstance(q, (int, float)):
                qs.append(q)
    gaps = sorted((qs[i+1]-qs[i])/1e6 for i in range(len(qs)-1))
    n = len(gaps)
    print("%s n=%d max_gap=%.1fms p99=%.2fms p999=%.2fms median=%.3fms over20ms=%d" % (
        p, len(qs), gaps[-1], gaps[int(n*0.99)], gaps[int(n*0.999)],
        gaps[n//2], sum(1 for g in gaps if g > 20)))
