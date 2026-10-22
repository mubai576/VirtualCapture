import json, glob, os
files = sorted(glob.glob("run/report-T*-20261021-*.json") ) + sorted(glob.glob("run/report-T*-20261022-*.json"))
seen = set()
for p in files:
    try:
        r = json.load(open(p, encoding="utf-8"))
    except Exception:
        continue
    for side in ("sender_session", "receiver_session"):
        h = ((r.get(side) or {}).get("args") or {}).get("hashlog", "")
        if h and h not in seen:
            seen.add(h)
print("distinct session hashlog paths cited:", len(seen))
arch = os.path.join("run", "archive.d"); missing = [h for h in seen if h.startswith("D:\\Project") and not os.path.exists(h) and not os.path.exists(os.path.join(arch, os.path.basename(h)))]
print("missing local files:", missing if missing else "NONE")
