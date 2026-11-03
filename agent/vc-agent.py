"""双机控制agent（T-03，接口冻结）
单文件零依赖，双机同一份代码。
启停查：组CLI→记pid→幂等启停；stats/hashlog落run/。
升级：验zip→staging→快照prev→execv切换，不起孤进程。"""
import argparse, time, uuid, json, os, sys, signal, subprocess, shutil, zipfile, threading, ipaddress, socket

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

DEFAULT_PORT = 5143
state = {"role": None, "port": DEFAULT_PORT, "vcam_managed": False,
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
    errors = low_level_stream_errors(kind, args)
    if errors: raise ValueError("；".join(errors))
    slot = state[kind]
    kill_slot(slot)  # idempotent: stop old first
    slot["adapter"] = str(args.get("adapter", ""))
    slot["stdout"] = ""
    if kind == "sender":
        slot["mode"] = args.get("mode", "pattern")
        slot["pattern"] = args.get("pattern", "motion")
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
        if args.get("gpu_convert") is True: cmd += ["--gpu-convert"]  # P5：产品模式 dda 默认走 GPU
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
    if any(key in args for key in ("width", "height", "source_mac", "transform")):
        cmd += ["--width", str(args.get("width", 640)), "--height", str(args.get("height", 640))]
        if kind == "receiver": cmd += ["--fps", str(args.get("fps", 240))]
        if kind == "sender": cmd += ["--transform", str(args.get("transform", "crop"))]
        if args.get("source_mac"): cmd += ["--source-mac", args["source_mac"]]
    if args.get("codec", "raw") != "raw":
        capture = _merge(CONFIG_DEFAULT["capture"], {key: args[key] for key in ("codec", *CODEC_FIELDS) if key in args})
        cmd += codec_cli(capture, kind)
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
    slot["stdout"] = os.path.join(RUN_DIR, sid + "." + kind + ".stdout.log")
    popen_kw = {"cwd": ROOT, "stdout": open(slot["stdout"], "wb"),
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
        if p.returncode:
            return 503, {"error": "adapter enumeration failed", "text": text.strip(), "names": []}
        items = [{"name": m.group(1), "description": m.group(2).strip()}
                 for m in re.finditer(r"adapter\[\d+\] name=(\S+) desc=([^\r\n]*)", text)]
        metadata = _adapter_metadata()
        for item in items:
            guid = item["name"].rsplit("_", 1)[-1].strip("{}").lower()
            item.update(metadata.get(guid, {}))
        return 200, {"text": text.strip(), "names": names, "items": items, "exe": os.path.basename(exe)}
    except Exception as e:
        return 500, {"error": str(e), "text": "", "names": []}


def _adapter_metadata():
    """关联 Npcap GUID 与系统网卡，用控制连接路由选择采集网口。"""
    if os.name != "nt": return {}
    script = r'''[Console]::OutputEncoding = [Text.UTF8Encoding]::new()
$ips = @(Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue)
@(Get-NetAdapter -IncludeHidden | ForEach-Object {
  $nic = $_
  [pscustomobject]@{guid=[string]$nic.InterfaceGuid; label=$nic.Name; mac=$nic.MacAddress;
    status=[string]$nic.Status; receive_link_bps=[uint64]$nic.ReceiveLinkSpeed;
    transmit_link_bps=[uint64]$nic.TransmitLinkSpeed;
    hardware_interface=[bool]$nic.HardwareInterface; virtual=[bool]$nic.Virtual;
    addresses=@($ips | Where-Object InterfaceIndex -eq $nic.ifIndex |
      ForEach-Object { [pscustomobject]@{ip=$_.IPAddress; prefix=$_.PrefixLength} })}
}) | ConvertTo-Json -Depth 4 -Compress'''
    try:
        result = subprocess.run(["powershell.exe", "-NoProfile", "-Command", script],
                                capture_output=True, encoding="utf-8", timeout=15,
                                creationflags=subprocess.CREATE_NO_WINDOW)
        rows = json.loads(result.stdout or "[]")
        if isinstance(rows, dict): rows = [rows]
        return {row["guid"].strip("{}").lower(): {key: value for key, value in row.items() if key != "guid"}
                for row in rows}
    except Exception:
        return {}


def _choose_capture_adapter(controller_ip):
    code, result = get_adapters()
    if code != 200: return code, result
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.connect((controller_ip, DEFAULT_PORT))
            local_ip = probe.getsockname()[0]
    except OSError as error:
        return 409, {"error": "无法确定连接主机的网口：%s" % error}
    matches = [item for item in result.get("items", [])
               if any(addr.get("ip") == local_ip for addr in item.get("addresses", []))]
    if ipaddress.IPv4Address(local_ip).is_loopback:
        # Npcap 回环设备没有 Windows 网卡 GUID，只在本机联调时按精确设备名匹配。
        matches = [item for item in result.get("items", []) if item["name"] == r"\Device\NPF_Loopback"]
    if len(matches) != 1:
        return 409, {"error": "无法唯一匹配被采集机控制连接的网卡，请检查该网口的 IPv4 配置。"}
    return 200, matches[0]

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
        return dict({"session_id": sid, "running": running, "uptime_s": up}, **process_details(kind))
    try:
        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
    except Exception as e:
        return dict({"session_id": sid, "running": running, "uptime_s": up, "stats_error": str(e)}, **process_details(kind))
    if isinstance(data, dict):
        data = dict(data)
        data["running"] = bool(running and data.get("running", True))
        data["uptime_s"] = up
        data["adapter"] = slot.get("adapter", "")
        if data.get("session_id") is None: data["session_id"] = sid
        data.update(process_details(kind))
    return data


def process_details(kind):
    """把原生进程退出和限长日志带回界面，避免只有一个 stopped 标记。"""
    slot = state[kind]; proc = slot.get("proc")
    out = {"adapter": slot.get("adapter", "")}
    if kind == "sender" and slot.get("mode"):
        out.update(mode=slot["mode"], pattern=slot.get("pattern", "motion"))
    if proc is None: return out
    code = proc.poll()
    out.update(pid=getattr(proc, "pid", None), exit_code=code)
    if code is not None and code != 0:
        path = slot.get("stdout", "")
        try:
            with open(path, "rb") as f:
                f.seek(0, os.SEEK_END); size = f.tell(); f.seek(max(0, size - 4096))
                out["log_tail"] = f.read().decode("utf-8", errors="replace")[-2048:]
        except OSError: pass
        label = "采集" if kind == "sender" else "接收"
        out["error"] = "%s程序异常退出（退出码 %s），请检查运行日志。" % (label, code)
        if path: out["stdout"] = os.path.relpath(path, ROOT).replace(os.sep, "/")
    return out


def launch_product(kind, args):
    sid, info = launch(kind, args)
    if deployment_role() and "cmd" in info:
        # 原生程序错误通常在初始化阶段退出；先确认它存活，再向主机报成功。
        deadline = time.monotonic() + 1.0
        while True:
            proc = state[kind]["proc"]
            code = proc.poll() if proc is not None else -1
            if code is not None:
                details = process_details(kind)
                label = "采集" if kind == "sender" else "接收"
                raise RuntimeError("%s程序启动失败（退出码 %s）。%s" %
                                   (label, code, details.get("log_tail", "请检查运行日志。")))
            if time.monotonic() >= deadline: break
            time.sleep(0.05)
    return sid, info


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
        if state.get("vcam_managed"):
            code, cleanup = vcam_unregister()
            if code != 200: return code, {"error": "升级前虚拟采集卡回收失败", "vcam": cleanup}
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
# ===================== P5 GUI（D16 方案 A）=====================
# 统一配置 vc-config.json + 主机开始/停止采集 + vcam 注册 + 内置 Web UI（/ui）。
# 只复用本 agent 已有能力（launch/kill_slot/slot_stats）与对端 agent 的固定端点；无通用反代、不新增数据面逻辑。
import re as _re, urllib.request as _ureq, urllib.error as _uerr
CONFIG_NAME = "vc-config.json"
CONFIG_DEFAULT = {
    "role": "",          # capture | receive | ""（未设）
    "peer_url": "",      # 主机只填被采集机 IPv4，产品端口固定 5143
    "adapter": "",       # 本机 2.5G 网卡子串（--adapter）
    "capture": {"mode": "dda", "pattern": "motion", "gpu_convert": True, "fps": 240, "batch": 69,
                "mtu": 9000, "dst_mac": "ff:ff:ff:ff:ff:ff", "width": 640, "height": 640,
                "transform": "crop", "codec": "raw", "bitrate_mbps": 50, "max_bitrate_mbps": 80,
                "codec_experimental": False, "encoder_backend": "", "decoder_backend": "",
                "encoder_hardware": True, "decoder_hardware": False, "gop_frames": 60,
                "decode_in_flight": 2, "max_au_bytes": 8388608},
    "transport": {"headroom_pct": 20},
    "receive": {"shm": "Local\\VirtualCaptureVcamFrame", "mtu": 9000, "vcam_autoregister": True},
}
CONFIG_LOCK = threading.Lock()
PRODUCT_LOCK = threading.RLock()
VCAM_CLSID = "{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}"


def deployment_role():
    return {"host": "receive", "receiver": "receive", "receive": "receive",
            "capture": "capture", "sender": "capture"}.get(state["role"])


def _product_config(config):
    role = deployment_role()
    if role:
        config["role"] = role
        config["receive"].update(shm=CONFIG_DEFAULT["receive"]["shm"], vcam_autoregister=True)
        # 旧 URL 配置只保留 IP，产品控制端口统一迁移到 5143。
        if role == "receive" and config["peer_url"]:
            match = _re.fullmatch(r"http://([0-9.]+):\d+", config["peer_url"])
            if match:
                try: config["peer_url"] = "http://%s:%d" % (ipaddress.IPv4Address(match[1]), DEFAULT_PORT)
                except ValueError: pass
    return config

def _config_path():
    return os.path.join(ROOT, CONFIG_NAME)

def _merge(base, over):
    # 只接受已知键（未知键丢弃），子对象逐键合并
    out = json.loads(json.dumps(base))
    for k, v in (over or {}).items():
        if k not in out: continue
        if isinstance(out[k], dict):
            if isinstance(v, dict): out[k].update({kk: vv for kk, vv in v.items() if kk in out[k]})
        else:
            out[k] = v
    return out

def load_config():
    try:
        with open(_config_path(), "r", encoding="utf-8") as f: d = json.load(f)
        if isinstance(d, dict): return _product_config(_merge(CONFIG_DEFAULT, d))
    except Exception: pass
    return _product_config(_merge(CONFIG_DEFAULT, {}))

def _is_int(v, lo, hi):
    return isinstance(v, int) and not isinstance(v, bool) and lo <= v <= hi

def validate_config(c):
    """返回错误列表（空=合法）。配置会拼进子进程参数，白名单式校验。"""
    e = []
    cp, rv = c["capture"], c["receive"]
    if c["role"] not in ("", "capture", "receive"): e.append("role must be capture|receive|''")
    if not isinstance(c["peer_url"], str) or (c["peer_url"] and not _re.fullmatch(r"http://[A-Za-z0-9.\-]{1,64}:\d{1,5}", c["peer_url"])):
        e.append("peer_url must be http://host:port")
    if not isinstance(c["adapter"], str) or not (_re.fullmatch(r"[A-Za-z0-9 _.()\-]{0,64}", c["adapter"])
            or _re.fullmatch(r"\\Device\\NPF_(?:\{[0-9A-Fa-f\-]{36}\}|Loopback)", c["adapter"])):
        e.append("adapter: up to 64 chars of [A-Za-z0-9 _.()-]")
    if cp.get("mode") not in ("dda", "pattern"): e.append("capture.mode must be dda|pattern")
    if cp.get("pattern") not in ("counter", "checker", "rings", "noise", "motion"): e.append("capture.pattern invalid")
    for key in ("width", "height"):
        if not _is_int(cp.get(key), 2, 8192) or cp[key] % 2:
            e.append("capture.%s 必须是 2..8192 的偶数整数。" % key)
    if cp.get("transform") not in ("crop", "fit", "fill", "stretch"):
        e.append("capture.transform 必须是 crop|fit|fill|stretch。")
    if cp.get("codec") not in ("raw", "h264", "hevc"):
        e.append("capture.codec 必须是 raw|h264|hevc。")
    for key in ("codec_experimental", "encoder_hardware", "decoder_hardware"):
        if not isinstance(cp.get(key), bool): e.append("capture.%s 必须是布尔值。" % key)
    for key in ("encoder_backend", "decoder_backend"):
        value = cp.get(key)
        if not isinstance(value, str) or len(value) > 256 or (value and not (
                _re.fullmatch(r"\{[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}\}", value)
                or value.startswith("name:") and len(value) > 5 and all(ord(char) >= 32 for char in value))):
            e.append("capture.%s 必须是明确的 MFT CLSID 或 name:精确名称。" % key)
    for key, lo, hi in (("gop_frames", 1, 1000), ("decode_in_flight", 1, 8), ("max_au_bytes", 1024, 16777216)):
        if not _is_int(cp.get(key), lo, hi): e.append("capture.%s 必须是 %d..%d 的整数。" % (key, lo, hi))
    for key in ("bitrate_mbps", "max_bitrate_mbps"):
        if not _is_int(cp.get(key), 1, 10000): e.append("capture.%s 必须是 1..10000 的整数。" % key)
    if (_is_int(cp.get("bitrate_mbps"), 1, 10000) and _is_int(cp.get("max_bitrate_mbps"), 1, 10000)
            and cp["max_bitrate_mbps"] < cp["bitrate_mbps"]):
        e.append("最大码率不能低于目标码率。")
    if not _is_int(c["transport"].get("headroom_pct"), 0, 50):
        e.append("transport.headroom_pct 必须是 0..50 的整数。")
    if not _is_int(cp.get("fps"), 1, 1000): e.append("capture.fps 1..1000")
    if not _is_int(cp.get("batch"), 1, 1000): e.append("capture.batch 1..1000")
    if not _is_int(cp.get("mtu"), 1500, 9000): e.append("capture.mtu 1500..9000")
    if not _is_int(rv.get("mtu"), 1500, 9000): e.append("receive.mtu 1500..9000")
    if not isinstance(cp.get("gpu_convert"), bool): e.append("capture.gpu_convert must be bool")
    if not isinstance(cp.get("dst_mac"), str) or not _re.fullmatch(r"([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}", cp["dst_mac"]):
        e.append("capture.dst_mac must be aa:bb:cc:dd:ee:ff")
    if not isinstance(rv.get("shm"), str) or not _re.fullmatch(r"(Local|Global)\\[A-Za-z0-9_.\-]{1,64}", rv["shm"]):
        e.append("receive.shm must be Local\\<name>")
    if not isinstance(rv.get("vcam_autoregister"), bool): e.append("receive.vcam_autoregister must be bool")
    return e

def save_config(patch):
    with PRODUCT_LOCK, CONFIG_LOCK:
        c = _merge(load_config(), patch if isinstance(patch, dict) else {})
        if deployment_role():
            c["role"] = deployment_role()
            c["receive"].update(shm=CONFIG_DEFAULT["receive"]["shm"], vcam_autoregister=True)
            if c["role"] == "receive" and c["peer_url"]:
                try:
                    match = _re.fullmatch(r"http://([0-9.]+):5143", c["peer_url"])
                    if not match: raise ValueError()
                    ipaddress.IPv4Address(match[1])
                except ValueError:
                    return 400, {"error": "invalid config", "details": ["被采集机地址必须是 IPv4，端口固定为 5143。"]}
            if has_product_process():
                return 409, {"error": "请先停止采集，再修改连接配置。"}
        err = validate_config(c)
        if err: return 400, {"error": "invalid config", "details": err}
        tmp = _config_path() + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f: json.dump(c, f, indent=2, ensure_ascii=False)
        os.replace(tmp, _config_path())
        return 200, {"saved": True, "config": c}

# ---- P6-1：配置草案与链路预算；未实现的数据面组合必须显式拒绝 ----
STREAM_PRESETS = ((416, 416), (640, 640), (1280, 720), (1920, 1080), (2560, 1440))
STREAM_CACHE = {}
STREAM_CACHE_LOCK = threading.Lock()
PEER_CODEC_CAPABILITIES = {}


CODEC_FIELDS = ("codec_experimental", "encoder_backend", "decoder_backend", "encoder_hardware",
                "decoder_hardware", "gop_frames", "decode_in_flight", "max_au_bytes", "bitrate_mbps", "max_bitrate_mbps")


def stream_implementation_errors(capture):
    if capture["codec"] == "raw": return []
    errors = []
    if not capture.get("codec_experimental") or not capture.get("encoder_backend") or not capture.get("decoder_backend"):
        errors.append("编码链路为开发功能：须明确启用，并选择两端 MFT 后端；不会自动选择或回退。")
    if _is_int(capture["max_bitrate_mbps"], 2001, 10000): errors.append("当前 MF 后端最大码率参数上限为 2000 Mbps。")
    if (_is_int(capture["mtu"], 1500, 9000) and _is_int(capture["max_au_bytes"], 1024, 16777216) and
            (capture["max_au_bytes"] + capture["mtu"] - 43) // (capture["mtu"] - 42) > 65535):
        errors.append("编码访问单元上限超过冻结包头允许的分片数。")
    return errors


def codec_cli(capture, kind):
    side = "encoder" if kind == "sender" else "decoder"
    values = {"codec": capture["codec"], "codec-backend": capture[side + "_backend"],
              "codec-hardware": int(capture[side + "_hardware"]), "bitrate": capture["bitrate_mbps"] * 1000000,
              "max-bitrate": capture["max_bitrate_mbps"] * 1000000, "gop": capture["gop_frames"],
              "decode-in-flight": capture["decode_in_flight"], "max-au-bytes": capture["max_au_bytes"]}
    return [part for key, value in values.items() for part in ("--" + key, str(value))]


CODEC_CACHE = {}
CODEC_CACHE_LOCK = threading.Lock()


def native_codec_request(kind, capture=None):
    """Bounded native subprocess; enumeration/type setup is not a frame/quality acceptance test."""
    executable = exe_path(kind)
    try:
        identity = (os.path.abspath(executable), os.stat(executable).st_mtime_ns, os.stat(executable).st_size)
        args = (["--codec-backends"] if capture is None else ["--codec-check", "--width", str(capture["width"]),
                "--height", str(capture["height"]), "--fps", str(capture["fps"]), "--mtu", str(capture["mtu"]), *codec_cli(capture, kind)])
        key = (identity, tuple(args))
        with CODEC_CACHE_LOCK:
            cached = CODEC_CACHE.get(key)
            if cached and time.monotonic() - cached[0] < 10: return cached[1]
        result = subprocess.run([executable, *args], capture_output=True, text=True, encoding="utf-8", errors="replace",
                                timeout=12, cwd=ROOT, **({"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}))
        report = json.loads(result.stdout) if result.returncode == 0 else {"error": (result.stderr or result.stdout)[-2048:]}
        if not isinstance(report, dict) or report.get("revision") != 1:
            report = {"error": report.get("error", "缺少匹配的原生编码能力。") if isinstance(report, dict) else "原生编码响应无效。"}
        with CODEC_CACHE_LOCK:
            if len(CODEC_CACHE) > 32: CODEC_CACHE.clear()
            CODEC_CACHE[key] = (time.monotonic(), report)
        return report
    except Exception as error:
        return {"error": str(error)}


def codec_check_request(args):
    if not isinstance(args.get("capture"), dict): return 400, {"error": "capture 必须是对象。"}
    config = _merge(CONFIG_DEFAULT, {"capture": args["capture"]})
    errors = validate_config(config) + stream_implementation_errors(config["capture"])
    if config["capture"]["codec"] == "raw": errors.append("codec-check 只接受编码模式。")
    if errors: return 409, {"error": "；".join(errors)}
    kind = "sender" if deployment_role() == "capture" else "receiver"
    report = native_codec_request(kind, config["capture"])
    return (200 if report.get("configured") is True else 409), report


NATIVE_STREAM_CACHE = {}
NATIVE_STREAM_LOCK = threading.Lock()


def native_stream_capabilities(kind):
    """Probe the actual executable/DLL, not the Python source revision or version string."""
    exe = exe_path(kind)
    command = [exe, "--capabilities"]
    paths = [exe]
    if kind == "vcam":
        candidates = [os.path.join(BUILD_DIR, "vcam_verify", "Release", "vc-vcam-verify.exe"),
                      os.path.join(BUILD_DIR, "Release", "vc-vcam-verify.exe"), os.path.join(ROOT, "vc-vcam-verify.exe")]
        exe = next((path for path in candidates if os.path.isfile(path)), "")
        dll = _vcam_dll()
        paths = [exe, dll]
        command = [exe, "--dll", dll, "--capabilities"]
    try:
        key = (kind, tuple((os.path.abspath(path), os.stat(path).st_mtime_ns, os.stat(path).st_size) for path in paths))
        with NATIVE_STREAM_LOCK:
            if key in NATIVE_STREAM_CACHE: return dict(NATIVE_STREAM_CACHE[key])
            result = subprocess.run(command, capture_output=True, text=True, timeout=10, cwd=ROOT,
                                    **({"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}))
            caps = json.loads(result.stdout) if result.returncode == 0 else {}
            if (not isinstance(caps, dict) or caps.get("stream_revision") != 2 or
                    caps.get("shm_version") != 3 or caps.get("variable_resolution") is not True or
                    caps.get("dda_fps_limit") is not True or not isinstance(caps.get("codecs"), list) or "raw" not in caps["codecs"]):
                return {"error": "%s 缺少 P6-2 原生能力；请构建匹配版本，不会沿用旧的固定格式。" % kind}
            if len(NATIVE_STREAM_CACHE) >= 32: NATIVE_STREAM_CACHE.clear()
            NATIVE_STREAM_CACHE[key] = caps
            return dict(caps)
    except Exception as error:
        return {"error": "%s 原生能力探测失败：%s" % (kind, error)}


def stream_native_errors(kind):
    errors = []
    for target in (("receiver", "vcam") if kind == "receiver" else (kind,)):
        capability = native_stream_capabilities(target)
        if capability.get("error"): errors.append(capability["error"])
    return errors


def stream_capabilities(controller_ip=None):
    kind = "sender" if deployment_role() == "capture" else "receiver"
    errors = stream_native_errors(kind)
    result = {"revision": 2, "presets": [{"width": w, "height": h} for w, h in STREAM_PRESETS],
              "native": {"codecs": native_stream_capabilities(kind).get("codecs", []) if not errors else [], "transforms": ["crop", "fit", "fill", "stretch"],
                         "codec_revision": native_stream_capabilities(kind).get("codec_revision", 0),
                         "variable_resolution": not errors, "dda_fps_limit": not errors, "shm_version": 3,
                         "errors": errors},
              "hardware": {"encoder": "unprobed", "decoder": "unprobed"}, "adapter": None}
    if result["native"]["codec_revision"] == 1:
        result["hardware"]["encoder" if kind == "sender" else "decoder"] = native_codec_request(kind)
    if deployment_role() == "capture" and controller_ip:
        code, adapter = _choose_capture_adapter(controller_ip)
        if code == 200: result["adapter"] = adapter
        else: result["adapter_error"] = adapter.get("error", "采集网卡识别失败。")
    return result


def stream_session(capture):
    token = uuid.uuid4().hex[:10]
    return {"revision": 2, "pixel_format": "nv12", "codec": capture["codec"],
            **{key: capture[key] for key in ("width", "height", "fps", "mtu", "transform")},
            **({"codec_revision": 1, **{key: capture[key] for key in CODEC_FIELDS}} if capture["codec"] != "raw" else {}),
            "source_mac": "02:" + ":".join(token[i:i+2] for i in range(0, 10, 2))}


def validate_stream_session(session, capture):
    if not isinstance(session, dict): return ["缺少显式 P6-2 流会话；拒绝使用旧格式启动。"]
    expected = {"revision": 2, "pixel_format": "nv12", "codec": capture["codec"],
                **{key: capture[key] for key in ("width", "height", "fps", "mtu", "transform")},
                **({"codec_revision": 1, **{key: capture[key] for key in CODEC_FIELDS}} if capture["codec"] != "raw" else {})}
    if set(session) != set(expected) | {"source_mac"}: return ["流会话字段不完整或含未知字段。"]
    if any(type(session[key]) is not type(value) or session[key] != value for key, value in expected.items()):
        return ["两端流会话的尺寸/帧率/MTU/格式/变换不一致。"]
    if not _re.fullmatch(r"02(?::[0-9a-f]{2}){5}", str(session["source_mac"])): return ["流会话来源 MAC 无效。"]
    return []


def low_level_stream_errors(kind, args):
    fields = ("width", "height", "transform", "codec", "source_mac", *CODEC_FIELDS)
    if not any(key in args for key in fields): return []
    if ("width" in args) != ("height" in args): return ["必须同时提供 width 和 height。"]
    capture = _merge(CONFIG_DEFAULT["capture"], {key: args[key] for key in (*fields, "fps", "mtu") if key in args and key != "source_mac"})
    config = _merge(CONFIG_DEFAULT, {"capture": capture})
    errors = validate_config(config) + stream_implementation_errors(capture)
    if capture["codec"] == "raw" and any(key in args for key in CODEC_FIELDS): errors.append("Raw 不接受编码参数。")
    if kind == "receiver" and args.get("transform", "crop") != "crop": errors.append("接收端不能再次变换像素。")
    if "source_mac" in args and not _re.fullmatch(r"(?:[0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}", str(args["source_mac"])):
        errors.append("source_mac 无效。")
    if not errors: errors.extend(stream_native_errors(kind))
    if not errors and capture["codec"] != "raw":
        caps = native_stream_capabilities(kind)
        if caps.get("codec_revision") != 1 or capture["codec"] not in caps.get("codecs", []): errors.append("原生程序缺少编码传输能力。")
        else:
            report = native_codec_request(kind, capture)
            if report.get("configured") is not True: errors.append("所选后端不支持请求参数：" + report.get("error", "协商失败。"))
    return errors


def _stream_selected_adapter(items, selection):
    exact = [item for item in items if item.get("name") == selection]
    if len(exact) == 1: return exact[0]
    if not selection: return None
    matches = [item for item in items if selection.lower() in
               (item.get("name", "") + " " + item.get("description", "")).lower()]
    return matches[0] if len(matches) == 1 else None


def _stream_link(adapter, direction):
    if not isinstance(adapter, dict): return {"state": "unknown", "bps": None}
    status = str(adapter.get("status", "")).lower()
    if status in ("down", "disconnected", "disabled", "not present", "lowerlayerdown"):
        return {"state": "down", "bps": None, "name": adapter.get("name")}
    speed = adapter.get(direction + "_link_bps")
    # 0、缺失、字符串或虚拟回环速率都不能当作真实链路容量。
    if (adapter.get("name") == r"\Device\NPF_Loopback" or adapter.get("virtual") is True
            or adapter.get("hardware_interface") is False or not _is_int(speed, 1, 10**15)):
        return {"state": "unknown", "bps": None, "name": adapter.get("name")}
    return {"state": "up", "bps": speed, "name": adapter.get("name"), "label": adapter.get("label")}


def calculate_stream_plan(config, host_adapter=None, capture_adapter=None, discovery_warnings=(), discovery_errors=()):
    """只算预算，不改配置、不启停进程；输入需先经 validate_config。"""
    cp = config["capture"]
    fps, payload = cp["fps"], cp["mtu"] - 42
    raw_bytes = cp["width"] * cp["height"] * 3 // 2
    encoded = cp["codec"] != "raw"
    frame_bytes = (48 + (cp["max_bitrate_mbps"] * 1000000 + 8 * fps - 1) // (8 * fps)
                   if encoded else raw_bytes)
    packets = (frame_bytes + payload - 1) // payload
    last = frame_bytes - (packets - 1) * payload
    # 每包 14B 以太头 + 28B VC 头 + FCS4 + preamble/SFD8 + IFG12；尾包补足以太网最小帧。
    wire_frame_bytes = frame_bytes + packets * 66 + max(0, 18 - last)
    wire_bps = wire_frame_bytes * 8 * fps
    host = _stream_link(host_adapter, "receive")
    capture = _stream_link(capture_adapter, "transmit")
    speeds = (host["bps"], capture["bps"])
    link_bps = min(speeds) if all(speed is not None for speed in speeds) else None
    usable_bps = link_bps * (100 - config["transport"]["headroom_pct"]) // 100 if link_bps else None
    known = [speed for speed in speeds if speed is not None]
    # 只知道一端时仍能证明超限，但不能据此宣称链路可行。
    upper_bound = min(known) if known else None
    if "down" in (host["state"], capture["state"]): status = "down"
    elif upper_bound is not None and wire_bps > upper_bound: status = "exceeds"
    elif link_bps is None: status = "unknown"
    elif wire_bps > usable_bps: status = "margin"
    else: status = "fits"
    errors = stream_implementation_errors(cp) + list(discovery_errors)
    if cp["mtu"] != config["receive"]["mtu"]:
        errors.append("两端 MTU 不一致，不能用不同切包参数启动。")
    if packets > 65535:
        errors.append("单帧包数超过冻结包头的 uint16 上限。")
    warnings = list(discovery_warnings)
    if status == "unknown": warnings.append("未取得两端实际网卡速率，不能确认链路预算可行。")
    if status == "margin": warnings.append("未超过物理线速，但侵入预留余量；建议降低分辨率/帧率或使用编码模式。")
    if encoded:
        warnings.append("编码预算按最大码率均摊估算，不约束 IDR 帧瞬时突发；不代表编码器吞吐或画质已达标。")
        warnings.append("开发版编码：最多 %d 帧解码在途；HEVC 可能额外缓存一帧，静态画面末帧可能等待下次输入。" % cp["decode_in_flight"])
        warnings.append("最大码率按访问单元发送起点的一秒滑窗限制（含48B封装，不含L2开销）；单帧仍可形成微突发。")
    if cp["mode"] == "dda":
        warnings.append("P6-2 DDA 只发送新桌面图像，按目标帧率限速；静态桌面不重复发送，实际帧率可能更低。")
    warnings.append("协商速率不是实测吞吐；源桌面尺寸、编解码吞吐与端到端效果未在本次计算中验证。")
    return {"revision": 2, "video": {"width": cp["width"], "height": cp["height"], "fps": fps,
            "codec": cp["codec"], "transform": cp["transform"], "raw_frame_bytes": raw_bytes},
            "network": {"status": status, "is_estimate": encoded, "host": host, "capture": capture,
                        "payload_bytes": payload, "packets_per_frame": packets,
                        "payload_bps": frame_bytes * 8 * fps, "wire_bps": wire_bps,
                        "link_bps": link_bps, "known_upper_bound_bps": upper_bound,
                        "headroom_pct": config["transport"]["headroom_pct"], "usable_bps": usable_bps,
                        "max_raw_fps": usable_bps // (wire_frame_bytes * 8) if usable_bps and not encoded else None},
            "implementation": {"supported": not errors, "reasons": errors, "hardware_verified": False},
            "can_start": not errors and status not in ("exceeds", "down"), "warnings": warnings}


def _stream_discovery(config, refresh=False):
    key = (ROOT, config["peer_url"], config["adapter"])
    with STREAM_CACHE_LOCK:
        cached = STREAM_CACHE.get(key)
        if not refresh and cached and time.monotonic() - cached[0] < 10:
            return cached[1]
        warnings, errors = [], []
        code, adapters = get_adapters()
        host = _stream_selected_adapter(adapters.get("items", []), config["adapter"]) if code == 200 else None
        if host is None: warnings.append("未能唯一识别主机所选网卡的速率。")
        capture = None
        if config["peer_url"]:
            code, peer = peer_call("/stream/capabilities", timeout=35, url=config["peer_url"])
            native = peer.get("native", {}) if isinstance(peer, dict) else {}
            if not isinstance(native, dict): native = {}
            if isinstance(peer, dict):
                if len(PEER_CODEC_CAPABILITIES) >= 16: PEER_CODEC_CAPABILITIES.clear()
                PEER_CODEC_CAPABILITIES[(ROOT, config["peer_url"])] = peer
            if code != 200 or not isinstance(peer, dict) or peer.get("revision") != 2 or native.get("variable_resolution") is not True or native.get("dda_fps_limit") is not True:
                errors.append("采集后台或原生 sender 缺少 P6-2 能力；旧后台不能忽略新会话参数。")
            if code == 200 and isinstance(peer, dict) and peer.get("revision") in (1, 2):
                capture = peer.get("adapter")
                if peer.get("adapter_error"): warnings.append(str(peer["adapter_error"]))
            else:
                warnings.append("采集后台未提供新版速率探测接口或暂不可达，未假设对端也是 2.5G。")
        else:
            warnings.append("尚未填写采集后台地址。")
        result = (host, capture, warnings, errors)
        if len(STREAM_CACHE) >= 16: STREAM_CACHE.pop(next(iter(STREAM_CACHE)))
        STREAM_CACHE[key] = (time.monotonic(), result)
        return result


def stream_plan(patch=None, refresh=False):
    if deployment_role() == "capture": return 403, {"error": "请在主机计算双端链路预算。"}
    config = _merge(load_config(), patch if isinstance(patch, dict) else {})
    errors = validate_config(config)
    if errors: return 400, {"error": "invalid config", "details": errors}
    if config["peer_url"] and deployment_role() == "receive":
        match = _re.fullmatch(r"http://([0-9.]+):5143", config["peer_url"])
        try:
            if not match: raise ValueError()
            ipaddress.IPv4Address(match[1])
        except ValueError:
            return 400, {"error": "被采集机地址必须是 IPv4，端口固定为 5143。"}
    result = calculate_stream_plan(config, *_stream_discovery(config, refresh))
    errors = stream_native_errors("receiver")
    cp = config["capture"]
    if cp["codec"] != "raw":
        peer_caps = PEER_CODEC_CAPABILITIES.get((ROOT, config["peer_url"]), {})
        local_backends = native_codec_request("receiver")
        hardware = peer_caps.get("hardware", {})
        encoder = hardware.get("encoder", {}) if isinstance(hardware, dict) else {}
        native = peer_caps.get("native", {})
        result["backends"] = {"decoder": local_backends.get("backends", []),
                              "encoder": encoder.get("backends", []) if isinstance(encoder, dict) else []}
        if not isinstance(native, dict) or native.get("codec_revision") != 1:
            errors.append("采集后台缺少 P6-3 编码传输能力，请先使用匹配版本。")
        if not result["implementation"]["reasons"] and not errors:
            report = native_codec_request("receiver", cp)
            if report.get("configured") is not True: errors.append("主机解码器协商失败：" + report.get("error", "无效响应。"))
            code, report = peer_call("/stream/codec-check", {"capture": cp}, timeout=20, url=config["peer_url"])
            if not isinstance(report, dict): report = {"error": "无效响应。"}
            if code != 200 or report.get("configured") is not True: errors.append("采集编码器协商失败：" + report.get("error", "无效响应。"))
    if errors:
        result["implementation"]["reasons"].extend(errors)
        result["implementation"]["supported"] = result["can_start"] = False
    return 200, result


def stream_plan_request(args):
    if not isinstance(args.get("config", {}), dict) or not isinstance(args.get("refresh", False), bool):
        return 400, {"error": "请求应为 {config:{...}, refresh:true|false}。"}
    return stream_plan(args.get("config", {}), args.get("refresh", False))

# ---- vcam 注册（HKCU，与运行 DShow 客户端的用户相同） ----
def _vcam_dll():
    for p in (os.path.join(BUILD_DIR, "vc-vcam.dll"), os.path.join(BUILD_DIR, "Release", "vc-vcam.dll"),
              os.path.join(ROOT, "vc-vcam.dll")):
        if os.path.isfile(p): return p
    return ""

def vcam_status():
    dll, reg = _vcam_dll(), ""
    if os.name == "nt":
        try:
            import winreg as _wr
            with _wr.OpenKey(_wr.HKEY_CURRENT_USER, r"Software\Classes\CLSID\%s\InprocServer32" % VCAM_CLSID) as k:
                reg = _wr.QueryValueEx(k, "")[0]
        except Exception:
            reg = ""
    match = bool(dll and reg and os.path.normcase(os.path.abspath(reg)) == os.path.normcase(os.path.abspath(dll)))
    return {"dll": dll, "registered": reg, "match": match}

def vcam_register():
    dll = _vcam_dll()
    if not dll: return 404, {"error": "vc-vcam.dll not found under ROOT/build"}
    if os.name != "nt": return 400, {"error": "windows only"}
    exe = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32", "regsvr32.exe")
    try:
        r = subprocess.run([exe, "/s", dll], timeout=30)
        if r.returncode == 0:
            # 产品映射固定；清除旧诊断开关，避免默认输出测试运动帧。
            import winreg as _wr
            with _wr.OpenKey(_wr.HKEY_CURRENT_USER, r"Software\Classes\CLSID\%s" % VCAM_CLSID,
                             0, _wr.KEY_SET_VALUE) as key:
                _wr.SetValueEx(key, "FrameMap", 0, _wr.REG_SZ, load_config()["receive"]["shm"])
                _wr.SetValueEx(key, "Motion", 0, _wr.REG_DWORD, 0)
    except Exception as error:
        return 500, dict(vcam_status(), error="虚拟采集卡注册未完成：%s" % error)
    st = vcam_status(); st["exit_code"] = r.returncode
    return (200 if r.returncode == 0 and st["match"] else 500), st


def vcam_unregister():
    st = vcam_status()
    if not st["registered"]:
        state["vcam_managed"] = False
        return 200, dict(st, reclaimed=True)
    if not st["match"]:
        return 409, dict(st, error="虚拟采集卡路径不符，未回收其他目录的注册。")
    exe = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32", "regsvr32.exe")
    result = subprocess.run([exe, "/s", "/u", st["dll"]], timeout=30)
    st = vcam_status()
    ok = result.returncode == 0 and not st["registered"]
    if ok: state["vcam_managed"] = False
    return (200 if ok else 500), dict(st, reclaimed=ok, exit_code=result.returncode)


def has_product_process():
    return any(state[k].get("proc") is not None and state[k]["proc"].poll() is None
               for k in ("sender", "receiver"))

# ---- 对端调用（只用固定路径，不做通用反代） ----
def peer_call(path, body=None, timeout=3.0, url=None):
    url = url if url is not None else load_config()["peer_url"]
    if not url: return 0, {"error": "peer_url not configured"}
    data = None if body is None else json.dumps(body).encode()
    req = _ureq.Request(url + path, data=data, method="GET" if body is None else "POST",
                        headers={"Content-Type": "application/json"})
    try:
        with _ureq.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read() or b"{}")
    except _uerr.HTTPError as he:
        try: return he.code, json.loads(he.read() or b"{}")
        except Exception: return he.code, {"error": "peer http %d" % he.code}
    except Exception as ex:
        return 0, {"error": "peer unreachable: %s" % ex}

def _stop_kind(kind):
    # 同 /sender|receiver/stop：停进程并把 stats 标 running=false
    kill_slot(state[kind])
    try:
        p = state[kind].get("stats", "")
        if p and os.path.exists(p):
            with open(p, "r", encoding="utf-8") as f: d = json.load(f)
            if isinstance(d, dict):
                d["running"] = False
                with open(p + ".tmp", "w", encoding="utf-8") as f: json.dump(d, f)
                os.replace(p + ".tmp", p)
    except Exception: pass

# ---- 产品模式：本机一侧 ----
def product_local_start(args=None, controller_ip=None):
    with PRODUCT_LOCK:
        return _product_local_start(args or {}, controller_ip)


def _product_local_start(args, controller_ip):
    c = load_config(); role = c["role"]
    if role not in ("capture", "receive"): return 409, {"error": "local role not set (config.role)"}
    if role == "capture" and args:
        c = _merge(c, {"capture": args.get("capture", {})})
    errors = validate_config(c)
    if errors: return 400, {"error": "invalid capture config", "details": errors}
    errors = stream_implementation_errors(c["capture"])
    if deployment_role(): errors.extend(stream_native_errors("sender" if role == "capture" else "receiver"))
    if not errors and c["capture"]["codec"] != "raw":
        kind = "sender" if role == "capture" else "receiver"
        report = native_codec_request(kind, c["capture"])
        if report.get("configured") is not True: errors.append("所选编解码器协商失败：" + report.get("error", "无效响应。"))
    if role == "receive" and c["capture"]["mtu"] != c["receive"]["mtu"]: errors.append("两端 MTU 不一致。")
    descriptor = args.get("stream")
    if not deployment_role() and descriptor is None and any(c["capture"][key] != CONFIG_DEFAULT["capture"][key] for key in ("width", "height", "transform")):
        errors.append("可变尺寸诊断也需要显式 stream 会话，不能沿用旧默认值。")
    if descriptor is None and role == "receive" and deployment_role(): descriptor = stream_session(c["capture"])
    if deployment_role() or descriptor is not None: errors.extend(validate_stream_session(descriptor, c["capture"]))
    if errors: return 409, {"error": "；".join(errors), "details": errors}
    if role == "capture" and args:
        if args.get("auto_adapter"):
            code, adapter = _choose_capture_adapter(controller_ip)
            if code != 200: return code, adapter
            c["adapter"] = adapter["name"]
        errors = validate_config(c)
        if errors: return 400, {"error": "invalid capture config", "details": errors}
    if not c["adapter"]: return 409, {"error": "local adapter not set (config.adapter)"}
    if deployment_role() and not os.path.isfile(exe_path("receiver" if role == "receive" else "sender")):
        return 503, {"error": "缺少采集或接收程序，请先完成构建。"}
    if role == "receive":
        rv = c["receive"]; out = {"role": role, "stream": descriptor}
        if rv["vcam_autoregister"]:
            vc, out["vcam"] = vcam_register()
            if vc != 200:
                if vcam_status()["match"]:
                    state["vcam_managed"] = True
                    out["cleanup"] = vcam_unregister()[1]
                return 500, dict(out, error="vcam register failed")
            state["vcam_managed"] = True
        else:
            out["vcam"] = vcam_status()
        try:
            sid, info = launch_product("receiver", {"adapter": c["adapter"], "mtu": rv["mtu"], "shm": rv["shm"], "timeout_s": 0,
                                                     **({"width": c["capture"]["width"], "height": c["capture"]["height"], "fps": c["capture"]["fps"],
                                                         "source_mac": descriptor["source_mac"]} if descriptor else {}),
                                                     **({"codec": c["capture"]["codec"], **{key: c["capture"][key] for key in CODEC_FIELDS}}
                                                        if c["capture"]["codec"] != "raw" else {})})
        except Exception as error:
            if state["vcam_managed"]: out["cleanup"] = vcam_unregister()[1]
            return 500, dict(out, error="接收程序启动失败：%s" % error)
        out.update(session_id=sid, args=info)
        return 200, out
    cp = c["capture"]
    try:
        sid, info = launch_product("sender", {"adapter": c["adapter"], "mode": cp["mode"], "pattern": cp["pattern"],
                                             "fps": cp["fps"], "frames": 0, "mtu": cp["mtu"], "batch": cp["batch"],
                                             "dst_mac": cp["dst_mac"], "gpu_convert": cp["gpu_convert"],
                                             **({"width": cp["width"], "height": cp["height"], "transform": cp["transform"],
                                                 "source_mac": descriptor["source_mac"]} if descriptor else {}),
                                             **({"codec": cp["codec"], **{key: cp[key] for key in CODEC_FIELDS}} if cp["codec"] != "raw" else {})})
    except Exception as error:
        return 500, {"role": role, "error": str(error), "diagnostics": slot_stats("sender")}
    return 200, {"role": role, "session_id": sid, "args": info, "stream": descriptor}

def product_local_stop():
    with PRODUCT_LOCK:
        role = load_config()["role"]
        kinds = {"capture": ("sender",), "receive": ("receiver",)}.get(role, ("sender", "receiver"))
        out = {}
        for k in kinds:
            out[k] = state[k].get("session_id"); _stop_kind(k)
        result = {"role": role, "stopped": out}
        if state["vcam_managed"]:
            code, result["vcam"] = vcam_unregister()
            if code != 200: return code, dict(result, error="虚拟采集卡回收失败，请重试停止采集。")
        return 200, result

# ---- 产品模式：主机编排 ----
def _side_call(side, local_role, op, stream=None):
    if side == local_role:
        return product_local_start({"stream": stream} if stream else {}) if op == "start" else product_local_stop()
    args = {}
    if side == "capture" and op == "start" and deployment_role() == "receive":
        cp = dict(load_config()["capture"])
        # 主机负责参数；被采集机自动选择控制连接所在网口，无需另开页面。
        code, adapters = get_adapters()
        if code == 200:
            selected = [item for item in adapters.get("items", []) if item["name"] == load_config()["adapter"]]
            if selected and selected[0].get("mac"):
                cp["dst_mac"] = selected[0]["mac"].replace("-", ":").lower()
        args = {"capture": cp, "auto_adapter": True, "stream": stream}
    return peer_call("/product/local/" + op, args, timeout=45)

def product_start():
    with PRODUCT_LOCK:
        return _product_start()


def _product_start():
    c = load_config(); me = c["role"]
    if deployment_role() == "capture": return 403, {"ok": False, "error": "采集后台仅接受主机下发的采集命令。"}
    if me not in ("capture", "receive"): return 409, {"ok": False, "error": "local role not set (config.role)"}
    errors = validate_config(c)
    if errors: return 400, {"ok": False, "error": "invalid config", "details": errors}
    errors = stream_implementation_errors(c["capture"])
    if errors: return 409, {"ok": False, "error": "；".join(errors), "details": errors}
    if not deployment_role() and any(c["capture"][key] != CONFIG_DEFAULT["capture"][key] for key in ("width", "height", "transform")):
        return 409, {"ok": False, "error": "可变尺寸产品链路需要 host/capture 角色及显式会话协商。"}
    if deployment_role() == "receive":
        hc, health = peer_call("/health")
        if hc != 200 or health.get("product_revision") != 2 or health.get("product_role") != "capture" or health.get("stream_revision") != 2:
            return 409, {"ok": False, "error": "请更新被采集机后台，并使用 --role capture 在 5143 端口启动。"}
    pc, pcfg = peer_call("/config")
    if pc != 200: return 502, {"ok": False, "error": "peer config: %s" % pcfg.get("error", pc)}
    want = "receive" if me == "capture" else "capture"
    if pcfg.get("role") != want:
        return 409, {"ok": False, "error": "peer role is %r, need %r" % (pcfg.get("role"), want)}
    if deployment_role():
        vc, version = peer_call("/version")
        if vc != 200 or version.get("version") != read_version():
            return 409, {"ok": False, "error": "两端版本不一致，请更新被采集机后台。"}
    if deployment_role() == "receive":
        code, plan = stream_plan(refresh=True)
        if code != 200: return code, dict(plan, ok=False)
        if not plan["can_start"]:
            reasons = plan["implementation"]["reasons"]
            return 409, {"ok": False, "error": "；".join(reasons) if reasons else
                         "所选网卡未连接或参数超过已知物理线速，请先调整配置。", "plan": plan}
    steps = []
    descriptor = stream_session(c["capture"]) if deployment_role() else None
    code, r = _side_call("receive", me, "start", descriptor); steps.append({"side": "receive", "code": code, "result": r})
    if code != 200: return 502, {"ok": False, "steps": steps}
    time.sleep(0.5)  # 收端先就绪，避免首帧丢
    if deployment_role() == "receive" and not slot_stats("receiver")["running"]:
        sc, sr = product_local_stop(); steps.append({"side": "receive", "code": sc, "result": sr, "rollback": True})
        return 502, {"ok": False, "error": "接收程序已退出，请检查运行日志。", "steps": steps}
    code, r = _side_call("capture", me, "start", descriptor)
    if code == 200 and descriptor is not None and r.get("stream") != descriptor:
        code, r = 409, {"error": "采集后台未确认同一流会话，已拒绝运行。"}
    steps.append({"side": "capture", "code": code, "result": r})
    if code != 200:
        # 请求超时可能发生在对端已经启动之后，先尽力停止再回收本机。
        sc, sr = _side_call("capture", me, "stop"); steps.append({"side": "capture", "code": sc, "result": sr, "rollback": True})
        sc, sr = _side_call("receive", me, "stop"); steps.append({"side": "receive", "code": sc, "result": sr, "rollback": True})
        return 502, {"ok": False, "steps": steps}
    return 200, {"ok": True, "steps": steps}

def product_stop():
    with PRODUCT_LOCK:
        return _product_stop()


def _product_stop():
    me = load_config()["role"]
    if me not in ("capture", "receive"): return 409, {"ok": False, "error": "local role not set (config.role)"}
    steps = []
    for side in ("capture", "receive"):  # 先停发端
        code, r = _side_call(side, me, "stop"); steps.append({"side": side, "code": code, "result": r})
    return 200, {"ok": all(s["code"] == 200 for s in steps), "steps": steps}

def reap_vcam():
    if not state["vcam_managed"] or not PRODUCT_LOCK.acquire(blocking=False): return
    try:
        if state["vcam_managed"] and not slot_stats("receiver")["running"]:
            vcam_unregister()
    finally:
        PRODUCT_LOCK.release()


def _local_summary():
    c = load_config()
    reap_vcam()
    vc = vcam_status() if c["role"] == "receive" else None
    if vc is not None: vc["managed"] = state["vcam_managed"]
    return {"online": True, "config": c, "version": read_version(),
            "sender": slot_stats("sender"), "receiver": slot_stats("receiver"),
            "vcam": vc}

def product_status():
    out = {"ts": time.time(), "local": _local_summary(), "peer": None}
    url = out["local"]["config"]["peer_url"]
    if url:
        code, h = peer_call("/health", timeout=1.0, url=url)
        if code != 200:
            out["peer"] = {"online": False, "url": url, "error": h.get("error", code)}
        else:
            p = {"online": True, "url": url, "version": h.get("version"),
                 "product_revision": h.get("product_revision"), "product_role": h.get("product_role"), "stream_revision": h.get("stream_revision")}
            for k, path in (("config", "/config"), ("sender", "/sender/stats"), ("receiver", "/receiver/stats"), ("vcam", "/vcam/status")):
                cc, d = peer_call(path, timeout=1.5, url=url)
                p[k] = d if cc == 200 else None
            out["peer"] = p
    return out

def _p5_get(h):
    p = h.path.split("?")[0]
    if p in ("/", "/ui", "/ui/"):
        if deployment_role() == "capture" or load_config()["role"] == "capture":
            h._j({"error": "采集后台不提供控制台，请在主机打开界面。"}, 404)
            return True
        b = UI_HTML.encode("utf-8")
        h.send_response(200); h.send_header("Content-Type", "text/html; charset=utf-8")
        h.send_header("Cache-Control", "no-store"); h.send_header("Content-Length", str(len(b)))
        h.end_headers(); h.wfile.write(b); return True
    if p == "/config": h._j(load_config()); return True
    if p == "/stream/capabilities": h._j(stream_capabilities(h.client_address[0])); return True
    if p == "/vcam/status": h._j(vcam_status()); return True
    if p == "/product/status": h._j(product_status()); return True
    return False

def _p5_post(h, args):
    routes = {"/config": lambda: save_config(args),
              "/stream/plan": lambda: stream_plan_request(args),
              "/stream/codec-check": lambda: codec_check_request(args),
              "/vcam/register": vcam_register,
              "/product/start": product_start, "/product/stop": product_stop,
              "/product/local/start": lambda: product_local_start(args, h.client_address[0]),
              "/product/local/stop": product_local_stop}
    fn = routes.get(h.path.split("?")[0])
    if not fn: return False
    try:
        code, res = fn()
    except Exception as ex:
        code, res = 500, {"error": "%s: %s" % (type(ex).__name__, ex)}
    h._j(res, code); return True

UI_HTML = r'''<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="dark">
<title>VirtualCapture 控制台</title>
<style>
:root{--bg:#101214;--surface:#181b1e;--raised:#202428;--line:#2b3035;--text:#edf0f2;--muted:#949da5;--green:#b0e868;--red:#ff8d86;--amber:#eec17b;--blue:#82b1ed;--purple:#b8a0e8}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:14px/1.55 "Segoe UI","Microsoft YaHei",sans-serif}button,input,select{font:inherit}button,a,input,select,summary{-webkit-tap-highlight-color:transparent}button,a{touch-action:manipulation}button{cursor:pointer}button:disabled{opacity:.4;cursor:not-allowed}button:focus-visible,a:focus-visible,summary:focus-visible{outline:2px solid var(--green);outline-offset:4px}input:focus,select:focus{outline:2px solid #b0e86855;border-color:var(--green)}[hidden]{display:none!important}a{color:var(--text);text-decoration:none}svg{width:18px;height:18px;fill:none;stroke:currentColor;stroke-width:1.6;stroke-linecap:round;stroke-linejoin:round;flex-shrink:0}.sprite{position:absolute;width:0;height:0;overflow:hidden}
.app{display:grid;grid-template-columns:216px minmax(0,1fr);min-height:100vh}.sidebar{border-right:1px solid var(--line);padding:30px 20px;display:flex;flex-direction:column;gap:42px;position:sticky;top:0;height:100vh}.brand{display:flex;align-items:center;gap:10px;font-size:17px;font-weight:650;letter-spacing:-.5px}.brand-mark{display:grid;place-items:center;width:31px;height:31px;color:var(--green);background:#b0e86812;border:1px solid #b0e86838;border-radius:9px}.brand-mark svg{width:22px;height:22px}.brand small{display:block;font-size:10px;font-weight:500;letter-spacing:1.8px;color:var(--muted);margin-top:2px}.nav-label{font-size:11px;letter-spacing:2px;color:#737e87;padding:0 12px;margin-bottom:12px}.nav{display:grid;gap:7px}.nav button{border:0;background:none;text-align:left;color:var(--muted);padding:12px;display:flex;gap:12px;align-items:center;border-radius:7px}.nav button[aria-current="page"]{background:#b0e86812;color:var(--green)}.nav button:hover{background:var(--raised)}.sidebar-foot{margin-top:auto;font-size:11px;color:var(--muted);line-height:1.9}.sidebar-foot strong{font-weight:500;color:#c3cbd1}.sidebar-foot .dot{margin-right:7px;background:var(--green)}
.main{max-width:1500px;width:100%;padding:30px 40px 24px;margin:auto}.topbar{display:flex;justify-content:space-between;align-items:center;gap:20px;margin-bottom:29px}.eyebrow{font-size:11px;color:var(--muted);letter-spacing:.8px;margin-bottom:5px}h1{font-size:26px;line-height:1.3;letter-spacing:-.6px;margin:0;font-weight:600}.toolbar{display:flex;align-items:center;gap:13px}.stamp{font-size:11px;color:var(--muted);white-space:nowrap}.badge{display:inline-flex;align-items:center;gap:7px;font-size:11px;white-space:nowrap;border:1px solid var(--line);border-radius:20px;padding:5px 10px;background:var(--surface)}.dot{display:inline-block;width:6px;height:6px;border-radius:50%;background:currentColor;flex-shrink:0}.muted{color:var(--muted)}.good{color:var(--green)}.warn{color:var(--amber)}.bad{color:var(--red)}.mono{font-family:Consolas,"SFMono-Regular",monospace;font-variant-numeric:tabular-nums}.btn{border:1px solid var(--line);background:var(--raised);color:var(--text);border-radius:7px;padding:9px 14px;display:inline-flex;align-items:center;justify-content:center;gap:8px;white-space:nowrap;font-size:13px;font-weight:500}.btn:hover:not(:disabled){background:#2a3035;border-color:#4a535b}.btn.primary{color:#19240d;background:var(--green);border-color:var(--green)}.btn.primary:hover:not(:disabled){background:#c0f180}.btn.danger{color:var(--red);background:#ff8d8608;border-color:#ff8d862b}.btn.icon{padding:9px}.btn.quiet{background:transparent}.btn.small{font-size:11px;padding:6px 10px}.btn svg{width:15px;height:15px}.btn[aria-busy="true"] svg{animation:spin 1.2s linear infinite}@keyframes spin{to{transform:rotate(360deg)}}
.hero{display:flex;justify-content:space-between;align-items:center;gap:24px;border:1px solid var(--line);background:var(--surface);border-radius:11px;padding:22px 24px;margin-bottom:23px}.hero-copy{display:flex;gap:14px;align-items:center;min-width:0}.hero-icon{width:40px;height:40px;display:grid;place-items:center;border:1px solid currentColor;border-radius:50%;opacity:.8;flex-shrink:0}.hero h2{margin:0 0 4px;font-size:16px;font-weight:600}.hero p{margin:0;color:var(--muted);font-size:12px;overflow-wrap:anywhere}.actions{display:flex;gap:8px;align-items:center}.kpis{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:14px;margin-bottom:24px}.kpi{padding:19px 21px;border:1px solid var(--line);border-radius:10px;background:var(--surface);min-width:0}.kpi-label{font-size:12px;color:var(--muted);display:flex;align-items:center;gap:7px}.kpi-label svg{width:14px;height:14px}.kpi-value{display:flex;align-items:baseline;gap:7px;margin-top:13px;min-height:45px}.kpi-value strong{font-size:34px;line-height:1.2;letter-spacing:-1.2px;font-weight:500;font-variant-numeric:tabular-nums}.kpi-value small{font-size:11px;color:var(--muted)}.kpi-note{font-size:10px;color:#7f8992;margin-top:6px;min-height:16px}
.section-heading{display:flex;justify-content:space-between;align-items:center;margin:0 0 12px;gap:10px}.section-heading h2{font-size:13px;font-weight:500;margin:0}.section-heading span{font-size:11px;color:var(--muted)}.topology{display:grid;grid-template-columns:minmax(0,1fr) 94px minmax(0,1fr);align-items:center;margin-bottom:25px}.device{border:1px solid var(--line);border-radius:10px;background:var(--surface);padding:18px 21px;min-width:0}.device-head{display:flex;align-items:center;gap:11px;margin-bottom:17px}.device-icon{background:var(--raised);width:34px;height:34px;display:grid;place-items:center;border-radius:8px;color:#c1c9d0}.device h3{margin:0;font-size:13px;font-weight:600}.device small{font-size:10px;color:var(--muted)}.device-state{margin-left:auto;display:flex;align-items:center;gap:6px;font-size:11px;white-space:nowrap}.device dl{margin:0;display:grid;grid-template-columns:68px minmax(0,1fr);gap:8px;font-size:11px}.device dt{color:var(--muted)}.device dd{margin:0;overflow-wrap:anywhere}.device-bottom{border-top:1px solid var(--line);margin-top:14px;padding-top:11px;font-size:10px;color:var(--muted);display:flex;justify-content:space-between;gap:10px}.wire{display:grid;place-items:center;gap:7px;font-size:9px;color:var(--muted)}.wire-line{display:flex;align-items:center;width:100%;color:#69757f}.wire-line:before,.wire-line:after{content:"";height:1px;background:var(--line);flex:1}.wire-line svg{width:15px;height:15px;margin:0 4px}.wire.live .wire-line{color:var(--green)}.wire.live .wire-line:before,.wire.live .wire-line:after{background:#b0e86855}
.analysis{display:grid;grid-template-columns:minmax(0,1fr) 268px;gap:17px;margin-bottom:24px}.panel{border:1px solid var(--line);border-radius:10px;background:var(--surface);padding:20px 22px;min-width:0}.panel-head{display:flex;justify-content:space-between;align-items:center;gap:12px;margin-bottom:20px}.panel h2{font-size:13px;font-weight:500;margin:0}.panel-head p{font-size:10px;color:var(--muted);margin:3px 0 0}.legend{display:flex;gap:12px;font-size:10px;color:var(--muted);flex-wrap:wrap}.legend i{display:inline-block;width:5px;height:5px;border-radius:50%;margin-right:5px}.chart{position:relative;height:170px}canvas{display:block;width:100%;height:170px}.chart-empty{position:absolute;inset:0;display:grid;place-items:center;font-size:11px;color:var(--muted);pointer-events:none}.latency-values{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));border-top:1px solid var(--line);margin-top:14px;padding-top:15px;gap:12px}.latency-values small{font-size:10px;color:var(--muted)}.latency-values strong{display:block;font-size:19px;font-weight:500;margin:4px 0;font-variant-numeric:tabular-nums}.latency-values strong span{font-size:10px;color:var(--muted);margin-left:4px}.latency-values em{font-style:normal;font-size:9px;color:var(--muted)}.quality-row{display:flex;align-items:center;justify-content:space-between;font-size:11px;padding:11px 0;border-bottom:1px solid var(--line);gap:8px}.quality-row span{color:var(--muted)}.quality-row strong{font-weight:500;font-variant-numeric:tabular-nums}.quality-row:last-of-type{border-bottom:0}.quality-foot{font-size:10px;color:var(--muted);padding-top:12px;line-height:1.7}.events{padding-top:17px;padding-bottom:8px}.events .panel-head{margin-bottom:7px}.event-list{max-height:180px;overflow:auto}.event{display:grid;grid-template-columns:61px 6px minmax(0,1fr);gap:11px;align-items:baseline;padding:10px 0;border-top:1px solid #ffffff08;font-size:11px}.event time{color:#7f8992;font-size:10px}.event p{margin:0;overflow-wrap:anywhere}.event-list:empty:before{content:"开始、停止采集和配置结果会显示在这里。";display:block;color:var(--muted);font-size:11px;padding:15px 0 20px}.footer{display:flex;justify-content:space-between;gap:15px;font-size:10px;color:#737e87;padding-top:18px}
.config-intro{color:var(--muted);font-size:12px;margin:-7px 0 24px}.config-layout{display:grid;grid-template-columns:minmax(0,1fr) 264px;gap:22px;align-items:start}.config-card{border:1px solid var(--line);border-radius:10px;background:var(--surface);overflow:hidden}.form-section{padding:23px 25px;border-bottom:1px solid var(--line)}.form-section:last-child{border-bottom:0}.form-heading{display:flex;gap:10px;align-items:center;margin-bottom:18px}.step{font-size:10px;color:var(--green);border:1px solid #b0e86830;border-radius:5px;padding:2px 5px}.form-heading h2{font-size:13px;margin:0;font-weight:500}.field-grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:18px 19px}.field{display:grid;gap:7px;align-content:start;min-width:0}.field.full{grid-column:1/-1}.field label{font-size:12px}.field p{margin:0;font-size:10px;color:var(--muted)}input,select{width:100%;min-width:0;background:#111416;border:1px solid #363d43;color:var(--text);border-radius:6px;padding:10px 11px;font-size:12px;height:40px}input::placeholder{color:#626e78}input:user-invalid,select:user-invalid{border-color:var(--red)}select{cursor:pointer}.input-action{display:flex;gap:8px}.input-action input,.input-action select{flex:1}.field-error{color:var(--red);font-size:11px;white-space:pre-line;margin-top:15px}.form-actions{display:flex;align-items:center;justify-content:space-between;gap:15px;padding:18px 25px;background:#ffffff02}.form-actions span{font-size:11px;color:var(--muted)}.help-card{border:1px solid var(--line);border-radius:10px;padding:20px;background:var(--surface);font-size:11px;color:var(--muted)}.help-card h2{font-size:13px;font-weight:500;color:var(--text);margin:0 0 15px}.help-step{display:flex;gap:10px;margin:14px 0}.help-step b{font-size:11px;color:var(--green);font-weight:500}.help-step p{margin:0}.help-note{border-top:1px solid var(--line);padding-top:15px;margin-top:18px;font-size:10px;line-height:1.8}.adapters{font-size:11px;white-space:pre-wrap;overflow-wrap:anywhere;color:var(--muted);max-height:150px;overflow:auto;margin:0}.adapters:not(:empty){padding-top:10px}.config-extra{margin-top:19px}summary{font-size:11px;color:var(--muted);cursor:pointer;list-style-position:inside}.config-extra[open] summary{margin-bottom:17px}.note-banner{border-left:2px solid var(--green);background:#b0e86808;padding:13px 15px;font-size:11px;color:#b8c6aa;margin-top:16px}.inline-feedback{font-size:11px;white-space:pre-line;margin:13px 0 0}
@media(min-width:1600px){.main{padding-top:38px}.topbar{margin-bottom:35px}.chart,canvas{height:200px}}
@media(max-width:1150px){.main{padding:26px}.sidebar{padding:27px 15px}.app{grid-template-columns:190px minmax(0,1fr)}.analysis{grid-template-columns:minmax(0,1fr) 220px}.kpi{padding:17px}.kpi-value strong{font-size:28px}.config-layout{grid-template-columns:minmax(0,1fr)}.help-card{display:none}.hero{padding:19px}.stamp{display:none}}
@media(max-width:900px){.app{display:block}.sidebar{position:static;height:auto;display:flex;flex-direction:row;align-items:center;justify-content:space-between;padding:15px 24px;border-right:0;border-bottom:1px solid var(--line);gap:15px}.brand small,.nav-label,.sidebar-foot{display:none}.nav{display:flex}.nav button{padding:9px 12px;font-size:12px}.main{padding:25px}.analysis{grid-template-columns:minmax(0,1fr) 245px}.topbar{margin-bottom:22px}.hero{gap:15px}}
@media(max-width:650px){.sidebar{padding:14px 16px}.brand{font-size:15px}.brand-mark{width:27px;height:27px}.nav button{font-size:11px;padding:8px}.nav button svg{display:none}.main{padding:22px 16px}.topbar{gap:12px}h1{font-size:23px}.eyebrow{font-size:9px}.toolbar{gap:8px}.badge{font-size:10px;padding:4px 8px}.hero{align-items:stretch;flex-direction:column;padding:18px;gap:18px}.hero-icon{width:32px;height:32px}.hero h2{font-size:14px}.hero p{font-size:11px}.actions{justify-content:flex-end}.actions .btn{flex:1}.kpis{grid-template-columns:repeat(2,minmax(0,1fr));gap:10px}.kpi{padding:16px}.kpi-value{min-height:35px;margin-top:9px}.kpi-value strong{font-size:29px}.kpi-note{font-size:9px}.topology{grid-template-columns:minmax(0,1fr);gap:0}.wire{height:53px;gap:2px}.wire-line{width:80px}.analysis{grid-template-columns:minmax(0,1fr)}.panel{padding:18px}.panel-head{gap:9px}.legend{gap:9px;font-size:9px}.chart,canvas{height:160px}.quality-row{padding:10px 0}.field-grid{grid-template-columns:minmax(0,1fr)}.form-section{padding:21px 18px}.form-actions{padding:17px 18px;flex-wrap:wrap}.footer{flex-direction:column;gap:5px}.device{padding:18px}.latency-values strong{font-size:18px}.latency-values em{font-size:8px}}
@media(prefers-reduced-motion:reduce){*,*:before,*:after{animation:none!important;scroll-behavior:auto!important}}
</style>
</head>
<body>
<svg class="sprite" aria-hidden="true"><defs>
<symbol id="i-logo" viewBox="0 0 24 24"><path d="M8 3H3v5m13-5h5v5M3 16v5h5m13-5v5h-5"/><rect x="8" y="8" width="8" height="8" rx="1.5"/></symbol>
<symbol id="i-grid" viewBox="0 0 24 24"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></symbol>
<symbol id="i-sliders" viewBox="0 0 24 24"><path d="M4 7h8m5 0h3M4 17h3m5 0h8"/><circle cx="14" cy="7" r="3"/><circle cx="10" cy="17" r="3"/></symbol>
<symbol id="i-play" viewBox="0 0 24 24"><path d="m8 5 11 7-11 7Z"/></symbol>
<symbol id="i-stop" viewBox="0 0 24 24"><rect x="6" y="6" width="12" height="12" rx="1"/></symbol>
<symbol id="i-refresh" viewBox="0 0 24 24"><path d="M20 10a8 8 0 1 0-1 7M20 4v6h-6"/></symbol>
<symbol id="i-monitor" viewBox="0 0 24 24"><rect x="3" y="4" width="18" height="13" rx="2"/><path d="M8 21h8m-4-4v4"/></symbol>
<symbol id="i-capture" viewBox="0 0 24 24"><path d="M8 3H3v5m13-5h5v5M3 16v5h5m13-5v5h-5M8 12h8m-4-4v8"/></symbol>
<symbol id="i-arrow" viewBox="0 0 24 24"><path d="M4 12h16m-6-6 6 6-6 6"/></symbol>
<symbol id="i-check" viewBox="0 0 24 24"><path d="m5 12 4 4L19 6"/></symbol>
<symbol id="i-signal" viewBox="0 0 24 24"><path d="M4 20v-4m5 4v-8m5 8V8m5 12V4"/></symbol>
</defs></svg>
<div class="app">
<aside class="sidebar">
<a class="brand" href="#overview" aria-label="VirtualCapture 总览"><span class="brand-mark"><svg aria-hidden="true"><use href="#i-logo"/></svg></span><span>VirtualCapture<small>HOST CONSOLE</small></span></a>
<div><div class="nav-label">控制台</div><nav class="nav" aria-label="主导航"><button type="button" data-view="overview" aria-current="page"><svg aria-hidden="true"><use href="#i-grid"/></svg>链路总览</button><button type="button" data-view="config"><svg aria-hidden="true"><use href="#i-sliders"/></svg>连接配置<span id="dirtyDot" class="dot warn" hidden></span></button></nav></div>
<div class="sidebar-foot"><div><span class="dot"></span>原始像素，直接传输</div><strong>NV12 · 640 × 640 · 240 fps</strong><br>VirtualCapture <span id="appVersion">—</span></div>
</aside>
<main class="main">
<header class="topbar"><div><div class="eyebrow">采集与传输 / 主机控制台</div><h1 id="pageTitle">链路总览</h1></div><div class="toolbar"><span id="lastUpdate" class="stamp">等待连接</span><span id="connectionBadge" class="badge muted"><i class="dot"></i><span>连接中</span></span><button id="bRefresh" class="btn quiet icon" title="刷新连接状态" aria-label="刷新连接状态"><svg aria-hidden="true"><use href="#i-refresh"/></svg></button></div></header>
<section id="overviewView" aria-label="链路总览">
<div class="hero"><div class="hero-copy"><span id="heroIcon" class="hero-icon muted"><svg aria-hidden="true"><use href="#i-signal"/></svg></span><div><h2 id="stateTitle">正在连接控制服务</h2><p id="stateDetail">读取两端状态，请稍候。</p></div></div><div class="actions"><button class="btn quiet" data-view="config"><svg aria-hidden="true"><use href="#i-sliders"/></svg>配置</button><button id="bCapture" class="btn primary" disabled><svg aria-hidden="true"><use href="#i-play"/></svg><span>开始采集</span></button></div></div>
<div class="kpis">
<article class="kpi"><div class="kpi-label"><svg aria-hidden="true"><use href="#i-monitor"/></svg>接收帧率</div><div class="kpi-value"><strong id="rxFps">—</strong><small>fps</small></div><div id="rxFpsNote" class="kpi-note">等待接收完整帧</div></article>
<article class="kpi"><div class="kpi-label"><svg aria-hidden="true"><use href="#i-capture"/></svg>采集帧率</div><div class="kpi-value"><strong id="txFps">—</strong><small>fps</small></div><div id="txFpsNote" class="kpi-note">等待采集会话</div></article>
<article class="kpi"><div class="kpi-label"><svg aria-hidden="true"><use href="#i-arrow"/></svg>链路带宽</div><div class="kpi-value"><strong id="bandwidth">—</strong><small>Gbps</small></div><div class="kpi-note">未压缩 NV12 · raw L2</div></article>
<article class="kpi"><div class="kpi-label"><svg aria-hidden="true"><use href="#i-check"/></svg>已接收帧</div><div class="kpi-value"><strong id="complete">—</strong><small>帧</small></div><div id="frameNote" class="kpi-note">当前接收会话累计</div></article>
</div>
<div class="section-heading"><h2>设备与链路</h2><span>采集 → 接收 → 虚拟采集卡</span></div>
<div class="topology">
<article id="capCard" class="device"><div class="device-head"><span class="device-icon"><svg aria-hidden="true"><use href="#i-capture"/></svg></span><div><h3>被采集机</h3><small data-device="location">发端</small></div><span class="device-state muted" data-device="state"><i class="dot"></i><span>等待配置</span></span></div><dl><dt>控制地址</dt><dd class="mono" data-device="url">—</dd><dt>网络适配器</dt><dd data-device="adapter">—</dd><dt>采集方式</dt><dd data-device="output">桌面采集 · NV12</dd></dl><div class="device-bottom"><span data-device="session">尚无会话</span><span data-device="version">—</span></div></article>
<div id="wire" class="wire"><div class="wire-line"><svg aria-hidden="true"><use href="#i-arrow"/></svg></div><span>Raw L2</span><span>0x88B5</span></div>
<article id="rxCard" class="device"><div class="device-head"><span class="device-icon"><svg aria-hidden="true"><use href="#i-monitor"/></svg></span><div><h3>主机</h3><small data-device="location">收端</small></div><span class="device-state muted" data-device="state"><i class="dot"></i><span>等待配置</span></span></div><dl><dt>控制地址</dt><dd class="mono" data-device="url">—</dd><dt>网络适配器</dt><dd data-device="adapter">—</dd><dt>虚拟采集卡</dt><dd data-device="output">等待读取注册状态</dd></dl><div class="device-bottom"><span data-device="session">尚无会话</span><span data-device="version">—</span></div></article>
</div>
<div class="analysis">
<section class="panel"><div class="panel-head"><div><h2>分段延迟</h2><p>p99 · 最近 60 秒 · 毫秒</p></div><div class="legend"><span><i style="background:var(--blue)"></i>发送</span><span><i style="background:var(--green)"></i>重组</span><span><i style="background:var(--purple)"></i>虚拟卡</span></div></div><div class="chart"><canvas id="cv" aria-label="最近60秒各段p99延迟，详细数值见下方" role="img"></canvas><div id="chartEmpty" class="chart-empty">采集开始后显示延迟曲线</div></div><div class="latency-values"><div><small>发送段 p99</small><strong><b id="txLatency" style="font-weight:inherit">—</b><span>ms</span></strong><em id="txLatencyRange">平均 — · 最大 —</em></div><div><small>重组段 p99</small><strong><b id="rxLatency" style="font-weight:inherit">—</b><span>ms</span></strong><em id="rxLatencyRange">平均 — · 最大 —</em></div><div><small>虚拟卡交付 p99</small><strong><b id="vcLatency" style="font-weight:inherit">—</b><span>ms</span></strong><em id="vcLatencyRange">平均 — · 最大 —</em></div></div></section>
<section class="panel"><div class="panel-head"><h2>传输质量</h2><span id="qualityBadge" class="badge muted">等待数据</span></div><div class="quality-row"><span>丢失帧</span><strong id="dropped">—</strong></div><div class="quality-row"><span id="hashErrorLabel">Raw 像素哈希错误</span><strong id="hashErrors">—</strong></div><div class="quality-row"><span>丢失包</span><strong id="packetLoss">—</strong></div><div class="quality-row"><span>虚拟卡交付</span><strong id="vcDelivered">—</strong></div><div class="quality-row"><span>重复帧</span><strong id="vcRepeats">—</strong></div><div class="quality-foot" id="decodeInfo">Raw 无编解码</div><div class="quality-foot" id="qualityNote">Raw 每帧像素 FNV-1a64 对账；缺失数据以 — 显示。</div></section>
</div>
<section class="panel events"><div class="panel-head"><h2>操作记录 <span id="eventCount" class="muted"></span></h2><button id="bClear" class="btn quiet small">清空显示</button></div><div id="events" class="event-list" role="log" aria-label="操作记录"></div></section>
</section>
<section id="configView" aria-label="连接配置" hidden>
<div class="config-intro">在主机填写连接信息，被采集机只需运行后台服务。</div>
<div class="config-layout"><form id="configForm" class="config-card" novalidate>
<div class="form-section"><div class="form-heading"><span class="step">01</span><h2>连接被采集机</h2></div><div class="field-grid">
<div class="field full"><label for="c_peer_ip">被采集机 IP</label><input id="c_peer_ip" required inputmode="decimal" autocomplete="off" spellcheck="false" placeholder="192.168.2.59"><p>只填写 IPv4 地址，控制端口固定为 5143。</p></div>
<div class="field full"><label for="c_adapter">本机网络适配器</label><div class="input-action"><select id="c_adapter" required><option value="">正在获取网络适配器…</option></select><button type="button" id="bAdapters" class="btn">刷新网卡</button></div><p id="adapterResult" class="inline-feedback" aria-live="polite">自动获取网卡后，选择连接被采集机的网口。</p></div>
</div></div>
<div class="form-section"><div class="form-heading"><span class="step">02</span><h2>采集设置</h2></div>
<div class="field-grid">
<div class="field full"><label for="c_resolution">输出分辨率预设</label><select id="c_resolution"><option value="416x416">416 × 416 · 方形</option><option value="640x640">640 × 640 · 默认</option><option value="1280x720">720p · 1280 × 720</option><option value="1920x1080">1080p · 1920 × 1080</option><option value="2560x1440">1440p · 2560 × 1440</option><option value="custom">自定义宽高</option></select><p>这是传输及虚拟卡的目标 NV12 尺寸，不会修改桌面显示分辨率。Raw 已支持这些尺寸；编码组合须通过所选后端协商。</p></div>
<div class="field"><label for="c_width">输出宽度</label><input id="c_width" type="number" min="2" max="8192" step="2" required><p>2–8192 的偶数；直接输入可切换为自定义。</p></div>
<div class="field"><label for="c_height">输出高度</label><input id="c_height" type="number" min="2" max="8192" step="2" required><p>NV12 色度采样要求宽高均为偶数。</p></div>
<div class="field"><label for="c_fps">发送帧率上限</label><input id="c_fps" type="number" min="1" max="1000" step="1" required><p>严格限制发送间隔，不追赶补发；DDA 只发新桌面画面，实际帧率可能更低。</p></div>
<div class="field"><label for="c_transform">裁切 / 缩放方式</label><select id="c_transform"><option value="crop">居中裁切 · 1:1 像素</option><option value="fit">完整画面 · 等比缩放 + 黑边</option><option value="fill">铺满输出 · 等比缩放 + 裁切</option><option value="stretch">拉伸填满 · 不保持比例</option></select><p>居中裁切不缩放；其他方式使用确定性的整数最近邻采样。裁切输出大于源桌面时拒绝启动。</p></div>
</div></div>
<div class="form-section"><div class="form-heading"><span class="step">03</span><h2>传输模式与预算</h2></div>
<div class="field-grid">
<div class="field full"><label for="c_codec">链路像素 / 编码模式</label><select id="c_codec"><option value="raw">Raw NV12 · 无压缩</option><option value="h264">H.264 · 有损编码 / 开发版</option><option value="hevc">H.265 / HEVC · 有损编码 / 开发版</option></select><p>编码只用于两机链路，主机解码后仍提供 NV12；不使用有损前后像素相等来判定传输正确。</p></div>
<div id="bitrateFields" class="field full" hidden><div class="field-grid">
<div class="field full"><label for="c_experimental">编码功能授权</label><select id="c_experimental"><option value="0">仅保存草案，不启动编码</option><option value="1">明确启用开发版编码（未完成双机 / 长稳 / 画质验收）</option></select></div>
<div class="field"><label for="c_encoder">采集机编码后端</label><input id="c_encoder" list="encoderBackends" maxlength="256" placeholder="明确选择 CLSID 或 name:精确名称" required><datalist id="encoderBackends"></datalist></div>
<div class="field"><label for="c_encoder_hw">编码器枚举类别</label><select id="c_encoder_hw"><option value="1">硬件 MFT（失败不回退）</option><option value="0">系统 / 软件 MFT（显式选择）</option></select></div>
<div class="field"><label for="c_decoder">主机解码后端</label><input id="c_decoder" list="decoderBackends" maxlength="256" placeholder="明确选择 CLSID 或 name:精确名称" required><datalist id="decoderBackends"></datalist></div>
<div class="field"><label for="c_decoder_hw">解码器枚举类别</label><select id="c_decoder_hw"><option value="0">系统 / 软件 MFT（显式选择）</option><option value="1">硬件 MFT（失败不回退）</option></select></div>
<div class="field"><label for="c_bitrate">目标码率 · Mbps</label><input id="c_bitrate" type="number" min="1" max="2000" step="1" required></div>
<div class="field"><label for="c_max_bitrate">最大码率 · Mbps</label><input id="c_max_bitrate" type="number" min="1" max="2000" step="1" required></div>
<div class="field"><label for="c_gop">强制 IDR 间隔 · 帧</label><input id="c_gop" type="number" min="1" max="1000" step="1" required></div>
<div class="field"><label for="c_in_flight">解码在途帧上限</label><input id="c_in_flight" type="number" min="1" max="8" step="1" required></div>
</div><p>候选不是兼容性保证；计算预算时实际协商媒体类型，启动后仍须观察帧到达。最大码率按发送起点的一秒滑窗限制，IDR 仍可能微突发。HEVC 本机后端需要额外缓存一帧，静态末帧可能等待下次输入；不静默增加队列。</p></div>
<div class="field"><label for="c_headroom">链路预留余量 · %</label><input id="c_headroom" type="number" min="0" max="50" step="1" required><p>仅预算建议，不是修改历史性能验收门限。</p></div>
</div>
<div class="note-banner" id="streamPlan" role="status" aria-live="polite"><strong id="planTitle">等待计算链路预算</strong><p id="planSummary" class="inline-feedback">配置读取后自动计算，不保存草稿，也不启停采集。</p><p id="planDetails" class="inline-feedback"></p><p id="planWarnings" class="inline-feedback"></p></div>
<button type="button" id="bPlan" class="btn small" style="margin-top:14px">重新检测两端链路</button>
<div id="configError" class="field-error" role="alert" hidden></div></div>
<div class="form-actions"><span id="saveState" role="status">配置读取中…</span><div class="actions"><button type="button" id="bReset" class="btn quiet" disabled>撤销修改</button><button type="submit" id="bSave" class="btn primary" disabled><svg aria-hidden="true"><use href="#i-check"/></svg>保存配置</button></div></div>
</form><aside><div class="help-card"><h2>配置不等于能力已达标</h2><div class="help-step"><b>1</b><p>先选尺寸、画面处理与传输模式，再看两端链路预算。</p></div><div class="help-step"><b>2</b><p>预算、原生实现和实机验收分开显示，不会自动降低清晰度或切换编码。</p></div><div class="help-step"><b>3</b><p>Raw 已支持可变尺寸和四种变换。编码须明确启用开发功能、选择两端后端，协商失败直接拒绝。</p></div><div class="help-note">开始接收自动注册虚拟采集卡，停止后回收。正在运行时不允许修改配置。</div></div><div class="note-banner">稳定版保持不动<br>H.264 / H.265 开发链路已接通；双机、长稳、画质与实际吞吐仍须单独验收。</div></aside></div>
</section>
<footer class="footer"><span>分段延迟在各自机器内测量，不代表跨机端到端延迟。</span><span id="footerStatus">自动刷新 · 约每秒一次</span></footer>
</main></div>
<script>
'use strict';
const $ = id => document.getElementById(id);
const history = [];
let lastStatus = null, previous = null, savedConfig = null, dirty = false, busy = false;
let polling = false, pollTimer = null, eventCount = 0, scanBusy = false, configGeneration = 0, busyAction = '';
let planTimer = null, planGeneration = 0, planInFlight = false, planPending = false, planRefresh = false;
const finite = value => typeof value === 'number' && Number.isFinite(value);
const fmt = (value, digits = 2) => finite(value) ? value.toFixed(digits) : '—';
const count = value => finite(value) ? value.toLocaleString('zh-CN') : '—';
const clone = value => JSON.parse(JSON.stringify(value));

function frameRate(current, old, key, seconds) {
  if (!current || !old || !current.running || !old.running || !finite(seconds) || seconds <= 0 || seconds > 10) return null;
  if (current.session_id !== old.session_id || current.uptime_s < old.uptime_s) return null;
  if (!finite(current[key]) || !finite(old[key]) || current[key] < old[key]) return null;
  return (current[key] - old[key]) / seconds;
}
function getSides(status) {
  const local = status && status.local, peer = status && status.peer;
  return {capture:peer || (local ? {online:false,url:local.config && local.config.peer_url} : null), receive:local};
}
function activeStats(machine, kind) {
  const stats = machine && machine[kind];
  return machine && machine.online !== false && stats && stats.running && stats.note !== 'stub-no-binary' ? stats : {};
}
function hasRunning(status) {
  return [status && status.local,status && status.peer].some(machine => machine &&
    (machine.sender && machine.sender.running || machine.receiver && machine.receiver.running));
}
function pendingPipeline(capture = {}) {
  if ((capture.codec || 'raw') !== 'raw' && (!capture.codec_experimental || !capture.encoder_backend || !capture.decoder_backend))
    return '编码为开发功能，须明确启用并选择两端后端；无自动回退。';
  return '';
}
function readiness(status, unsaved) {
  const local = status && status.local, peer = status && status.peer;
  const config = local && local.config || {};
  const sides = getSides(status);
  const tx = sides.capture && sides.capture.sender || {}, rx = sides.receive && sides.receive.receiver || {};
  const canStop = !!(local && local.online !== false && (hasRunning(status) || local.vcam && local.vcam.managed));
  const result = (level,title,detail,canStart=false) => ({level,title,detail,canStart,canStop});
  if (!local || local.online === false) return result('bad','本机控制服务未连接','请检查 agent 是否运行，服务恢复后会自动重连。');
  if (unsaved) return result('warn','配置尚未保存','请在连接配置中保存修改后，再开始新的采集。');
  if (!config.peer_url || !config.adapter) return result('warn','连接被采集机','请在连接配置中填写被采集机 IP，并选择本机网卡。');
  if (!peer || !peer.online) return result('warn','等待采集后台上线','请确认被采集机后台已启动，IP 正确，5143 端口可达。');
  if (!peer.config || !peer.sender || !peer.receiver) return result('warn','对端状态读取不完整','检查对端 agent 是否支持当前控制台，稍后重试。');
  if (peer.product_revision !== 2 || peer.product_role !== 'capture' || peer.stream_revision !== 2) return result('warn','请更新被采集机后台','需要支持当前主机控制流程的采集后台，控制端口为 5143。');
  if (peer.config.role !== 'capture') return result('warn','对端未以采集后台运行','请在被采集机使用 --role capture 启动后台服务。');
  if (local.version && peer.version && local.version !== peer.version) return result('warn','两端版本不一致','本机 '+local.version+'，对端 '+peer.version+'。请统一版本后开始采集。');
  if (tx.note === 'stub-no-binary' && tx.running || rx.note === 'stub-no-binary' && rx.running) return result('warn','占位进程运行中 · 没有真实传输','缺少采集或接收程序，请先完成构建并停止占位进程。');
  if (tx.running && rx.running) {
    if (rx.waiting_idr) return result('warn','等待下一个 IDR 恢复画面','解码状态已重置；依赖帧不会直接发布，等待带参数集的完整 IDR。');
    if (qualityErrors(rx) > 0) return result('warn','链路运行中 · 请检查传输质量','发现丢帧、字节校验或解码异常，详细计数见传输质量。');
    if (!finite(rx.complete) || rx.complete <= 0) return result('warn','等待首个完整画面','两端进程已启动，尚未收到完整帧；请检查采集源与网卡链路。');
    return result('good','采集链路已启动','接收机将完整画面交给 VirtualCapture；实时帧率在下方更新。');
  }
  if (tx.error || rx.error) return result('bad','原生采集链路异常',tx.error || rx.error,!canStop && !pendingPipeline(config.capture));
  if (tx.running || rx.running) return result('warn','链路仅有一端在运行','请先停止采集，再重新开始。');
  if (local.vcam && local.vcam.managed) return result('warn','虚拟采集卡等待回收','点击停止采集重试回收。');
  const pending = pendingPipeline(config.capture);
  if (pending) return result('warn','所选配置尚不可运行',pending);
  return result('good','两端已连接，可以开始采集','主机自动注册虚拟采集卡并开始接收，再通知对端推送画面。',true);
}
function friendlyError(message) {
  const text = String(message || '未知错误');
  const exact = {
    'Failed to fetch':'无法连接控制服务，请检查 agent 和网络。',
    'local role not set (config.role)':'请先设置本机角色。',
    'local adapter not set (config.adapter)':'请先选择本机网卡。',
    'peer_url not configured':'请先填写被采集机 IP。',
    'invalid config':'配置校验未通过。',
    'vc-vcam.dll not found under ROOT/build':'未找到虚拟采集卡程序，请先完成构建。',
    'vcam register failed':'虚拟采集卡注册失败，请检查程序路径和当前用户。',
    'role must be capture|receive|\'\'':'本机角色应为采集机或接收机。',
    'peer_url must be http://host:port':'被采集机地址格式错误，请只填写 IPv4 地址。',
    'adapter: up to 64 chars of [A-Za-z0-9 _.()-]':'网卡字段最多64字符，可填设备名或英文描述关键字。',
    'capture.dst_mac must be aa:bb:cc:dd:ee:ff':'目标 MAC 应为 aa:bb:cc:dd:ee:ff 格式。',
    'receive.shm must be Local\\<name>':'共享内存名称应为 Local\\名称 或 Global\\名称。'
  };
  if (exact[text]) return exact[text];
  if (/peer role/.test(text)) return '请在被采集机使用 --role capture 启动后台服务。';
  if (/peer unreachable|peer config/.test(text)) return '无法连接采集后台，请检查 IP、5143 端口和后台进程。';
  if (/capture.fps/.test(text)) return '目标帧率必须为 1–1000 的整数。';
  if (/capture.batch/.test(text)) return '批大小必须为 1–1000 的整数。';
  if (/(capture|receive).mtu/.test(text)) return 'MTU 必须为 1500–9000 的整数。';
  return text;
}
function qualityErrors(rx) {
  return (rx.dropped || 0)+(rx.packets_lost || 0)+(rx.hash_mismatch || 0)+(rx.transport_errors || 0)+
    (rx.decode_errors || 0)+(rx.decode_queue_drops || 0)+(rx.decode_discarded || 0)+(rx.decode_resets || 0);
}
async function api(path, body, timeout = 10000) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeout);
  try {
    const response = await fetch(path, {signal:controller.signal, cache:'no-store',
      ...(body === undefined ? {} : {method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)})});
    const data = await response.json();
    return [response.status,data];
  } catch (error) {
    throw new Error(error.name === 'AbortError' ? '请求超时，请检查连接后重试。' : friendlyError(error.message));
  } finally { clearTimeout(timer); }
}
function text(id, value) { $(id).textContent = value; }
function log(message, level = 'muted') {
  const row = document.createElement('div'); row.className = 'event '+level;
  const time = document.createElement('time'); time.textContent = new Date().toLocaleTimeString('zh-CN',{hour12:false});
  const dot = document.createElement('i'); dot.className = 'dot';
  const content = document.createElement('p'); content.textContent = message;
  row.append(time,dot,content); $('events').prepend(row);
  while ($('events').children.length > 80) $('events').lastElementChild.remove();
  text('eventCount','· '+(++eventCount));
}
function showView(view) {
  view = view === 'config' ? 'config' : 'overview';
  $('overviewView').hidden = view !== 'overview'; $('configView').hidden = view !== 'config';
  text('pageTitle',view === 'overview' ? '链路总览' : '连接配置');
  document.querySelectorAll('.nav [data-view]').forEach(button => {
    if (button.dataset.view === view) button.setAttribute('aria-current','page');
    else button.removeAttribute('aria-current');
  });
  if (location.hash !== '#'+view) window.history.replaceState(null,'','#'+view);
  if (view === 'overview') draw();
}
function syncControls() {
  const state = readiness(lastStatus,dirty);
  text('stateTitle',state.title); text('stateDetail',state.detail);
  $('heroIcon').className = 'hero-icon '+state.level;
  const stopping = state.canStop;
  $('bCapture').disabled = busy || !(stopping || state.canStart);
  $('bCapture').className = 'btn '+(stopping ? 'danger' : 'primary');
  $('bCapture').title = state.detail;
  $('bCapture').querySelector('use').setAttribute('href',stopping ? '#i-stop' : '#i-play');
  $('bCapture').lastElementChild.textContent = busyAction || (stopping ? '停止采集' : '开始采集');
  $('bSave').disabled = busy || !savedConfig || !dirty;
  $('bReset').disabled = busy || !dirty;
  $('dirtyDot').hidden = !dirty;
  text('saveState',!savedConfig ? '配置尚未读取' : dirty ? '有未保存的修改' : '配置已同步');
}
function captureSource(stats, config) {
  const source = stats.mode ? stats : config.capture || {};
  return source.mode === 'pattern' ? '测试图案 · '+source.pattern : '桌面采集 · NV12';
}
function renderMachine(id, machine, kind, local) {
  const card = $(id), set = (key,value) => { card.querySelector('[data-device="'+key+'"]').textContent = value; };
  const online = !!machine && machine.online !== false;
  const stats = machine && machine[kind] || {}, config = machine && machine.config || {};
  let label = !machine ? '等待配置' : !online ? '离线' : stats.note === 'stub-no-binary' && stats.running ? '占位进程' : stats.running ? '运行中' : '已停止';
  const level = !online ? 'muted' : stats.note === 'stub-no-binary' ? 'warn' : stats.running ? 'good' : 'muted';
  const state = card.querySelector('[data-device="state"]'); state.className = 'device-state '+level; state.lastElementChild.textContent = label;
  set('location',machine && machine === local ? '本机' : machine ? '对端' : kind === 'sender' ? '发端' : '收端');
  set('url',machine ? machine === local ? location.origin : machine.url || local && local.config.peer_url || '—' : '—');
  set('adapter',stats.adapter || config.adapter || (kind === 'sender' ? '按控制连接自动选择' : '未选择'));
  set('version',online && machine.version ? 'v'+machine.version : '—');
  set('session',online && stats.session_id ? stats.session_id+' · '+fmt(stats.uptime_s,0)+'s' : '尚无活动会话');
  if (kind === 'sender') set('output',!online ? '等待连接' : captureSource(stats,config));
  else set('output',!online ? '等待连接' : !machine.vcam ? '未读取到注册状态' : machine.vcam.match ? 'VirtualCapture · 已注册' : machine.vcam.registered ? '注册路径不符' : '接收时自动注册');
}
function render(status) {
  const sides = getSides(status), tx = activeStats(sides.capture,'sender'), rx = activeStats(sides.receive,'receiver');
  const dt = previous ? status.ts-previous.ts : 0;
  const txFps = frameRate(tx,previous && previous.tx,'sent_frames',dt);
  const rxFps = frameRate(rx,previous && previous.rx,'complete',dt);
  const signature = [tx.session_id,rx.session_id].join('|');
  if (previous && previous.signature !== signature) history.length = 0;
  previous = {ts:status.ts,tx,rx,signature};
  lastStatus = status;
  if (status.local && status.local.online !== false && status.local.config && (!savedConfig || !dirty)) {
    if (JSON.stringify(savedConfig) !== JSON.stringify(status.local.config)) fillConfig(status.local.config);
  }
  renderMachine('capCard',sides.capture,'sender',status.local);
  renderMachine('rxCard',sides.receive,'receiver',status.local);
  text('txFps',fmt(txFps,0)); text('rxFps',fmt(rxFps,0));
  text('bandwidth',fmt(finite(tx.bps) ? tx.bps/1e9 : finite(rx.bps) ? rx.bps/1e9 : null,2));
  text('complete',count(rx.complete));
  text('txFpsNote',tx.running ? txFps === null ? '正在建立采样基线' : '当前采集会话 · 每秒更新' : '等待采集会话');
  text('rxFpsNote',rx.running ? rxFps === null ? '正在建立采样基线' : rxFps > 0 ? '正在接收完整帧' : '暂未收到新的完整帧' : '等待接收完整帧');
  text('frameNote',rx.session_id || '当前接收会话累计');
  const encoded = rx.codec && rx.codec !== 'raw', integrityKey = encoded ? 'transport_errors' : 'hash_mismatch';
  text('hashErrorLabel',encoded ? '压缩字节校验错误' : 'Raw 像素哈希错误');
  text('qualityNote',encoded ? '压缩访问单元 FNV-1a64 校验；解码后另记像素哈希，不做有损前后像素相等判定。' : 'Raw 每帧像素 FNV-1a64 对账；缺失数据以 — 显示。');
  text('decodeInfo',encoded ? (rx.waiting_idr ? '等待 IDR' : '在途 '+count(rx.decode_pending))+
    ' · 重置 '+count(rx.decode_resets)+' · 丢弃 '+count(rx.decode_discarded)+' · 错误 '+count(rx.decode_errors) : 'Raw 无编解码');
  [['dropped','dropped'],['hashErrors',integrityKey],['packetLoss','packets_lost'],['vcDelivered','vcam_delivered'],['vcRepeats','vcam_repeats']].forEach(pair => {
    text(pair[0],count(rx[pair[1]]));
    $(pair[0]).className = ['dropped','hashErrors','packetLoss'].includes(pair[0]) && rx[pair[1]] > 0 ? 'warn' : '';
  });
  const hasQuality = finite(rx.complete) && rx.complete > 0 && finite(rx.dropped) && finite(rx[integrityKey]);
  const errors = qualityErrors(rx);
  $('qualityBadge').className = 'badge '+(errors ? 'warn' : !hasQuality ? 'muted' : 'good');
  text('qualityBadge',errors ? '需检查' : !hasQuality ? '等待数据' : encoded ? '压缩字节校验正常' : '哈希对账正常');
  [['tx',tx,'pc1_seg',2],['rx',rx,'rseg',2],['vc',rx,'vseg',3]].forEach(([id,stats,key,digits]) => {
    text(id+'Latency',fmt(stats[key+'_p99_ms'],digits));
    text(id+'LatencyRange','平均 '+fmt(stats[key+'_avg_ms'],digits)+' · 最大 '+fmt(stats[key+'_max_ms'],digits));
  });
  $('wire').className = 'wire'+(rxFps > 0 ? ' live' : '');
  history.push({time:Date.now()/1000,values:[tx.pc1_seg_p99_ms,rx.rseg_p99_ms,rx.vseg_p99_ms]});
  draw();
  const online = !!status.local && status.local.online !== false;
  $('connectionBadge').className = 'badge '+(!online ? 'bad' : status.peer && status.peer.online ? 'good' : 'muted');
  $('connectionBadge').lastElementChild.textContent = !online ? '本机离线' : status.peer && status.peer.online ? '两端在线' : '本机在线';
  text('lastUpdate',online ? '更新于 '+new Date().toLocaleTimeString('zh-CN',{hour12:false}) : '等待重新连接');
  text('appVersion',status.local && status.local.version || '—');
  text('footerStatus',online ? '自动刷新 · 约每秒一次' : '连接中断 · 自动重试');
  syncControls();
}
function draw() {
  const canvas = $('cv'), width = canvas.clientWidth, height = canvas.clientHeight;
  if (!width || !height) return;
  const now = Date.now()/1000;
  while (history.length && history[0].time < now-60) history.shift();
  const dpr = Math.min(window.devicePixelRatio || 1,2);
  canvas.width = Math.round(width*dpr); canvas.height = Math.round(height*dpr);
  const g = canvas.getContext('2d'); g.setTransform(dpr,0,0,dpr,0,0);
  const left = 28, right = width-6, top = 10, bottom = height-22;
  let max = 1, samples = 0;
  history.forEach(sample => sample.values.forEach(value => { if (finite(value) && value >= 0) { max = Math.max(max,value); samples++; } }));
  max = Math.ceil(max*1.15*10)/10;
  g.font = '9px Segoe UI'; g.textBaseline = 'middle';
  for (let i=0;i<4;i++) {
    const y = top+(bottom-top)*i/3;
    g.strokeStyle = '#2b3035'; g.lineWidth = .6; g.beginPath(); g.moveTo(left,y); g.lineTo(right,y); g.stroke();
    g.fillStyle = '#77838d'; g.fillText((max*(1-i/3)).toFixed(max < 3 ? 1 : 0),0,y);
  }
  g.fillText('60 秒前',left,height-6); g.textAlign = 'right'; g.fillText('现在',right,height-6); g.textAlign = 'left';
  ['#82b1ed','#b0e868','#b8a0e8'].forEach((color,index) => {
    g.strokeStyle = color; g.lineWidth = 1.6; g.lineJoin = 'round'; g.beginPath();
    let started = false;
    history.forEach(sample => {
      const value = sample.values[index];
      if (!finite(value) || value < 0) { started = false; return; }
      const x = left+(right-left)*(sample.time-now+60)/60, y = bottom-(bottom-top)*value/max;
      if (started) g.lineTo(x,y); else { g.moveTo(x,y); g.arc(x,y,1.1,0,Math.PI*2); }
      started = true;
    });
    g.stroke();
  });
  $('chartEmpty').hidden = samples > 0;
}
function peerUrl(ip) {
  ip = ip.trim();
  if (!/^(?:\d{1,3}\.){3}\d{1,3}$/.test(ip) || ip.split('.').some(part => Number(part)>255 || String(Number(part))!==part)) return null;
  return 'http://'+ip+':5143';
}
function readConfig() {
  if (!savedConfig) return null;
  // MTU、批大小等未显示的参数沿用后台配置。
  const config = clone(savedConfig);
  config.peer_url = peerUrl($('c_peer_ip').value) || '';
  config.adapter = $('c_adapter').value;
  config.capture.fps = Number($('c_fps').value);
  config.capture.width = Number($('c_width').value);
  config.capture.height = Number($('c_height').value);
  config.capture.transform = $('c_transform').value;
  config.capture.codec = $('c_codec').value;
  if (config.capture.codec !== 'raw') {
    config.capture.bitrate_mbps = Number($('c_bitrate').value);
    config.capture.max_bitrate_mbps = Number($('c_max_bitrate').value);
    config.capture.codec_experimental = $('c_experimental').value === '1';
    config.capture.encoder_backend = $('c_encoder').value.trim();
    config.capture.decoder_backend = $('c_decoder').value.trim();
    config.capture.encoder_hardware = $('c_encoder_hw').value === '1';
    config.capture.decoder_hardware = $('c_decoder_hw').value === '1';
    config.capture.gop_frames = Number($('c_gop').value);
    config.capture.decode_in_flight = Number($('c_in_flight').value);
  }
  config.transport.headroom_pct = Number($('c_headroom').value);
  return config;
}
function fillConfig(config) {
  savedConfig = clone(config); dirty = false;
  const match = /^http:\/\/([0-9.]+):\d+$/.exec(config.peer_url || '');
  const cp = config.capture;
  const values = {c_peer_ip:match ? match[1] : '',c_fps:cp.fps,c_width:cp.width,c_height:cp.height,
    c_transform:cp.transform,c_codec:cp.codec,c_bitrate:cp.bitrate_mbps,c_max_bitrate:cp.max_bitrate_mbps,
    c_headroom:config.transport.headroom_pct,c_experimental:cp.codec_experimental ? '1' : '0',
    c_encoder:cp.encoder_backend || '',c_decoder:cp.decoder_backend || '',c_encoder_hw:cp.encoder_hardware === false ? '0' : '1',
    c_decoder_hw:cp.decoder_hardware ? '1' : '0',c_gop:cp.gop_frames || 60,c_in_flight:cp.decode_in_flight || 2};
  Object.entries(values).forEach(([id,value]) => { $(id).value = value; });
  const preset = cp.width+'x'+cp.height;
  $('c_resolution').value = [...$('c_resolution').options].some(option => option.value === preset) ? preset : 'custom';
  syncCodecFields();
  if (![...$('c_adapter').options].some(option => option.value === config.adapter)) {
    const option = document.createElement('option'); option.value = config.adapter;
    option.textContent = config.adapter ? '已保存 · '+config.adapter : '请选择网络适配器'; $('c_adapter').append(option);
  }
  $('c_adapter').value = config.adapter;
  $('configError').hidden = true;
  schedulePlan();
}
function markDirty(event) {
  const id = event && event.target && event.target.id;
  if (id === 'c_resolution' && $('c_resolution').value !== 'custom') {
    const [width,height] = $('c_resolution').value.split('x');
    $('c_width').value = width; $('c_height').value = height;
  } else if (id === 'c_width' || id === 'c_height') $('c_resolution').value = 'custom';
  syncCodecFields();
  dirty = !!savedConfig && (JSON.stringify(readConfig()) !== JSON.stringify(savedConfig) || !peerUrl($('c_peer_ip').value));
  $('configError').hidden = true; syncControls();
  schedulePlan();
}
function syncCodecFields() {
  const raw = $('c_codec').value === 'raw';
  $('bitrateFields').hidden = raw;
  ['c_bitrate','c_max_bitrate','c_experimental','c_encoder','c_decoder','c_encoder_hw','c_decoder_hw','c_gop','c_in_flight']
    .forEach(id => { $(id).disabled = raw; });
  ['c_encoder','c_decoder'].forEach(id => { $(id).required = !raw && $('c_experimental').value === '1'; });
}
function schedulePlan(refresh = false) {
  planGeneration++; planRefresh = planRefresh || refresh;
  clearTimeout(planTimer);
  text('planTitle','配置已变化，等待重新计算');
  text('planSummary','计算不保存草稿、不启停进程，也不会自动修改分辨率、帧率或编码。');
  text('planDetails',''); text('planWarnings','');
  planTimer = setTimeout(updateStreamPlan,refresh ? 0 : 600);
}
function renderStreamPlan(plan) {
  const network = plan.network, implementation = plan.implementation;
  const labels = {fits:'带宽预算可容纳',margin:'带宽余量不足',exceeds:'超过已知物理线速',unknown:'链路预算尚不能确认',down:'网卡未连接'};
  const speed = value => finite(value) && value > 0 ? (value/1e9).toFixed(3)+' Gbps' : '未知';
  text('planTitle',labels[network.status]+(implementation.supported ? ' · 已有原生路径' : ' · 功能待接通'));
  $('planTitle').className = !plan.can_start ? 'warn' : network.status === 'fits' ? 'good' : 'muted';
  text('planSummary','采集端发送 '+speed(network.capture.bps)+' → 主机接收 '+speed(network.host.bps)+
    '\n'+(network.is_estimate ? '编码均摊估算' : 'NV12 线速需求')+' '+speed(network.wire_bps)+
    '（含 L2 / VC 包头、FCS、前导码、帧间隙）\n预留 '+network.headroom_pct+'% 后预算 '+speed(network.usable_bps)+
    (network.max_raw_fps !== null ? ' · 此尺寸理论上限 '+network.max_raw_fps+' fps' : ''));
  text('planDetails',(implementation.reasons.length ? implementation.reasons.join('\n') : '原生路径与配置检查通过；不等于该参数组合的双机 / 长稳 / 画质验收。'));
  text('planWarnings',plan.warnings.join('\n'));
  if (plan.backends) {
    for (const side of ['encoder','decoder']) {
      const list = $(side+'Backends'); list.replaceChildren();
      for (const item of plan.backends[side] || []) {
        if (item.codec !== $('c_codec').value) continue;
        const option = document.createElement('option'); option.value = item.identity;
        option.label = item.name+' · '+(item.hardware ? '硬件' : '系统/软件')+' · 候选未验收'; list.append(option);
      }
    }
  }
}
async function updateStreamPlan() {
  if (planInFlight) { planPending = true; return; }
  if (!savedConfig) return;
  if ($('c_peer_ip').value.trim() && !peerUrl($('c_peer_ip').value)) {
    text('planTitle','请先填写有效的被采集机 IPv4 地址'); return;
  }
  const generation = planGeneration, config = readConfig(), refresh = planRefresh;
  planRefresh = false; planInFlight = true; $('bPlan').disabled = true;
  text('planTitle','正在检测两端网卡并计算预算…');
  try {
    const [code,plan] = await api('/stream/plan',{config,refresh},75000);
    if (generation !== planGeneration) return;
    if (code !== 200) throw new Error((plan.details || [plan.error || '预算计算失败']).map(friendlyError).join('\n'));
    renderStreamPlan(plan);
  } catch (error) {
    if (generation === planGeneration) { text('planTitle','预算暂不可用'); text('planSummary',error.message); }
  } finally {
    planInFlight = false; $('bPlan').disabled = false;
    if (planPending) { planPending = false; updateStreamPlan(); }
  }
}
async function save(event) {
  event.preventDefault();
  if (busy || !savedConfig) return;
  if (!$('configForm').reportValidity()) return;
  if (!peerUrl($('c_peer_ip').value)) {
    text('configError','请只填写有效的 IPv4 地址，例如 192.168.2.59。'); $('configError').hidden = false;
    return;
  }
  if (hasRunning(lastStatus)) {
    text('configError','请先停止采集，再修改连接配置。'); $('configError').hidden = false;
    return;
  }
  busy = true; syncControls(); text('saveState','正在保存…');
  try {
    const [code,result] = await api('/config',readConfig());
    if (code !== 200) throw new Error((result.details || [result.error || '保存失败']).map(friendlyError).join('\n'));
    fillConfig(result.config);
    configGeneration++;
    if (lastStatus && lastStatus.local) lastStatus.local.config = clone(result.config);
    log('本机配置已保存。正在刷新两端连接状态。','good');
  } catch (error) {
    text('configError',error.message); $('configError').hidden = false;
    log('保存失败：'+error.message,'bad');
  } finally { busy = false; syncControls(); }
  poll();
}
async function action(path, name) {
  if (busy) return;
  busy = true; busyAction = name+'中…'; syncControls();
  const button = $('bCapture'); button.setAttribute('aria-busy','true');
  log(name+'请求已发出，请稍候。');
  try {
    const [code,result] = await api(path,{},100000);
    const steps = (result.steps || []).map(step => (step.side === 'capture' ? '被采集机' : '主机')+
      (step.rollback ? '回滚' : '')+'：'+(step.code === 200 ? '完成' : friendlyError(step.result && step.result.error || '操作失败')));
    const stub = (result.steps || []).some(step => step.result && step.result.args && step.result.args.warning);
    const ok = code === 200 && result.ok === true;
    log((stub ? '已启动占位进程，没有真实传输。请构建发收程序。' : name+(ok ? '完成。' : '失败。'))+
      (result.error ? ' '+friendlyError(result.error) : '')+(steps.length ? ' '+steps.join('；') : ''),stub || !ok ? 'warn' : 'good');
  } catch (error) { log(name+'失败：'+error.message,'bad'); }
  finally {
    busyAction = ''; button.removeAttribute('aria-busy');
    // 操作后先刷新状态，避免短暂恢复成错误方向的按钮。
    if (polling) await new Promise(resolve => { const check = () => polling ? setTimeout(check,50) : resolve(); check(); });
    await poll(); busy = false; syncControls();
  }
}
async function detectAdapters() {
  if (scanBusy) return;
  scanBusy = true; $('bAdapters').disabled = true; text('adapterResult','正在检测本机网卡…');
  try {
    const [code,result] = await api('/adapters',undefined,35000);
    if (code !== 200) throw new Error(result.error && /no sender\/receiver binary/.test(result.error) ? '缺少发收程序，请先构建后再检测网卡。' : friendlyError(result.error));
    const selected = $('c_adapter').value;
    $('c_adapter').replaceChildren();
    const placeholder = document.createElement('option'); placeholder.value = ''; placeholder.textContent = '请选择网络适配器'; $('c_adapter').append(placeholder);
    const items = result.items || (result.names || []).map(name => ({name,description:name}));
    items.forEach(item => { const option = document.createElement('option'); option.value = item.name;
      option.textContent = (item.label ? item.label+' · ' : '')+item.description+(item.status ? ' · '+item.status : '');
      $('c_adapter').append(option); });
    if (selected && !items.some(item => item.name === selected)) {
      const option = document.createElement('option'); option.value = selected; option.textContent = '已保存 · '+selected; $('c_adapter').append(option);
    }
    $('c_adapter').value = selected;
    text('adapterResult',items.length ? '已自动获取 '+items.length+' 个网络适配器，请选择连接被采集机的网口。' : '没有检测到可用网卡，请检查 Npcap。');
  } catch (error) { text('adapterResult','检测失败：'+error.message); }
  finally { scanBusy = false; $('bAdapters').disabled = false; if (savedConfig) schedulePlan(); }
}
async function poll() {
  if (polling) return;
  clearTimeout(pollTimer); polling = true; $('bRefresh').disabled = true;
  const generation = configGeneration;
  try {
    const [code,status] = await api('/product/status');
    if (code !== 200 || !status.local || !status.local.config) throw new Error('无法读取控制台状态');
    if (generation !== configGeneration && savedConfig) status.local.config = clone(savedConfig);
    render(status);
  } catch (error) {
    const wasOnline = !!lastStatus && lastStatus.local.online !== false;
    const config = savedConfig || {role:'',peer_url:''};
    render({ts:Date.now()/1000,local:{online:false,config,version:lastStatus && lastStatus.local.version},peer:null});
    previous = null;
    if (wasOnline) log('连接中断：'+error.message,'warn');
  } finally {
    polling = false; $('bRefresh').disabled = false; pollTimer = setTimeout(poll,1000);
  }
}
function init() {
  document.querySelectorAll('[data-view]').forEach(button => button.addEventListener('click',() => showView(button.dataset.view)));
  document.querySelector('.brand').addEventListener('click',event => { event.preventDefault(); showView('overview'); });
  $('configForm').addEventListener('input',markDirty); $('configForm').addEventListener('change',markDirty);
  $('configForm').addEventListener('submit',save);
  $('bReset').onclick = () => { fillConfig(savedConfig); syncControls(); };
  $('bCapture').onclick = () => { const stopping = readiness(lastStatus,dirty).canStop;
    action(stopping ? '/product/stop' : '/product/start',stopping ? '停止采集' : '开始采集'); };
  $('bAdapters').onclick = detectAdapters; $('bRefresh').onclick = poll;
  $('bPlan').onclick = () => schedulePlan(true);
  $('bClear').onclick = () => { $('events').replaceChildren(); eventCount = 0; text('eventCount',''); };
  if (typeof ResizeObserver !== 'undefined') new ResizeObserver(draw).observe($('cv'));
  window.addEventListener('hashchange',() => showView(location.hash.slice(1)));
  showView(location.hash.slice(1)); poll(); detectAdapters();
}
if (typeof document !== 'undefined') init();
</script>
</body>
</html>
'''

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class H(BaseHTTPRequestHandler):
    def _j(self, o, code=200):
        b = json.dumps(o).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        if _p5_get(self): return
        if self.path == "/health":
            anyrun = any((state[k].get("proc") is not None and state[k]["proc"].poll() is None) for k in ("sender", "receiver"))
            self._j({"ok": True, "version": read_version(), "role": state["role"], "running": anyrun, "updater": updater_status(), "pid": os.getpid(), "staged": STAGED.get("version"),
                     "product_revision": 2, "stream_revision": 2,
                     "product_role": deployment_role() or load_config()["role"], "root": os.path.abspath(ROOT)})
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
        if _p5_post(self, args if isinstance(args, dict) else {}): return
        if self.path == "/sender/start":
            _a = (args if isinstance(args, dict) else {})
            errors = low_level_stream_errors("sender", _a)
            if errors: self._j({"error": "；".join(errors)}, 409)
            elif not str(_a.get("adapter", "")).strip():
                _code, _ad = get_adapters()
                self._j({"error": "missing --adapter; GET /adapters first", "adapters": _ad}, 400)
            else:
                sid, info = launch("sender", _a)
                self._j({"session_id": sid, "args": info})
        elif self.path == "/receiver/start":
            _a = (args if isinstance(args, dict) else {})
            errors = low_level_stream_errors("receiver", _a)
            if errors: self._j({"error": "；".join(errors)}, 409)
            elif not str(_a.get("adapter", "")).strip():
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
    ap = argparse.ArgumentParser()
    ap.add_argument("--role", default="host", help="host: 主机控制台（默认）；capture: 被采集机后台；兼容 sender/receiver 别名")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT, help="产品默认 5143；显式端口用于诊断/隔离测试")
    ap.add_argument("--root", default=""); a = ap.parse_args()
    if a.root: ROOT = a.root; RUN_DIR = os.path.join(ROOT, "run"); BUILD_DIR = os.path.join(ROOT, "build"); VERSION_FILE = os.path.join(ROOT, "version.json")
    state["role"] = a.role
    state["port"] = a.port
    print("[agent] role=%s port=%d version=%s root=%s" % (a.role, a.port, read_version(), ROOT), flush=True)
    ThreadingHTTPServer.daemon_threads = True
    _gc_versioned_layout()
    ThreadingHTTPServer.allow_reuse_address = True
    stop_reaper = threading.Event()
    def _watch_receiver():
        while not stop_reaper.wait(1):
            try: reap_vcam()
            except Exception: pass  # 回收失败保留 managed，下一次重试
    threading.Thread(target=_watch_receiver, daemon=True).start()
    server = ThreadingHTTPServer(("0.0.0.0", a.port), H)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        stop_reaper.set()
        product_local_stop()
        server.server_close()








