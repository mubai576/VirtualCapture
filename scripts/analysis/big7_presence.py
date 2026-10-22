import json, glob, os, urllib.request, urllib.parse
R = "http://192.168.2.59:18080"
want = ["checker-20261021-194156", "checker-rerun-20261021-194723", "counter-20261021-194047",
        "motion-20261021-193650", "noise-20261021-195100", "rings-20261021-194946", "soak-uni-20261021-202819"]
for p in sorted(glob.glob("run/report-*.json")):
    if not any(w in p for w in want):
        continue
    r = json.load(open(p, encoding="utf-8"))
    sh = ((r.get("sender_session") or {}).get("args") or {}).get("hashlog", "")
    rh = ((r.get("receiver_session") or {}).get("args") or {}).get("hashlog", "")
    s_ok = os.path.exists(sh) if sh.startswith("D:\\") else ("nonlocal:" + sh)
    r_sid = rh.split("/")[-1].replace(".receiver.hash.jsonl", "") if rh else ""
    r_info = ""
    if r_sid:
        url = R + "/files?" + urllib.parse.urlencode({"path": "run/%s.receiver.hash.jsonl" % r_sid})
        try:
            o = json.loads(urllib.request.urlopen(url, timeout=30).read().decode())
            r_info = "size=%s trunc=%s" % (o.get("size"), o.get("truncated"))
        except Exception as e:
            r_info = "ERR %s" % e
    print(os.path.basename(p), "| sender_local=", s_ok, "| rx_pc2=", r_info)
