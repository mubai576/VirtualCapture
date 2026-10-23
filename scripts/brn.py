"""双机批探针：备好PC2收端，本地直跑发端；写run/<tag>.*"""
import json, time, urllib.request, subprocess, os, sys
os.chdir(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))  # repo root: exe/run paths are relative
R = "http://192.168.2.59:18080"
print("brn-init")
def post(base, path, args):
    d = json.dumps(args).encode()
    q = urllib.request.Request(base+path, data=d, headers={"Content-Type": "application/json"}, method="POST")
    return json.loads(urllib.request.urlopen(q, timeout=30).read().decode())
def get(base, path):
    return json.loads(urllib.request.urlopen(base+path, timeout=15).read().decode())
print("brn-funcs")
if len(sys.argv) < 4: print("usage: python scripts/brn.py <batch> <frames> <tag>"); sys.exit(2)
batch = sys.argv[1]
frames = sys.argv[2]
tag = sys.argv[3]
rs = post(R, "/receiver/start", {"adapter": "Controller", "ethertype": "0x88B5", "mtu": 9000, "timeout_s": 120, "expect_frames": int(frames)})
print("rx-armed", rs.get("session_id"))
subprocess.run(["build/vc-sender.exe", "--adapter", "Controller", "--mode", "pattern", "--pattern", "motion", "--fps", "240", "--frames", frames, "--ethertype", "0x88B5", "--mtu", "9000", "--pacing-us", "0", "--dst-mac", "00:E2:69:A9:F0:0A", "--batch", batch, "--session", tag, "--stats", "run/" + tag + ".sender.stats.json", "--hashlog", "run/" + tag + ".sender.hash.jsonl"], check=False)
time.sleep(3)
try:
    post(R, "/receiver/stop", {})
except Exception as e:
    print("rx-stop-warn", e)
try:
    rstats = get(R, "/receiver/stats")
    with open("run/" + tag + ".receiver.stats.json", "w", encoding="utf-8") as f:
        json.dump(rstats, f, indent=1)
    print("rx-saved", rstats.get("session_id"))
except Exception as e:
    print("rx-save-warn", e)
    rstats = {}
try:
    sstats = json.load(open("run/" + tag + ".sender.stats.json", encoding="utf-8"))
    fps = sstats.get("sent_frames", 0) / max(sstats.get("uptime_s", 0.001), 0.001)
    print("SUMMARY batch=%s sent=%s complete=%s dropped=%s lost=%s mm=%s send_err=%s fps=%.1f pps=%.0f" % (
        batch, sstats.get("sent_frames"), rstats.get("complete"), rstats.get("dropped"),
        rstats.get("packets_lost"), rstats.get("hash_mismatch"), sstats.get("send_errors"),
        fps, sstats.get("pps", 0)))
except Exception as e:
    print("summary-warn", e)
print("BATCHRUN-DONE", batch)
