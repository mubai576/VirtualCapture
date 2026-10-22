"""vc-agent.py — T-03 control agent (PLAN §2.3, agent-api.md frozen).
Single file, zero third-party deps, dual-PC same code (--role sender|receiver).
- /sender|receiver/start: assemble CLI per PLAN §2.2, cwd=project root,
  stats/hashlog → run/<session>.sender|receiver.stats.json + .hash.jsonl,
  record pid; idempotent (kill old first); return session_id.
- /stop: graceful 2s then kill, keep files for reconcile.
- /stats: return stats file verbatim + running/uptime; missing → {running:false}.
- /update: validate zip, extract to staging mirror, snapshot full-tree prev, no restart; returns staged version.
- /activate: apply staged (or rollback prev) then in-process execv; no detached processes.
- /version from version.json else "dev".
Acceptance: with T-01/T-02 --selftest exes the start/stop/stats file flow still works.
"""
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
    """Pre-flight for the PS restart helper: ps-ok | ps-missing | unchecked."""
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
    try:
        with open(VERSION_FILE, "r", encoding="utf-8") as f:
            return json.load(f).get("version", VERSION_DEFAULT)
    except Exception:
        return VERSION_DEFAULT

state = {"role": None, "port": 18080,
         "sender": {"session_id": None, "proc": None, "started_at": 0, "stats": "", "hashlog": ""},
         "receiver": {"session_id": None, "proc": None, "started_at": 0, "stats": "", "hashlog": ""}}

def exe_path(kind):
    # kind sender|receiver → build/vc-sender.exe / build/vc-receiver.exe ; fallback .exe in root?
    name = "vc-sender.exe" if kind == "sender" else "vc-receiver.exe"
    p = os.path.join(BUILD_DIR, name)
    if os.path.exists(p): return p
    # also accept build/Release etc (MSVC multi-config)
    for cand in [os.path.join(BUILD_DIR, "Release", name), os.path.join(BUILD_DIR, "Debug", name),
                 os.path.join(ROOT, name)]:
        if os.path.exists(cand): return cand
    return p  # return expected path so error message is clear

