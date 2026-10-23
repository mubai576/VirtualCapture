"""远控执行：POST /exec到agent（白名单限内）
查日志/跑脚本/推OTA包/回滚；退出码=远端码，超时1传输错2拒绝3"""
import argparse, json, sys
import urllib.request, urllib.error

def post(base, argv, timeout):
    # 发/exec远控命令
    data = json.dumps({"argv": argv, "timeout_s": timeout}).encode()
    req = urllib.request.Request(base + "/exec", data=data,
                                 headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout + 15) as r:
            return r.status, json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        try: return e.code, json.loads(e.read().decode())
        except Exception: return e.code, {"denied": str(e)}


def poll_version(base, want, timeout=180):
    # 轮询/version等换代
    import time as _t
    _dead = _t.time() + timeout
    while _t.time() < _dead:
        try:
            with urllib.request.urlopen(base + '/version', timeout=10) as r:
                _v = json.loads(r.read().decode()).get('version', '')
            print('version=%s' % _v)
            if _v and _v == want:
                print('confirmed at %s' % want)
                return 0
        except Exception as e:
            print('poll: %s' % e)
        _t.sleep(3)
    print('not confirmed within %ss (want %s)' % (timeout, want))
    return 1

def main():
    # 入口：解析参数并转发
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
        import zipfile as _zf
        if not a.force:
            try:
                with urllib.request.urlopen(a.agent + '/health', timeout=15) as r:
                    h = json.loads(r.read().decode())
            except Exception:
                print('health check failed, refuse to push')
                return 2
            if h.get('updater') != 'ps-ok':
                print('updater not ready, refuse to push')
                return 2
        try:
            with _zf.ZipFile(a.update) as _z:
                _exp = json.loads(_z.read('version.json').decode()).get('version', '')
        except Exception as _e:
            print('bad version zip: %s' % _e)
            return 2
        if not _exp:
            print('version.json missing version, refuse to push')
            return 2
        data = open(a.update, 'rb').read()
        req = urllib.request.Request(a.agent + '/update', data=data,
                                     headers={'Content-Type': 'application/zip'}, method='POST')
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                _st = r.status
                _sb = r.read().decode()
                print(_st, _sb[:500])
        except urllib.error.HTTPError as e:
            print(e.code, e.read().decode()[:500])
            return 2
        except Exception as e:
            print('stage POST failed: %s' % e)
            return 2
        if 'staged' not in _sb:
            print('stage rejected, aborting before activate')
            return 2
        _ab = json.dumps({'version': _exp}).encode()
        _aq = urllib.request.Request(a.agent + '/activate', data=_ab,
                                     headers={'Content-Type': 'application/json'}, method='POST')
        try:
            with urllib.request.urlopen(_aq, timeout=120) as r:
                print(r.status, r.read().decode()[:500])
        except urllib.error.HTTPError as e:
            print(e.code, e.read().decode()[:500])
            return 2
        except Exception as e:
            print('activate sent, no response (execv handover); polling: %s' % e)
        return poll_version(a.agent, _exp)
    if a.rollback:
        data = json.dumps({'rollback': True}).encode()
        req = urllib.request.Request(a.agent + '/activate', data=data,
                                     headers={'Content-Type': 'application/json'}, method='POST')
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                _rb = r.read().decode()
                print(r.status, _rb[:500])
        except urllib.error.HTTPError as e:
            print(e.code, e.read().decode()[:500])
            return 2
        except Exception as e:
            print('activate sent, no response (execv handover); polling: %s' % e)
            _rb = '{}'
        try:
            _want = json.loads(_rb).get('version', '')
        except Exception:
            _want = ''
        if _want:
            return poll_version(a.agent, _want)
        import time as _t2
        _dead = _t2.time() + 180
        while _t2.time() < _dead:
            try:
                with urllib.request.urlopen(a.agent + '/version', timeout=10) as r:
                    _v = json.loads(r.read().decode()).get('version', '')
                print('version=%s' % _v)
                if _v:
                    print('agent reachable at %s' % _v)
                    return 0
            except Exception as e:
                print('poll: %s' % e)
            _t2.sleep(3)
        print('rollback target not reachable within 180s')
        return 1
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
