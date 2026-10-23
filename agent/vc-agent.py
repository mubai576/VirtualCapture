"""双机控制agent（T-03，接口冻结）
单文件零依赖，双机同一份代码。
启停查：组CLI→记pid→幂等启停；stats/hashlog落run/。
升级：验zip→staging→快照prev→execv切换，不起孤进程。"""
import argparse, time, uuid, json, os, sys, signal, subprocess, shutil, zipfile, threading

VERSION_DEFAULT = "dev"
import sys as _sys
if getattr(_sys, "frozen", False):
    ROOT = os.path.dirname(os.path.abspath(_sys.executable))  # onefile exe dir (e.g. C:\vc); --root overrides
else:
    ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))  # project root (agent/..)
RUN_DIR = os.path.join(ROOT, "run")
BUILD_DIR = os.path.join(ROOT, "build")
VERSION_FILE = os.path.join(ROOT, "version.json")

def updater_status():
    """重启前检查：ps可用/缺失/未查"""
    if os.name != "nt":
        return "unchecked"
    import shutil as _sh
    ps = _sh.which("powershell.exe") or _sh.which("powershell") or _sh.which("pwsh")
    if not ps:
        return "ps-missing"
    return "ps-ok"
    try:
        import winreg as _wr
        for hive, path in ((_wr.HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows Script Host\Settings"),
                          (_wr.HKEY_CURRENT_USER, r"SOFTWARE\Microsoft\Windows Script Host\Settings")):
            try:
                with _wr.OpenKey(hive, path) as k:
                    v, _ = _wr.QueryValueEx(k, "Enabled")
                    if int(v) == 0:
                        return "vbs-disabled"
            except FileNotFoundError:
                pass
    except Exception:
        pass
    return "vbs-ok"

def read_version():
    # 读版本号（version.json缺省dev）
    try:
        with open(VERSION_FILE, "r", encoding="utf-8") as f:
            return json.load(f).get("version", VERSION_DEFAULT)
    except Exception:
        return VERSION_DEFAULT

state = {"role": None, "port": 18080,
         "sender": {"session_id": None, "proc": None, "started_at": 0, "stats": "", "hashlog": ""},
         "receiver": {"session_id": None, "proc": None, "started_at": 0, "stats": "", "hashlog": ""}}

def exe_path(kind):
    # 发收两用：找build下exe，兜底根目录
    name = "vc-sender.exe" if kind == "sender" else "vc-receiver.exe"
    p = os.path.join(BUILD_DIR, name)
    if os.path.exists(p): return p
    # 兼容多配置目录
    for cand in [os.path.join(BUILD_DIR, "Release", name), os.path.join(BUILD_DIR, "Debug", name),
                 os.path.join(ROOT, name)]:
        if os.path.exists(cand): return cand
    return p  # return expected path so error message is clear

def kill_slot(slot):
    # 停槽位进程（优雅2s再杀）
    p = slot.get("proc")
    if p is None: return
    try:
        if p.poll() is None:
            try:
                if os.name == "nt":
                    # 优雅停：发CTRL_BREAK让exe刷完统计
                    os.kill(p.pid, signal.CTRL_BREAK_EVENT)
                else:
                    p.terminate()
            except Exception:
                try: p.terminate()
                except Exception: pass
            try: p.wait(timeout=2)
            except Exception:
                try: p.terminate()
                except Exception: pass
                try: p.wait(timeout=2)
                except Exception:
                    try: p.kill()
                    except Exception: pass
    except Exception: pass
    slot["proc"] = None

def launch(kind, args):
    # 起一端：先停旧，再拉新会话
    slot = state[kind]
    kill_slot(slot)  # idempotent: stop old first
    sid = "s-%s" % uuid.uuid4().hex[:8]
    os.makedirs(RUN_DIR, exist_ok=True)
    stats = os.path.join(RUN_DIR, "%s.%s.stats.json" % (sid, kind))
    hashlog = os.path.join(RUN_DIR, "%s.%s.hash.jsonl" % (sid, kind))
    exe = exe_path(kind)
    if not os.path.exists(exe):
        # 无二进制时跑桩进程保流程
        # 写桩统计/哈希供对账
        # 桩模拟C++落盘：统计覆写+哈希追加
        slot.update(session_id=sid, started_at=time.time(), stats=stats, hashlog=hashlog)
        stub_stats = {"session_id": sid, "running": True, "note": "stub-no-binary",
                      "sent_frames": 0, "complete": 0, "version": read_version()}
        with open(stats, "w", encoding="utf-8") as f: json.dump(stub_stats, f)
        open(hashlog, "w", encoding="utf-8").close()
        # 占位睡进程，stop前保持运行态
        skw = {"cwd": ROOT, "stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL}
        if os.name == "nt":
            skw["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        slot["proc"] = subprocess.Popen([sys.executable, "-c", "import time;time.sleep(3600)"], **skw)
        return sid, {"exe": exe, "warning": "binary missing, stub process", "stats": stats, "hashlog": hashlog}
    if kind == "sender":
        cmd = [exe,
               "--adapter", str(args.get("adapter", "")),
               "--mode", str(args.get("mode", "pattern")),
               "--pattern", str(args.get("pattern", "motion")),
               "--fps", str(args.get("fps", 240)),
               "--frames", str(args.get("frames", 0)),
               "--ethertype", str(args.get("ethertype", "0x88B5")),
               "--mtu", str(args.get("mtu", 9000)),
               "--pacing-us", str(args.get("pacing_us", args.get("pacing-us", 0))),
               "--dst-mac", str(args.get("dst_mac", args.get("dst-mac", "ff:ff:ff:ff:ff:ff"))),
               "--session", sid, "--stats", stats, "--hashlog", hashlog]
        if args.get("file"): cmd += ["--file", str(args["file"])]
        if args.get("loop"): cmd += ["--loop"]
        _b = args.get("batch", 69)
        try: _b = int(_b)
        except Exception: _b = 69
        if _b < 1: _b = 69
        cmd += ["--batch", str(_b)]
    else:
        cmd = [exe,
               "--adapter", str(args.get("adapter", "")),
               "--ethertype", str(args.get("ethertype", "0x88B5")),
               "--mtu", str(args.get("mtu", 9000)),
               "--session", sid, "--stats", stats, "--hashlog", hashlog,
               "--timeout-s", str(args.get("timeout_s", args.get("timeout-s", 0)))]
        _ef = args.get("expect_frames", args.get("expect-frames", None))
        if _ef is not None and int(_ef) > 0:
            cmd += ["--expect-frames", str(_ef)]
        _shm = args.get("shm", None)
        if _shm:
            cmd += ["--shm", str(_shm)]
    # 过滤空网卡参数
    cmd2 = []
    skip_next = False
    for i, c in enumerate(cmd):
        if skip_next: skip_next = False; continue
        if c == "--adapter" and (i + 1 < len(cmd) and cmd[i+1] == ""):
            skip_next = True; continue
        cmd2.append(c)
    slot.update(session_id=sid, started_at=time.time(), stats=stats, hashlog=hashlog)
    # 截断输出防爆
    open(hashlog, "w", encoding="utf-8").close()
    popen_kw = {"cwd": ROOT, "stdout": open(os.path.join(RUN_DIR, sid + "." + kind + ".stdout.log"), "wb"),
                "stderr": subprocess.STDOUT}
    if os.name == "nt":
        popen_kw["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    proc = subprocess.Popen(cmd2, **popen_kw)
    slot["proc"] = proc
    return sid, {"cmd": cmd2, "stats": stats, "hashlog": hashlog, "pid": proc.pid}

def get_adapters():
    """列网卡：跑--list-adapters，不起进程"""
    import re
    exe = ""
    for cand in [exe_path("sender"), exe_path("receiver")]:
        if cand and os.path.exists(cand):
            exe = cand
            break
    if not exe:
        return 503, {"error": "no sender/receiver binary; build first per docs/build.md", "text": "", "names": []}
    try:
        p = subprocess.run([exe, "--list-adapters"], capture_output=True, text=True, timeout=15, cwd=ROOT)
        text = (p.stdout or "") + (("\n" + p.stderr) if p.stderr else "")
        names = []
        for m in re.finditer(r"name=(\S+)", text):
            nm = m.group(1).strip()
            if nm and nm != "(null)" and nm not in names:
                names.append(nm)
        # 兼容无name=的行
        if not names:
            for line in text.splitlines():
                line=line.strip()
                if line.lower().startswith("adapter["):
                    names.append(line)
        return 200, {"text": text.strip(), "names": names, "exe": os.path.basename(exe)}
    except Exception as e:
        return 500, {"error": str(e), "text": "", "names": []}

def slot_stats(kind):
    # 读槽位统计（缺文件则报停止）
    slot = state[kind]
    sid = slot.get("session_id")
    proc = slot.get("proc")
    running = proc is not None and proc.poll() is None
    up = round(time.time() - slot["started_at"], 2) if slot.get("started_at") else 0
    path = slot.get("stats", "")
    if not path or not os.path.exists(path):
        # 兜底读同类最新run文件
        return {"session_id": sid, "running": running, "uptime_s": up}
    try:
        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
    except Exception as e:
        return {"session_id": sid, "running": running, "uptime_s": up, "stats_error": str(e)}
    if isinstance(data, dict):
        data = dict(data)
        data["running"] = bool(running and data.get("running", True))
        data["uptime_s"] = up
        if data.get("session_id") is None: data["session_id"] = sid
    return data


EXEC_DEFAULT_TIMEOUT = 30
EXEC_MAX_TIMEOUT = 300
EXEC_OUT_CAP = 131072  # per-stream truncation cap
EXEC_LOCK = threading.Lock()

def _under_root(path):
    # 判路径在ROOT内防越界
    try:
        rp = os.path.realpath(path)
        rr = os.path.realpath(ROOT)
        return rp == rr or rp.startswith(rr + os.sep)
    except Exception:
        return False

def _resolve_argv(argv):
    """白名单解析：返回命令或拒绝原因"""
    if not argv or not isinstance(argv, list) or not all(isinstance(a, str) for a in argv):
        return None, "argv must be a non-empty string list"
    if any(a == "" for a in argv):
        return None, "empty arg"
    a0 = argv[0]
    if ".." in a0.replace("\\", "/").split("/"):
        return None, "path traversal denied"
    lo = a0.lower()
    base = os.path.basename(a0)
    # py脚本限ROOT内，走本地解释器
    if lo in ("python", "python3", "python.exe"):
        interp = sys.executable
        if getattr(sys, "frozen", False):
            import shutil as _sh
            interp = _sh.which("python") or _sh.which("python3") or ""
            if not interp: return None, "no python on PATH (frozen agent)"
        rest = list(argv[1:])
        if rest and rest[0] == "-c":
            return None, "inline code denied (use script files under ROOT)"
        for a in rest:
            if a.lower().endswith((".py", ".ps1")) and not a.startswith("-"):
                cand = a if os.path.isabs(a) else os.path.join(ROOT, a)
                if not _under_root(cand) or not os.path.isfile(cand):
                    return None, "script outside ROOT denied: " + a
        return [interp] + rest, ""
    # pwsh限-File，不接命令串
    if lo in ("pwsh", "powershell", "pwsh.exe", "powershell.exe"):
        if len(argv) < 6 or argv[1] != "-NoProfile" or argv[2] != "-ExecutionPolicy" \
                or argv[3] != "Bypass" or argv[4] != "-File":
            return None, "only 'pwsh -NoProfile -ExecutionPolicy Bypass -File <script-under-ROOT/scripts> [args]' allowed"
        f = argv[5]
        cand = f if os.path.isabs(f) else os.path.join(ROOT, "scripts", os.path.basename(f))
        if not _under_root(cand) or not os.path.isfile(cand):
            return None, "script outside ROOT/scripts denied"
        exe = "pwsh.exe" if lo.startswith("pwsh") else "powershell.exe"
        return [exe, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", cand] + argv[6:], ""
    # 预置诊断命令
    if lo in ("ipconfig", "ipconfig.exe"):
        if len(argv) > 2 or (len(argv) == 2 and argv[1].lower() != "/all"):
            return None, "ipconfig allows no args or /all only"
        sys32 = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32", "ipconfig.exe")
        return [sys32] + argv[1:], ""
    if lo in ("ping", "ping.exe"):
        if len(argv) > 8 or any(a.lower() == "-t" for a in argv):
            return None, "ping denied (-t or too many args)"
        sys32 = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32", "PING.EXE")
        return [sys32] + argv[1:], ""
    # 文件限ROOT内，绝对路径防越界
    if os.path.isabs(a0):
        cand = a0
        if not _under_root(cand):
            return None, "absolute path outside ROOT denied"
    else:
        cand = ""
        for d in (ROOT, os.path.join(ROOT, "build"), os.path.join(ROOT, "scripts")):
            q = os.path.join(d, a0)
            if os.path.isfile(q):
                cand = q; break
            if os.name == "nt" and os.path.isfile(q + ".exe"):
                cand = q + ".exe"; break
        if not cand:
            return None, "not found under ROOT: " + a0
    return [cand] + argv[1:], ""

def run_exec(argv, timeout_s, client):
    # 远控执行：白名单内跑命令
    import uuid as _uuid, time as _time
    run_argv, denied = _resolve_argv(argv)
    if denied:
        return 403, {"denied": denied}
    try:
        timeout_s = max(1, min(int(timeout_s or EXEC_DEFAULT_TIMEOUT), EXEC_MAX_TIMEOUT))
    except Exception:
        return 400, {"denied": "bad timeout_s"}
    t0 = _time.time()
    try:
        r = subprocess.run(run_argv, cwd=ROOT, capture_output=True, timeout=timeout_s)
        ec, to = r.returncode, False
        out, err = r.stdout or b"", r.stderr or b""
    except subprocess.TimeoutExpired as e:
        ec, to = None, True
        out, err = e.stdout or b"", e.stderr or b""
    except Exception as e:
        return 500, {"error": str(e)}
    dur = round(_time.time() - t0, 2)
    def _dec(b):
        try: return b.decode("utf-8")
        except Exception:
            try: return b.decode("cp936")
            except Exception: return b.decode("utf-8", "replace")
    so, se = _dec(out), _dec(err)
    res = {"exec_id": _uuid.uuid4().hex[:8], "exit_code": ec, "timed_out": to,
           "duration_s": dur, "stdout": so[:EXEC_OUT_CAP], "stderr": se[:EXEC_OUT_CAP],
           "truncated": len(so) > EXEC_OUT_CAP or len(se) > EXEC_OUT_CAP}
    try:
        os.makedirs(RUN_DIR, exist_ok=True)
        with EXEC_LOCK:
            with open(os.path.join(RUN_DIR, "exec.log"), "a", encoding="utf-8") as f:
                f.write(json.dumps({"ts": _time.strftime("%Y-%m-%dT%H:%M:%S"), "client": client,
                                    "argv": argv, "exit_code": ec, "timed_out": to,
                                    "duration_s": dur}) + "\n")
    except Exception:
        pass
    return 200, res


UPDATE_LOCK = threading.RLock()
UPDATE_MAX_ZIP = 100 * 1024 * 1024
UPDATE_MAX_FILES = 1000
UPDATE_ALLOWED_ROOT = {"vc-agent.exe", "vc-agent.py", "vc-sender.exe", "vc-receiver.exe", "vc-vcam.dll", "vc-testwin.exe", "version.json"}


FILES_MAX_BYTES = 262144
def read_root_file(rel, offset=0, limit=0):
    # 读ROOT文件分页（供拉日志）
    try:
        offset = int(offset or 0)
        limit = int(limit or 0)
    except Exception:
        return 400, {'error': 'bad offset/limit'}
    if offset < 0:
        offset = 0
    if limit < 0:
        limit = 0
    if limit > 1048576:
        limit = 1048576
    """只读取文件：限ROOT内"""
    if not rel or not isinstance(rel, str):
        return 400, {"error": "path required"}
    nn = rel.replace("\\", "/")
    if nn.startswith("/") or nn in ("", ".") or nn.startswith("..") or "/../" in nn:
        return 403, {"error": "outside ROOT denied"}
    if os.path.isabs(rel):
        return 403, {"error": "absolute path denied"}
    full = os.path.realpath(os.path.join(ROOT, nn.replace("/", os.sep)))
    if not _under_root(full):
        return 403, {"error": "outside ROOT denied"}
    if not os.path.isfile(full):
        return 404, {"error": "not found"}
    try:
        size = os.path.getsize(full)
        with open(full, 'rb') as f:
            f.seek(offset)
            want = limit if limit else FILES_MAX_BYTES
            data = f.read(want + 1)
        trunc = len(data) > want
        data = data[:want]
        try:
            text = data.decode("utf-8")
        except Exception:
            try:
                text = data.decode("cp936")
            except Exception:
                text = data.decode("utf-8", "replace")
        if trunc:
            nxt = offset + want
        else:
            nxt = None
        return 200, {'path': nn, 'size': size, 'offset': offset, 'truncated': trunc, 'next_offset': nxt, 'content': text}
    except Exception as e:
        return 500, {"error": str(e)}

def _update_slots():
    """本运行时管理槽位绝对路径"""
    m = {}
    if getattr(sys, "frozen", False):
        m["vc-agent.exe"] = os.path.abspath(sys.executable)
    else:
        try: m["vc-agent.py"] = _agent_script_path()
        except Exception: pass
    m["vc-sender.exe"] = os.path.join(BUILD_DIR, "vc-sender.exe")
    m["vc-receiver.exe"] = os.path.join(BUILD_DIR, "vc-receiver.exe")
    m["vc-vcam.dll"] = os.path.join(BUILD_DIR, "vc-vcam.dll")
    m["vc-testwin.exe"] = os.path.join(BUILD_DIR, "vc-testwin.exe")
    m["version.json"] = VERSION_FILE
    return m

def _safe_zip_entries(zf):
    # 验zip条目防越界
    items, total = [], 0
    names = zf.namelist()
    if len(names) > UPDATE_MAX_FILES:
        return None, "too many files"
    for n in names:
        if n.endswith("/"):
            continue
        nn = n.replace("\\", "/")
        if nn.startswith("/") or nn in ("", ".") or nn.startswith("..") or "/../" in nn:
            return None, "zip-slip entry denied: " + n
        parts = nn.split("/")
        if len(parts) == 1:
            if parts[0] not in UPDATE_ALLOWED_ROOT:
                return None, "entry not allowed: " + n
            rel = parts[0]
        elif len(parts) == 2 and parts[0] == "scripts" and parts[1].lower().endswith(".ps1") \
                and parts[1] not in (".", ".."):
            rel = "scripts/" + parts[1]
        else:
            return None, "entry not allowed: " + n
        info = zf.getinfo(n)
        total += info.file_size
        if total > 2 * UPDATE_MAX_ZIP:
            return None, "extracted too large"
        items.append((n, rel))
    return items, ""

def _live_for_rel(rel, slots):
    # 判运行文件绝对路径
    if rel in ("vc-agent.exe", "vc-agent.py", "vc-sender.exe", "vc-receiver.exe", "vc-vcam.dll", "vc-testwin.exe", "version.json"):
        return slots.get(rel)
    if rel.startswith("scripts/"):
        return os.path.join(ROOT, rel.replace("/", os.sep))
    return None


STAGED = {"version": None, "dir": None}

def _agent_script_path():
    # 取agent自身脚本路径
    try:
        _c = os.path.join(ROOT, "agent", "vc-agent.py")
        if os.path.isfile(_c):
            return _c
    except Exception:
        pass
    try:
        return os.path.abspath(__file__)
    except Exception:
        return ""

def _all_managed_files():
    # 列管控文件（OTA范围）
    _out = []
    try:
        for _rel, _live in _update_slots().items():
            if not _live:
                continue
            try:
                _rp = os.path.realpath(_live)
            except Exception:
                continue
            if not _under_root(_rp):
                continue
            _out.append((_rel, _live))
    except Exception:
        pass
    try:
        _sp = os.path.join(ROOT, "scripts")
        if os.path.isdir(_sp):
            for _fn in sorted(os.listdir(_sp)):
                if _fn.lower().endswith(".ps1"):
                    _fp = os.path.join(_sp, _fn)
                    if os.path.isfile(_fp):
                        _out.append((os.path.join("scripts", _fn), _fp))
    except Exception:
        pass
    return _out

def _full_prev_snapshot():
    # 全量备份prev（回滚用）
    import shutil as _sh
    _prev = os.path.join(ROOT, "prev", "mirror")
    if os.path.isdir(_prev):
        _sh.rmtree(_prev, ignore_errors=True)
    for _rel, _live in _all_managed_files():
        if os.path.isfile(_live):
            _dest = os.path.join(_prev, _rel)
            _dd = os.path.dirname(_dest)
            if _dd:
                os.makedirs(_dd, exist_ok=True)
            _sh.copy2(_live, _dest)
    return _prev

def _versioned_exe_name(ver):
    # 版本化exe名
    _safe = "".join(c if c.isalnum() else "_" for c in ver) or "update"
    return "vc-agent-%s.exe" % _safe

def _gc_versioned_layout():
    # 清旧版本留最新
    try:
        _me = os.path.realpath(sys.executable) if getattr(sys, "frozen", False) else ""
        _keep = set()
        if _me:
            _keep.add(_me)
        try:
            _vd = json.load(open(os.path.join(ROOT, "version.json"), encoding="utf-8"))
            _px = (_vd.get("exe", "") or "")
            if _px:
                _keep.add(os.path.realpath(os.path.join(ROOT, _px)))
        except Exception:
            pass
        import glob as _gg
        for _ff in _gg.glob(os.path.join(ROOT, "vc-agent-*.exe")):
            try:
                if os.path.realpath(_ff) not in _keep and os.path.isfile(_ff):
                    os.remove(_ff)
            except Exception:
                pass
        _st = os.path.join(ROOT, "staging")
        if os.path.isdir(_st):
            for _e in os.listdir(_st):
                try:
                    _p = os.path.join(_st, _e)
                    if os.path.isdir(_p):
                        import shutil as _sh2
                        _sh2.rmtree(_p, ignore_errors=True)
                    else:
                        os.remove(_p)
                except Exception:
                    pass
        for _e in os.listdir(ROOT):
            try:
                if _e.startswith("update_helper-") and _e.endswith(".ps1"):
                    os.remove(os.path.join(ROOT, _e))
            except Exception:
                pass
    except Exception:
        pass

def _apply_staged_to_live(stag, newver):
    # staging搬live
    import shutil as _sh
    import time as _tt
    _slots = _update_slots()
    _applied = []
    _exe_live = None
    try:
        _me_img = os.path.realpath(sys.executable) if getattr(sys, "frozen", False) else ""
    except Exception:
        _me_img = ""
    for _dp, _dn, _fn in os.walk(stag):
        for _f in sorted(_fn):
            _src = os.path.join(_dp, _f)
            _rel = os.path.relpath(_src, stag).replace(chr(92), "/")
            if _rel == "vc-agent.exe" and getattr(sys, "frozen", False):
                _live = os.path.join(ROOT, _versioned_exe_name(newver))
            else:
                _live = _live_for_rel(_rel, _slots)
            if not _live:
                continue
            try:
                if not _under_root(os.path.realpath(_live)):
                    continue
            except Exception:
                continue
            try:
                _is_me = bool(_me_img) and os.path.isfile(_live) and os.path.realpath(_live) == _me_img
            except Exception:
                _is_me = False
            if _is_me:
                _bd, _bn = os.path.dirname(_live), os.path.basename(_live)
                _stem, _ext = os.path.splitext(_bn)
                if not _ext:
                    _ext = ".exe"
                _cand = os.path.join(_bd, "%s-%d%s" % (_stem, int(_tt.time()), _ext))
                _k = 0
                while os.path.isfile(_cand):
                    _k += 1
                    _cand = os.path.join(_bd, "%s-%d-%d%s" % (_stem, int(_tt.time()), _k, _ext))
                _live = _cand
            _dd = os.path.dirname(_live)
            if _dd:
                os.makedirs(_dd, exist_ok=True)
            _sh.copy2(_src, _live)
            _applied.append(_rel)
            if _rel in ("vc-agent.exe", "vc-agent.py"):
                _exe_live = _live
    return _applied, _exe_live

def _do_activate(ver_or_none, is_rollback):
    # 执行切换/回滚
    with UPDATE_LOCK:
        if is_rollback:
            _prev = os.path.join(ROOT, "prev", "mirror")
            if not os.path.isdir(_prev) or not os.listdir(_prev):
                return 409, {"error": "no backup"}
            _target = _prev
            try:
                _pvf = os.path.join(_prev, "version.json")
                _want = json.loads(open(_pvf, encoding="utf-8").read()).get("version", None) if os.path.isfile(_pvf) else None
            except Exception:
                _want = None
        else:
            if not STAGED.get("version") or not STAGED.get("dir"):
                return 409, {"error": "nothing staged"}
            if ver_or_none and ver_or_none != STAGED["version"]:
                return 409, {"error": "staged version mismatch"}
            _target = STAGED["dir"]
            if not os.path.isdir(_target):
                STAGED["version"] = None; STAGED["dir"] = None
                return 409, {"error": "staged files gone"}
            _want = STAGED["version"]
        _sp = os.path.join(_target, "vc-agent.py")
        if os.path.isfile(_sp) and not getattr(sys, "frozen", False):
            try:
                import py_compile as _pc
                _pc.compile(_sp, doraise=True)
            except Exception:
                return 500, {"error": "staged vc-agent.py does not compile"}
        kill_slot(state["sender"])
        kill_slot(state["receiver"])
        _vn = _want if isinstance(_want, str) and _want else "rollback"
        _applied, _exe_live = _apply_staged_to_live(_target, _vn)
        if not _applied:
            return 500, {"error": "nothing applied"}
        if getattr(sys, "frozen", False) and isinstance(_want, str) and _want and _exe_live:
            try:
                _vf = os.path.join(ROOT, "version.json")
                _vd = json.loads(open(_vf, encoding="utf-8").read())
                _vd["exe"] = os.path.basename(_exe_live)
                open(_vf, "w", encoding="utf-8").write(json.dumps(_vd, indent=1))
            except Exception:
                pass
        try:
            with open(os.path.join(RUN_DIR, "updates.log"), "a", encoding="utf-8") as _f:
                _f.write(str((int(time.time()), is_rollback, _want)) + chr(10))
        except Exception:
            pass
        STAGED["version"] = None; STAGED["dir"] = None
        return 200, {"activated": True, "version": _want, "rollback": is_rollback, "restart": "execv", "exe": (os.path.basename(_exe_live) if _exe_live else None)}
def _execv_self(new_exe=None):
    # 原地换代（execv）
    if getattr(sys, "frozen", False):
        _exe = new_exe or os.path.abspath(sys.executable)
        os.execv(_exe, [_exe] + sys.argv[1:])
    else:
        _py = getattr(sys, "_base_executable", None) or sys.executable
        _script = _agent_script_path()
        os.execv(_py, [_py, _script] + sys.argv[1:])
def handle_activate(body_args):
    # 处理activate请求
    _v = body_args.get("version") if isinstance(body_args, dict) else None
    return _do_activate(_v if isinstance(_v, str) else None, False)
def handle_rollback():
    # 处理回滚请求
    return _do_activate(None, True)
def handle_update_binary(body):
    # 处理推包（验zip存staging）
    import shutil as _sh, zipfile as _zf
    os.makedirs(RUN_DIR, exist_ok=True)
    tmp = os.path.join(RUN_DIR, "update-incoming.zip")
    with open(tmp, "wb") as f:
        f.write(body)
    if not _zf.is_zipfile(tmp):
        return 400, {"error": "not a zip"}
    with _zf.ZipFile(tmp) as zf:
        items, reason = _safe_zip_entries(zf)
        if items is None:
            return 400, {"error": reason}
        vnames = [n for n, rel in items if rel == "version.json"]
        if not vnames:
            return 400, {"error": "version.json required"}
        try:
            newver = json.loads(zf.read(vnames[0]).decode("utf-8")).get("version", "")
        except Exception:
            return 400, {"error": "bad version.json"}
        if not newver or not isinstance(newver, str):
            return 400, {"error": "bad version.json"}
        slots = _update_slots()
        pairs = []
        for n, rel in items:
            live = _live_for_rel(rel, slots)
            if not live or not _under_root(live):
                continue
            pairs.append((rel, live, n))
        if not pairs:
            return 400, {'error': 'no applicable files for this layout'}
        for _n, _rel in items:
            if _rel in ('vc-agent.exe', 'vc-agent.py') and _rel not in [r for r, _lv, _nn in pairs]:
                return 400, {'error': 'self update not applicable: ' + _rel + ' outside ROOT'}
        self_up = [r for r, live, n in pairs if r in ("vc-agent.exe", "vc-agent.py")]
        if not self_up:
            for _sk in ("sender", "receiver"):
                _pr = state[_sk].get("proc")
                if _pr is not None and _pr.poll() is None:
                    return 409, {"error": "stop " + _sk + " session first (exe locked)"}
        safe = "".join(c if (c.isalnum() or c in ".-_") else "_" for c in newver)
        stag = os.path.join(ROOT, "staging", "mirror-" + safe)
        if os.path.isdir(stag):
            _sh.rmtree(stag, ignore_errors=True)
        prev = os.path.join(ROOT, "prev", "mirror")
        # 同版本重打保留旧备，回滚用
        if newver != read_version():
            _full_prev_snapshot()
        for relp, live, n in pairs:
            dest = os.path.join(stag, relp)
            os.makedirs(os.path.dirname(dest) or stag, exist_ok=True)
            with zf.open(n) as src, open(dest, "wb") as f:
                _sh.copyfileobj(src, f)
    try:
        os.remove(tmp)
    except Exception:
        pass
    if not self_up:
        kill_slot(state["sender"])
        kill_slot(state["receiver"])
        for _rp, _lv, _nn in pairs:
            _dd = os.path.dirname(_lv)
            if _dd:
                os.makedirs(_dd, exist_ok=True)
        for relp, live, n in pairs:
            _sh.copy2(os.path.join(stag, relp), live)
        try:
            os.remove(tmp)
        except Exception:
            pass
        try:
            with open(os.path.join(RUN_DIR, "updates.log"), "a", encoding="utf-8") as f:
                f.write(json.dumps({"ts": time.strftime("%Y-%m-%dT%H:%M:%S"), "action": "update-live", "version": newver, "from": read_version()}) + "\n")
        except Exception:
            pass
        return 200, {"updated": True, "version": newver, "restart": "none"}
    STAGED["version"] = newver
    STAGED["dir"] = stag
    try:
        with open(os.path.join(RUN_DIR, "updates.log"), "a", encoding="utf-8") as f:
            f.write(str(int(time.time())) + " staged " + str(newver) + chr(10))
    except Exception:
        pass
    return 200, {"staged": True, "version": newver}



def _respond_activate(handler, code, res):
    # 回activate应答
    handler._j(res, code)
    if code != 200 or not isinstance(res, dict) or res.get("restart") != "execv":
        return
    try:
        handler.wfile.flush()
    except Exception:
        pass
    try:
        _nx = None
        if getattr(sys, "frozen", False):
            _en = res.get("exe") if isinstance(res, dict) else None
            if isinstance(_en, str) and _en and "/" not in _en and chr(92) not in _en and _en.lower().endswith(".exe"):
                _cand = os.path.realpath(os.path.join(ROOT, _en))
                try:
                    _rr = os.path.realpath(ROOT)
                    _ok = _cand == _rr or _cand.startswith(_rr + os.sep)
                except Exception:
                    _ok = False
                if _ok and os.path.isfile(_cand):
                    _nx = _cand
        if _nx is None and getattr(sys, "frozen", False):
            _rv = res.get("version") if isinstance(res, dict) else None
            _rv = _rv if isinstance(_rv, str) and _rv else "rollback"
            _nx = os.path.join(ROOT, _versioned_exe_name(_rv))
        _execv_self(_nx)
    except Exception as _ex:
        try:
            _ef = open(os.path.join(RUN_DIR, "updates.log"), "a", encoding="utf-8")
            _ef.write("execv failed ")
            _ef.close()
        except Exception:
            pass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class H(BaseHTTPRequestHandler):
    def _j(self, o, code=200):
        b = json.dumps(o).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        if self.path == "/health":
            anyrun = any((state[k].get("proc") is not None and state[k]["proc"].poll() is None) for k in ("sender", "receiver"))
            self._j({"ok": True, "version": read_version(), "role": state["role"], "running": anyrun, "updater": updater_status(), "pid": os.getpid(), "staged": STAGED.get("version")})
        elif self.path == "/version":
            self._j({"version": read_version()})
        elif self.path == "/adapters":
            code, res = get_adapters()
            self._j(res, code)
        elif self.path == "/sender/stats":
            self._j(slot_stats("sender"))
        elif self.path == "/receiver/stats":
            self._j(slot_stats("receiver"))
        elif self.path == "/receiver/report":
            # 打包用统计+版本+文件存在性
            self._j({"version": read_version(), "sender": slot_stats("sender"), "receiver": slot_stats("receiver")})
        elif self.path == '/files' or self.path.startswith('/files?'):
            try:
                import urllib.parse as _up2
                q = _up2.parse_qs(_up2.urlparse(self.path).query)
                rel = (q.get('path', [''])[0])
                _off = (q.get('offset', ['0'])[0])
                _lim = (q.get('limit', ['0'])[0])
            except Exception:
                rel = ''; _off = 0; _lim = 0
            code, res = read_root_file(rel, _off, _lim)
            self._j(res, code)
        elif self.path == "/exec/log" or self.path.startswith("/exec/log?"):
            try:
                import urllib.parse as _up
                q = _up.urlparse(self.path).query
                n = int((_up.parse_qs(q).get("lines", ["50"])[0]))
                n = max(1, min(n, 500))
            except Exception:
                n = 50
            entries = []
            try:
                with open(os.path.join(RUN_DIR, "exec.log"), "r", encoding="utf-8") as f:
                    lines = f.read().splitlines()
                for line in lines[-n:]:
                    try: entries.append(json.loads(line))
                    except Exception: pass
            except Exception:
                pass
            self._j({"entries": entries})
        else:
            self._j({"err": "unknown"}, 404)
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0)); body = self.rfile.read(n) if n else b"{}"
        try: args = json.loads(body or b"{}")
        except Exception: args = {}
        if self.path == "/sender/start":
            _a = (args if isinstance(args, dict) else {})
            if not str(_a.get("adapter", "")).strip():
                _code, _ad = get_adapters()
                self._j({"error": "missing --adapter; GET /adapters first", "adapters": _ad}, 400)
            else:
                sid, info = launch("sender", _a)
                self._j({"session_id": sid, "args": info})
        elif self.path == "/receiver/start":
            _a = (args if isinstance(args, dict) else {})
            if not str(_a.get("adapter", "")).strip():
                _code, _ad = get_adapters()
                self._j({"error": "missing --adapter; GET /adapters first", "adapters": _ad}, 400)
            else:
                sid, info = launch("receiver", _a)
                self._j({"session_id": sid, "args": info})
        elif self.path in ("/sender/stop", "/receiver/stop"):
            kind = "sender" if self.path.startswith("/sender") else "receiver"
            sid = state[kind].get("session_id")
            kill_slot(state[kind])
            # 停后标运行假，留文件对账
            try:
                p = state[kind].get("stats", "")
                if p and os.path.exists(p):
                    with open(p, "r", encoding="utf-8") as f: d = json.load(f)
                    if isinstance(d, dict): d["running"] = False
                    else: d = {"session_id": sid, "running": False}
                    tmp = p + ".tmp"
                    with open(tmp, "w", encoding="utf-8") as f: json.dump(d, f)
                    os.replace(tmp, p)
            except Exception: pass
            try:
                os.makedirs(RUN_DIR, exist_ok=True)
                with EXEC_LOCK:
                    with open(os.path.join(RUN_DIR, "exec.log"), "a", encoding="utf-8") as _f:
                        _f.write(json.dumps({"ts": time.strftime("%Y-%m-%dT%H:%M:%S"), "client": self.client_address[0] if self.client_address else "?", "op": kind + "/stop", "session_id": sid}) + chr(10))
            except Exception: pass
            self._j({"session_id": sid, "stopped": True})
        elif self.path == "/exec":
            if not isinstance(args, dict):
                self._j({"denied": "body must be {argv:[...], timeout_s:N}"}, 400)
            else:
                code, res = run_exec(args.get("argv"), args.get("timeout_s", EXEC_DEFAULT_TIMEOUT),
                                     self.client_address[0] if self.client_address else "?")
                self._j(res, code)
        elif self.path == "/update":
            ctype = (self.headers.get('Content-Type', '') or '').lower()
            if isinstance(args, dict) and args.get("rollback"):
                try:
                    code, res = _do_activate(None, True)
                except Exception as _e:
                    code, res = 500, {"error": "activate failed: %s" % _e}
                _respond_activate(self, code, res)



            elif body[:2] == b"PK" or "zip" in ctype or "octet-stream" in ctype:
                if len(body) > UPDATE_MAX_ZIP:
                    self._j({"error": "zip too large"}, 413)
                else:
                    with UPDATE_LOCK:
                        try:
                            code, res = handle_update_binary(body)
                        except Exception as _e:
                            code, res = 500, {"error": "stage failed: %s" % _e}
                    self._j(res, code)
            else:
                self._j({"updated": False, "version": read_version(),
                         "note": "POST zip bytes (Content-Type: application/zip) or {rollback:true}"}, 400)
        elif self.path == "/activate":
            _ab = args if isinstance(args, dict) else {}
            if _ab.get("rollback"):
                with UPDATE_LOCK:
                    try:
                        code, res = _do_activate(None, True)
                    except Exception as _e:
                        code, res = 500, {"error": "activate failed: %s" % _e}
            else:
                _av = _ab.get("version")
                with UPDATE_LOCK:
                    try:
                        code, res = _do_activate(_av if isinstance(_av, str) else None, False)
                    except Exception as _e:
                        code, res = 500, {"error": "activate failed: %s" % _e}
            _respond_activate(self, code, res)
        else:
            self._j({"err": "unknown"}, 404)
    def log_message(self, *a): pass

if __name__ == "__main__":
    ap = argparse.ArgumentParser(); ap.add_argument("--role", default="sender")
    ap.add_argument("--port", type=int, default=18080)
    ap.add_argument("--root", default=""); a = ap.parse_args()
    if a.root: ROOT = a.root; RUN_DIR = os.path.join(ROOT, "run"); BUILD_DIR = os.path.join(ROOT, "build"); VERSION_FILE = os.path.join(ROOT, "version.json")
    state["role"] = a.role
    state["port"] = a.port
    print("[agent] role=%s port=%d version=%s root=%s" % (a.role, a.port, read_version(), ROOT), flush=True)
    ThreadingHTTPServer.daemon_threads = True
    _gc_versioned_layout()
    ThreadingHTTPServer.allow_reuse_address = True
    ThreadingHTTPServer(("0.0.0.0", a.port), H).serve_forever()