def kill_slot(slot):
    p = slot.get("proc")
    if p is None: return
    try:
        if p.poll() is None:
            try:
                if os.name == "nt":
                    # graceful: CTRL_BREAK to the child process group (exe flushes final stats)
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
    slot = state[kind]
    kill_slot(slot)  # idempotent: stop old first
    sid = "s-%s" % uuid.uuid4().hex[:8]
    os.makedirs(RUN_DIR, exist_ok=True)
    stats = os.path.join(RUN_DIR, "%s.%s.stats.json" % (sid, kind))
    hashlog = os.path.join(RUN_DIR, "%s.%s.hash.jsonl" % (sid, kind))
    exe = exe_path(kind)
    if not os.path.exists(exe):
        # No binary yet (pre-T01/T02): run a stub long-lived process so ctrl flow still works,
        # and write stub stats/hashlog files so /stats + reconcile have real files.
        # Stub mimics C++ file IPC: stats atomic write + hashlog append.
        slot.update(session_id=sid, started_at=time.time(), stats=stats, hashlog=hashlog)
        stub_stats = {"session_id": sid, "running": True, "note": "stub-no-binary",
                      "sent_frames": 0, "complete": 0, "version": read_version()}
        with open(stats, "w", encoding="utf-8") as f: json.dump(stub_stats, f)
        open(hashlog, "w", encoding="utf-8").close()
        # placeholder sleeper process (python) so running stays true until /stop
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
    # filter empty --adapter value (exe treats missing as auto)
    cmd2 = []
    skip_next = False
    for i, c in enumerate(cmd):
        if skip_next: skip_next = False; continue
        if c == "--adapter" and (i + 1 < len(cmd) and cmd[i+1] == ""):
            skip_next = True; continue
        cmd2.append(c)
    slot.update(session_id=sid, started_at=time.time(), stats=stats, hashlog=hashlog)
    # truncate outputs
    open(hashlog, "w", encoding="utf-8").close()
    popen_kw = {"cwd": ROOT, "stdout": open(os.path.join(RUN_DIR, sid + "." + kind + ".stdout.log"), "wb"),
                "stderr": subprocess.STDOUT}
    if os.name == "nt":
        popen_kw["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    proc = subprocess.Popen(cmd2, **popen_kw)
    slot["proc"] = proc
    return sid, {"cmd": cmd2, "stats": stats, "hashlog": hashlog, "pid": proc.pid}

def get_adapters():
    """P1-1: run exe --list-adapters, return (code, {text, names[]}). No process launch."""
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
        # also accept adapter[...] lines without name= (fallback: whole line)
        if not names:
            for line in text.splitlines():
                line=line.strip()
                if line.lower().startswith("adapter["):
                    names.append(line)
        return 200, {"text": text.strip(), "names": names, "exe": os.path.basename(exe)}
    except Exception as e:
        return 500, {"error": str(e), "text": "", "names": []}

def slot_stats(kind):
    slot = state[kind]
    sid = slot.get("session_id")
    proc = slot.get("proc")
    running = proc is not None and proc.poll() is None
    up = round(time.time() - slot["started_at"], 2) if slot.get("started_at") else 0
    path = slot.get("stats", "")
    if not path or not os.path.exists(path):
        # also try latest run file for this kind (survive agent restart within same session?)
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
    try:
        rp = os.path.realpath(path)
        rr = os.path.realpath(ROOT)
        return rp == rr or rp.startswith(rr + os.sep)
    except Exception:
        return False

def _resolve_argv(argv):
    """Allowlist resolution. Returns (run_argv, denied_reason)."""
    if not argv or not isinstance(argv, list) or not all(isinstance(a, str) for a in argv):
        return None, "argv must be a non-empty string list"
    if any(a == "" for a in argv):
        return None, "empty arg"
    a0 = argv[0]
    if ".." in a0.replace("\\", "/").split("/"):
        return None, "path traversal denied"
    lo = a0.lower()
    base = os.path.basename(a0)
    # python scripts: only files under ROOT, via local interpreter
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
    # pwsh: only -File scripts under ROOT (no -Command/-EncodedCommand)
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
    # canned diagnostics
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
    # files under ROOT: bare name searched in ROOT/build/scripts, abs path must stay under ROOT
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
UPDATE_ALLOWED_ROOT = {"vc-agent.exe", "vc-agent.py", "vc-sender.exe", "vc-receiver.exe", "version.json"}


FILES_MAX_BYTES = 262144

def read_root_file(rel):
    """Read-only file fetch under ROOT for automation/diagnosis. Returns (code, obj)."""
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
        if size > 4 * 1024 * 1024:
            return 413, {"error": "file too large"}
        with open(full, "rb") as f:
            data = f.read(FILES_MAX_BYTES + 1)
        trunc = len(data) > FILES_MAX_BYTES
        data = data[:FILES_MAX_BYTES]
        try:
            text = data.decode("utf-8")
        except Exception:
            try:
                text = data.decode("cp936")
            except Exception:
                text = data.decode("utf-8", "replace")
        return 200, {"path": nn, "size": size, "truncated": trunc, "content": text}
    except Exception as e:
        return 500, {"error": str(e)}

def _update_slots():
    """Live absolute paths of managed slots for this runtime."""
    m = {}
    if getattr(sys, "frozen", False):
        m["vc-agent.exe"] = os.path.abspath(sys.executable)
    else:
        try: m["vc-agent.py"] = _agent_script_path()
        except Exception: pass
    m["vc-sender.exe"] = os.path.join(BUILD_DIR, "vc-sender.exe")
    m["vc-receiver.exe"] = os.path.join(BUILD_DIR, "vc-receiver.exe")
    m["version.json"] = VERSION_FILE
    return m

def _safe_zip_entries(zf):
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
    if rel in ("vc-agent.exe", "vc-agent.py", "vc-sender.exe", "vc-receiver.exe", "version.json"):
        return slots.get(rel)
    if rel.startswith("scripts/"):
        return os.path.join(ROOT, rel.replace("/", os.sep))
    return None


STAGED = {"version": None, "dir": None}

def _agent_script_path():
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
    _safe = "".join(c if c.isalnum() else "_" for c in ver) or "update"
    return "vc-agent-%s.exe" % _safe

def _gc_versioned_layout():
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
    import shutil as _sh
    _slots = _update_slots()
    _applied = []
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
            _dd = os.path.dirname(_live)
            if _dd:
                os.makedirs(_dd, exist_ok=True)
            _sh.copy2(_src, _live)
            _applied.append(_rel)
    return _applied

def _do_activate(ver_or_none, is_rollback):
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
        _applied = _apply_staged_to_live(_target, _vn)
        if not _applied:
            return 500, {"error": "nothing applied"}
        if getattr(sys, "frozen", False) and isinstance(_want, str) and _want:
            try:
                _vf = os.path.join(ROOT, "version.json")
                _vd = json.loads(open(_vf, encoding="utf-8").read())
                _vd["exe"] = _versioned_exe_name(_want)
                open(_vf, "w", encoding="utf-8").write(json.dumps(_vd, indent=1))
            except Exception:
                pass
        try:
            with open(os.path.join(RUN_DIR, "updates.log"), "a", encoding="utf-8") as _f:
                _f.write(str((int(time.time()), is_rollback, _want)) + chr(10))
        except Exception:
            pass
        STAGED["version"] = None; STAGED["dir"] = None
        return 200, {"activated": True, "version": _want, "rollback": is_rollback, "restart": "execv"}
def _execv_self(new_exe=None):
    if getattr(sys, "frozen", False):
        _exe = new_exe or os.path.abspath(sys.executable)
        os.execv(_exe, [_exe] + sys.argv[1:])
    else:
        _py = getattr(sys, "_base_executable", None) or sys.executable
        _script = _agent_script_path()
        os.execv(_py, [_py, _script] + sys.argv[1:])
def handle_activate(body_args):
    _v = body_args.get("version") if isinstance(body_args, dict) else None
    return _do_activate(_v if isinstance(_v, str) else None, False)
def handle_rollback():
    return _do_activate(None, True)
def handle_update_binary(body):
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
            return 400, {"error": "no applicable files for this layout"}
        pass
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
        # same-version re-apply keeps the older backup (the true rollback target)
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
            _rv = res.get("version")
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
            # bundle stats + versions + file existence for ctrl packaging
            self._j({"version": read_version(), "sender": slot_stats("sender"), "receiver": slot_stats("receiver")})
        elif self.path == "/files" or self.path.startswith("/files?"):
            try:
                import urllib.parse as _up2
                q = _up2.urlparse(self.path).query
                rel = (_up2.parse_qs(q).get("path", [""])[0])
            except Exception:
                rel = ""
            code, res = read_root_file(rel)
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
            # mark stats file running:false (keep file for reconcile)
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
                code, res = _do_activate(None, True)
                _respond_activate(self, code, res)



            elif body[:2] == b"PK" or "zip" in ctype or "octet-stream" in ctype:
                if len(body) > UPDATE_MAX_ZIP:
                    self._j({"error": "zip too large"}, 413)
                else:
                    with UPDATE_LOCK:
                        code, res = handle_update_binary(body)
                    self._j(res, code)
            else:
                self._j({"updated": False, "version": read_version(),
                         "note": "POST zip bytes (Content-Type: application/zip) or {rollback:true}"}, 400)
        elif self.path == "/activate":
            _ab = args if isinstance(args, dict) else {}
            if _ab.get("rollback"):
                with UPDATE_LOCK:
                    code, res = _do_activate(None, True)
            else:
                _av = _ab.get("version")
                with UPDATE_LOCK:
                    code, res = _do_activate(_av if isinstance(_av, str) else None, False)
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








