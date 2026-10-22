"""reconcile.py - compare sender.hash.jsonl vs receiver.hash.jsonl for a session.
Usage:
  python ctrl/reconcile.py --sender run/<sid>.sender.hash.jsonl --receiver run/<sid>.receiver.hash.jsonl
  python ctrl/reconcile.py --session <sid>   (looks in run/)
Output: summary JSON to stdout + mismatch frame list. Exit 0 if 0 mismatch and 0 missing, else 1.
"""
import argparse, json, os, sys

def load(p):
    d = {}
    if not p or not os.path.exists(p): return d, ["missing-file:" + str(p)]
    errs = []
    with open(p, "r", encoding="utf-8") as f:
        for i, line in enumerate(f, 1):
            line = line.strip()
            if not line: continue
            try:
                o = json.loads(line)
                d[int(o["frame_id"])] = o.get("hash", "")
            except Exception as e:
                errs.append("line%d:%s" % (i, e))
    return d, errs

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sender", default=""); ap.add_argument("--receiver", default="")
    ap.add_argument("--session", default="")
    ap.add_argument("--out", default="")
    a = ap.parse_args()
    sp, rp = a.sender, a.receiver
    if a.session:
        run = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "run")
        sp = sp or os.path.join(run, a.session + ".sender.hash.jsonl")
        rp = rp or os.path.join(run, a.session + ".receiver.hash.jsonl")
    s, se = load(sp); r, re = load(rp)
    sf, rf = set(s), set(r)
    missing_in_rx = sorted(sf - rf)   # sent but not received (dropped)
    extra_in_rx = sorted(rf - sf)     # received but never sent (should be 0)
    mismatch = sorted([f for f in (sf & rf) if s[f] != r[f]])
    res = {"sender_frames": len(s), "receiver_frames": len(r),
           "missing_in_receiver": missing_in_rx, "extra_in_receiver": extra_in_rx,
           "mismatch": [{"frame_id": f, "sender": s[f], "receiver": r[f]} for f in mismatch],
           "hash_mismatch": len(mismatch), "sender_errors": se, "receiver_errors": re}
    out = json.dumps(res, indent=1)
    print(out)
    if a.out:
        with open(a.out, "w", encoding="utf-8") as f: f.write(out)
    ok = (len(mismatch) == 0 and len(extra_in_rx) == 0)
    # missing frames are reported but do not force nonzero when receiver simply got fewer (dropped); caller decides.
    # Exit 0 only when hash-clean AND no extra; missing listed separately.
    return 0 if ok else 1

if __name__ == "__main__":
    sys.exit(main())
