"""run_remote.py - POST /exec to a vc-agent (allowlist-bound remote exec for automation).
Usage:
  python ctrl/run_remote.py --agent http://192.168.2.59:18080 --timeout 30 -- ipconfig /all
  python ctrl/run_remote.py --agent http://127.0.0.1:18081 -- python --version
  python ctrl/run_remote.py --agent http://127.0.0.1:18081 --log --lines 10
Exit code = remote exit code (None/timed-out -> 1, transport error -> 2, denied -> 3).
"""
import argparse, json, sys
import urllib.request, urllib.error

def post(base, argv, timeout):
    data = json.dumps({"argv": argv, "timeout_s": timeout}).encode()
    req = urllib.request.Request(base + "/exec", data=data,
                                 headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout + 15) as r:
            return r.status, json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        try: return e.code, json.loads(e.read().decode())
        except Exception: return e.code, {"denied": str(e)}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--agent", required=True)
    ap.add_argument("--timeout", type=int, default=30)
    ap.add_argument("--log", action="store_true")
    ap.add_argument("--lines", type=int, default=10)
    ap.add_argument("--update", default="")
    ap.add_argument("--rollback", action="store_true")
    ap.add_argument("--force", action="store_true")
    ap.add_argument("argv", nargs="*")
    a = ap.parse_args()
    if a.update:
        import os as _os
        if not a.force:
            try:
                with urllib.request.urlopen(a.agent + "/health", timeout=15) as r:
                    h = json.loads(r.read().decode())
            except Exception:
                print("health check failed, refuse to push")
                return 2
            if h.get("updater") != "ps-ok":
                print("updater not ready, refuse to push")
                return 2
        data = open(a.update, "rb").read()
        req = urllib.request.Request(a.agent + "/update", data=data,
                                     headers={"Content-Type": "application/zip"}, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                print(r.status, r.read().decode()[:500])
        except urllib.error.HTTPError as e:
            print(e.code, e.read().decode()[:500])
            return 2
        print("posted; agent restarts itself, poll /version to confirm")
        return 0
    if a.rollback:
        data = json.dumps({"rollback": True}).encode()
        req = urllib.request.Request(a.agent + "/update", data=data,
                                     headers={"Content-Type": "application/json"}, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                print(r.status, r.read().decode()[:500])
        except urllib.error.HTTPError as e:
            print(e.code, e.read().decode()[:500])
            return 2
        print("posted; agent restarts itself, poll /version to confirm")
        return 0
    if a.log:
        with urllib.request.urlopen(a.agent + "/exec/log?lines=%d" % a.lines, timeout=15) as r:
            for e in json.loads(r.read().decode()).get("entries", []):
                print(json.dumps(e, ensure_ascii=False))
        return 0
    if not a.argv:
        print("need argv after --"); return 2
    code, res = post(a.agent, a.argv, a.timeout)
    if code == 403:
        print("DENIED: " + res.get("denied", "?")); return 3
    if code != 200:
        print("ERR %s: %s" % (code, res)); return 2
    def _emit(s, stream):
        try:
            stream.write(s)
        except UnicodeEncodeError:
            enc = stream.encoding or "utf-8"
            stream.write(s.encode(enc, "replace").decode(enc))
    _emit(res.get("stdout", ""), sys.stdout)
    if res.get("stderr"):
        _emit(res["stderr"], sys.stderr)
    print("[exit=%s timed_out=%s duration=%ss]%s" % (
        res.get("exit_code"), res.get("timed_out"), res.get("duration_s"),
        " TRUNCATED" if res.get("truncated") else ""))
    if res.get("timed_out"): return 1
    ec = res.get("exit_code")
    return ec if isinstance(ec, int) else 1

if __name__ == "__main__":
    sys.exit(main())
