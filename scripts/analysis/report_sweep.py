# 扫报告目录汇总判分
import json, glob, os
for p in sorted(glob.glob("run/report-*.json")):
    try:
        r = json.load(open(p, encoding="utf-8"))
        s, x = r.get("sender_stats", {}), r.get("receiver_stats", {})
        print("%s | %s | %s | sent=%s comp=%s drop=%s mm=%s" % (
            os.path.basename(p), r.get("case"), r.get("verdict"),
            s.get("sent_frames"), x.get("complete"), x.get("dropped"), x.get("hash_mismatch")))
    except Exception as e:
        print(os.path.basename(p), "UNREADABLE", e)
