# 拉回PC2全量收端哈希
import json, urllib.request, urllib.parse
R = "http://192.168.2.59:18080"
for tag in ("q1", "q1b", "q3", "q23", "q69", "q3b"):
    sid = json.load(open("run/%s.receiver.stats.json" % tag, encoding="utf-8"))["session_id"]
    url = R + "/files?" + urllib.parse.urlencode({"path": "run/%s.receiver.hash.jsonl" % sid})
    o = json.loads(urllib.request.urlopen(url, timeout=30).read().decode())
    if "content" not in o:
        print(tag, sid, "FETCH-FAILED", o)
        continue
    open("run/%s.receiver.hash.jsonl" % tag, "w", encoding="utf-8").write(o["content"])
    n = len(open("run/%s.receiver.hash.jsonl" % tag, encoding="utf-8").read().splitlines())
    print(tag, sid, "size=", o.get("size"), "truncated=", o.get("truncated"), "lines=", n)
