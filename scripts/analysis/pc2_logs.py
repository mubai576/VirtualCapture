import json, urllib.request, urllib.parse
R = "http://192.168.2.59:18080"
for rel in ("run/updates.log", "run/exec.log", "run/helper.log"):
    url = R + "/files?" + urllib.parse.urlencode({"path": rel})
    try:
        o = json.loads(urllib.request.urlopen(url, timeout=30).read().decode())
        c = o.get("content", "")
        print("=== ", rel, "size=", o.get("size"), "trunc=", o.get("truncated"))
        print(c[-1200:] if len(c) > 1200 else c)
    except Exception as e:
        print(rel, "ERR", e)
