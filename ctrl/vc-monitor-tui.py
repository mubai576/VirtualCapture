#!/usr/bin/env python3
"""
vc-monitor-tui.py — VirtualCapture 控制台监控 + 启停控制
用法:
  python ctrl/vc-monitor-tui.py
  python ctrl/vc-monitor-tui.py --pc1 http://192.168.2.59:18080 --pc2 http://127.0.0.1:18080
快捷键: [S]tart  sto[P]  [Q]uit  [R]eset stats  [V]ideo window
"""
import argparse, json, os, sys, time, threading, subprocess
import urllib.request, urllib.error
from datetime import datetime
from runtime_paths import find_binary

try:
    from rich.console import Console
    from rich.layout import Layout
    from rich.panel import Panel
    from rich.table import Table
    from rich.text import Text
    from rich.live import Live
    from rich.columns import Columns
    from rich import box
    from rich.prompt import Prompt
except ImportError:
    print("Missing dependency: pip install rich"); sys.exit(1)

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUN_DIR = os.path.join(ROOT, "run")
MONITOR_EXE = find_binary("vc-monitor.exe", ROOT)

# ── HTTP helpers ──────────────────────────────────────────────────────────────
def http_get(url, timeout=3):
    try:
        r = urllib.request.urlopen(url, timeout=timeout)
        return json.loads(r.read().decode("utf-8", "replace"))
    except Exception:
        return None

def http_post(url, body, timeout=5):
    try:
        req = urllib.request.Request(url, data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"}, method="POST")
        r = urllib.request.urlopen(req, timeout=timeout)
        return json.loads(r.read().decode("utf-8", "replace"))
    except Exception as e:
        return {"error": str(e)}

def exec_ps1(agent_url, script, timeout_s=8):
    return http_post(f"{agent_url}/exec",
        {"argv": ["pwsh", "-NoProfile", "-ExecutionPolicy", "Bypass",
                  "-File", f"scripts/{script}"], "timeout_s": timeout_s},
        timeout=timeout_s + 5)

# ── State ─────────────────────────────────────────────────────────────────────
class State:
    def __init__(self, pc1_url, pc2_url):
        self.pc1_url = pc1_url
        self.pc2_url = pc2_url
        self.lock = threading.Lock()
        # live data
        self.rx_stats: dict = {}
        self.tx_stats: dict = {}
        self.pc1_health: dict = {}
        self.pc2_health: dict = {}
        self.rx_sid: str = ""
        self.tx_sid: str = ""
        self.log: list[str] = []
        self.monitor_proc = None
        self._last_refresh = 0.0
        # history for sparkline (rseg_p99)
        self.rseg_history: list[float] = []

    def add_log(self, msg: str):
        ts = datetime.now().strftime("%H:%M:%S")
        with self.lock:
            self.log.append(f"[grey50]{ts}[/grey50] {msg}")
            if len(self.log) > 50:
                self.log.pop(0)

    def refresh(self):
        """Poll agents and stats files."""
        now = time.time()
        if now - self._last_refresh < 0.4:
            return
        self._last_refresh = now

        # agent health
        h1 = http_get(f"{self.pc1_url}/health")
        h2 = http_get(f"{self.pc2_url}/health")
        # live stats from agent
        rx = http_get(f"{self.pc2_url}/receiver/stats")
        tx = http_get(f"{self.pc1_url}/sender/stats")

        with self.lock:
            self.pc1_health = h1 or {}
            self.pc2_health = h2 or {}
            self.rx_stats   = (rx or {}).get("stats", {})
            self.tx_stats   = (tx or {}).get("stats", {})
            # fallback: read latest stats files directly
            if not self.rx_stats:
                self.rx_stats = self._read_latest_stats("receiver")
            if not self.tx_stats:
                self.tx_stats = self._read_latest_stats("sender")
            # rseg history
            p99 = self.rx_stats.get("rseg_p99_ms", 0)
            if p99:
                self.rseg_history.append(float(p99))
                if len(self.rseg_history) > 30:
                    self.rseg_history.pop(0)

    def _read_latest_stats(self, kind):
        """Read the most recently modified stats file."""
        try:
            files = [f for f in os.listdir(RUN_DIR)
                     if f.endswith(f".{kind}.stats.json")]
            if not files:
                return {}
            latest = max(files, key=lambda f:
                os.path.getmtime(os.path.join(RUN_DIR, f)))
            with open(os.path.join(RUN_DIR, latest), encoding="utf-8") as fh:
                return json.load(fh)
        except Exception:
            return {}

# ── Rendering ─────────────────────────────────────────────────────────────────
SPARK = " ▁▂▃▄▅▆▇█"

def sparkline(values, width=20, max_val=None):
    if not values:
        return " " * width
    m = max_val or (max(values) or 1)
    recent = values[-width:]
    chars = []
    for v in recent:
        idx = min(8, int(v / m * 8))
        chars.append(SPARK[idx])
    return "".join(chars).rjust(width)

def color_ms(v, warn=8.0, crit=12.0):
    if v == 0:   return f"[dim]-[/dim]"
    if v < warn: return f"[green]{v:.2f}[/green]"
    if v < crit: return f"[yellow]{v:.2f}[/yellow]"
    return f"[red]{v:.2f}[/red]"

def color_count(v, bad_threshold=0):
    if v > bad_threshold: return f"[red]{v}[/red]"
    return f"[green]{v}[/green]"

def agent_status(health: dict, url: str) -> Text:
    if health and health.get("ok"):
        ver  = health.get("version", "?")
        role = health.get("role", "")
        run  = " ● stream" if health.get("running") else ""
        return Text(f"● {url}  v{ver}  {role}{run}", style="green")
    return Text(f"○ {url}  unreachable", style="red")

def make_layout(state: State) -> Layout:
    with state.lock:
        rx = state.rx_stats
        tx = state.tx_stats
        h1 = state.pc1_health
        h2 = state.pc2_health
        logs = list(state.log[-12:])
        hist = list(state.rseg_history)

    layout = Layout()
    layout.split_column(
        Layout(name="header", size=3),
        Layout(name="agents", size=3),
        Layout(name="main"),
        Layout(name="log",   size=14),
        Layout(name="footer", size=3),
    )
    layout["main"].split_row(
        Layout(name="rx_panel"),
        Layout(name="tx_panel"),
        Layout(name="spark_panel"),
    )

    # ── header ────────────────────────────────────────────────────────────────
    ts = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    header = Panel(
        Text(f"  VirtualCapture Monitor   {ts}", style="bold cyan"),
        style="cyan", box=box.HORIZONTALS)
    layout["header"].update(header)

    # ── agent status ──────────────────────────────────────────────────────────
    ag_table = Table.grid(padding=(0,4))
    ag_table.add_column(width=50)
    ag_table.add_column(width=50)
    ag_table.add_row(
        agent_status(h1, state.pc1_url),
        agent_status(h2, state.pc2_url),
    )
    layout["agents"].update(Panel(ag_table, title="Agents", style="blue"))

    # ── receiver stats ────────────────────────────────────────────────────────
    rx_t = Table(box=box.SIMPLE, show_header=True, header_style="bold",
                 expand=True)
    rx_t.add_column("Metric", style="dim", width=22)
    rx_t.add_column("Value", justify="right")
    running = rx.get("running", False)
    rx_t.add_row("Status",
        "[green]● RUNNING[/green]" if running else "[yellow]○ stopped[/yellow]")
    rx_t.add_row("complete",   f"[cyan]{rx.get('complete', 0)}[/cyan]")
    rx_t.add_row("shm_published", str(rx.get("shm_published", 0)))
    rx_t.add_row("dropped",    color_count(rx.get("dropped", 0)))
    rx_t.add_row("hash_mismatch", color_count(rx.get("hash_mismatch", 0)))
    rx_t.add_row("gaps",       str(rx.get("gaps", 0)))
    rx_t.add_row("rseg_avg",   color_ms(rx.get("rseg_avg_ms", 0), 5, 8))
    rx_t.add_row("rseg_p50",   color_ms(rx.get("rseg_p50_ms", 0), 5, 8))
    rx_t.add_row("rseg_p99",   color_ms(rx.get("rseg_p99_ms", 0), 8, 12))
    rx_t.add_row("rseg_max",   color_ms(rx.get("rseg_max_ms", 0), 50, 200))
    rx_t.add_row("pps",        f"{rx.get('pps', 0):.0f}")
    rx_t.add_row("bps",        f"{rx.get('bps', 0)/1e9:.3f} Gbps")
    rx_t.add_row("uptime",     f"{rx.get('uptime_s', 0):.0f}s")
    layout["rx_panel"].update(Panel(rx_t, title="[bold]PC2 Receiver[/bold]", style="green"))

    # ── sender stats ──────────────────────────────────────────────────────────
    tx_t = Table(box=box.SIMPLE, show_header=True, header_style="bold",
                 expand=True)
    tx_t.add_column("Metric", style="dim", width=22)
    tx_t.add_column("Value", justify="right")
    tx_run = tx.get("running", False)
    tx_t.add_row("Status",
        "[green]● RUNNING[/green]" if tx_run else "[yellow]○ stopped[/yellow]")
    tx_t.add_row("fps_actual",
        f"[cyan]{tx.get('fps_actual', 0):.1f}[/cyan] / {tx.get('fps_target', 240)}")
    tx_t.add_row("sent_frames",    str(tx.get("sent_frames", 0)))
    tx_t.add_row("send_errors",    color_count(tx.get("send_errors", 0)))
    tx_t.add_row("pc1_seg_avg",    color_ms(tx.get("pc1_seg_avg_ms", 0), 3, 5))
    tx_t.add_row("pc1_seg_p99",    color_ms(tx.get("pc1_seg_p99_ms", 0), 4, 6))
    tx_t.add_row("pc1_seg_max",    color_ms(tx.get("pc1_seg_max_ms", 0), 10, 20))
    tx_t.add_row("gpu_dispatch",   f"{tx.get('gpu_dispatch_ms', 0):.3f} ms")
    tx_t.add_row("gpu_ms",         f"{tx.get('gpu_ms', 0):.2f} ms")
    tx_t.add_row("gpu_convert",
        "[green]GPU[/green]" if tx.get("gpu_convert") else "[dim]CPU[/dim]")
    tx_t.add_row("pps",            f"{tx.get('pps', 0):.0f}")
    tx_t.add_row("bps",            f"{tx.get('bps', 0)/1e9:.3f} Gbps")
    tx_t.add_row("uptime",         f"{tx.get('uptime_s', 0):.0f}s")
    layout["tx_panel"].update(Panel(tx_t, title="[bold]PC1 Sender[/bold]", style="yellow"))

    # ── sparkline + derived metrics ───────────────────────────────────────────
    sp_t = Table(box=box.SIMPLE, show_header=False, expand=True)
    sp_t.add_column(width=30)
    sp_t.add_column()

    full_p99 = tx.get("pc1_seg_p99_ms", 0) + rx.get("rseg_p99_ms", 0)
    full_color = color_ms(full_p99, 10, 15)
    sp_t.add_row("full-path p99", f"{full_color} ms")
    sp_t.add_row("", "")

    # sparkline for rseg_p99
    sp = sparkline(hist, width=25, max_val=20.0)
    sp_t.add_row("rseg_p99 trend", f"[cyan]{sp}[/cyan]")
    if hist:
        sp_t.add_row("  min/avg/max",
            f"[dim]{min(hist):.1f}/{sum(hist)/len(hist):.1f}/{max(hist):.1f} ms[/dim]")
    sp_t.add_row("", "")

    # link utilization
    bps = rx.get("bps") or tx.get("bps") or 0
    util = bps / 2.5e9 * 100
    bar_len = int(util / 5)
    bar = "█" * bar_len + "░" * (20 - bar_len)
    bar_color = "green" if util < 70 else ("yellow" if util < 90 else "red")
    sp_t.add_row("link util",
        f"[{bar_color}]{bar}[/{bar_color}] {util:.1f}%")
    sp_t.add_row("", "")
    sp_t.add_row("session rx", f"[dim]{state.rx_sid or '-'}[/dim]")
    sp_t.add_row("session tx", f"[dim]{state.tx_sid or '-'}[/dim]")
    sp_t.add_row("", "")
    sp_t.add_row("[bold]Keys[/bold]", "")
    sp_t.add_row("  [S] Start stream", "")
    sp_t.add_row("  [P] stoP stream",  "")
    sp_t.add_row("  [V] Video window", "")
    sp_t.add_row("  [Q] Quit",         "")
    layout["spark_panel"].update(Panel(sp_t, title="[bold]Summary[/bold]"))

    # ── log ───────────────────────────────────────────────────────────────────
    log_text = Text()
    for line in logs:
        log_text.append_text(Text.from_markup(line + "\n"))
    layout["log"].update(Panel(log_text, title="Log", style="dim"))

    # ── footer ────────────────────────────────────────────────────────────────
    footer = Panel(
        Text("  [S] Start   [P] Stop   [V] Video   [Q] Quit   [R] Reset stats",
             style="bold white on dark_blue"),
        style="blue", box=box.HORIZONTALS)
    layout["footer"].update(footer)

    return layout

# ── Control actions ───────────────────────────────────────────────────────────
def do_start(state: State, batch=69, gpu_convert=True):
    """Start PC2 receiver + PC1 sender."""
    state.add_log("Starting receiver on PC2...")
    session = f"tui_{int(time.time())}"
    rx = http_post(f"{state.pc2_url}/receiver/start", {
        "adapter": "I226", "ethertype": "0x88B5", "mtu": 9000,
        "session": session, "timeout_s": 3600,
        "shm": r"Local\VirtualCaptureVcamFrame"
    })
    if "error" in (rx or {}):
        state.add_log(f"[red]receiver/start error: {(rx or {}).get('error')}[/red]")
        return
    with state.lock:
        state.rx_sid = (rx or {}).get("session_id", "")
    state.add_log(f"[green]receiver started: {state.rx_sid}[/green]")

    time.sleep(0.3)
    state.add_log("Starting sender on PC1...")
    tx = http_post(f"{state.pc1_url}/sender/start", {
        "adapter": "Realtek", "mode": "dda", "batch": batch,
        "dst_mac": "34:5a:60:4f:f2:33", "session": session,
        **({"gpu_convert": True} if gpu_convert else {}),
    })
    if "error" in (tx or {}):
        state.add_log(f"[red]sender/start error: {(tx or {}).get('error')}[/red]")
        return
    with state.lock:
        state.tx_sid = (tx or {}).get("session_id", "")
    state.add_log(f"[green]sender started: {state.tx_sid}[/green]")

def do_stop(state: State):
    """Stop sender (kill_sender.ps1) + stop receiver."""
    state.add_log("Stopping sender (kill_sender.ps1)...")
    r = exec_ps1(state.pc1_url, "kill_sender.ps1", 10)
    state.add_log(f"kill: {(r or {}).get('stdout','?')[:80].strip()}")

    if state.rx_sid:
        state.add_log(f"Stopping receiver {state.rx_sid}...")
        http_post(f"{state.pc2_url}/receiver/stop", {"session_id": state.rx_sid})
        state.add_log("[yellow]receiver stopped[/yellow]")
        with state.lock:
            state.rx_sid = ""
            state.tx_sid = ""

def do_open_video(state: State):
    """Launch vc-monitor.exe preview window."""
    if not os.path.exists(MONITOR_EXE):
        state.add_log(f"[red]Not found: {MONITOR_EXE}[/red]")
        return
    if state.monitor_proc and state.monitor_proc.poll() is None:
        state.add_log("[yellow]Video window already running[/yellow]")
        return
    state.monitor_proc = subprocess.Popen([MONITOR_EXE])
    state.add_log(f"[green]Launched video window (pid {state.monitor_proc.pid})[/green]")

# ── Main TUI loop ─────────────────────────────────────────────────────────────
def run_tui(state: State):
    console = Console()

    # background polling thread
    def poll():
        while True:
            state.refresh()
            time.sleep(0.4)
    t = threading.Thread(target=poll, daemon=True)
    t.start()

    # keyboard input (non-blocking via msvcrt on Windows)
    import msvcrt

    state.add_log("TUI started.  Press [S] to start stream, [Q] to quit.")

    with Live(make_layout(state), console=console, refresh_per_second=4,
              screen=True) as live:
        while True:
            live.update(make_layout(state))
            if msvcrt.kbhit():
                ch = msvcrt.getwch().upper()
                if ch == 'Q':
                    state.add_log("Quit requested.")
                    break
                elif ch == 'S':
                    threading.Thread(target=do_start, args=(state,),
                                     daemon=True).start()
                elif ch == 'P':
                    threading.Thread(target=do_stop, args=(state,),
                                     daemon=True).start()
                elif ch == 'V':
                    threading.Thread(target=do_open_video, args=(state,),
                                     daemon=True).start()
                elif ch == 'R':
                    with state.lock:
                        state.rseg_history.clear()
                    state.add_log("Stats history cleared.")
            time.sleep(0.1)

# ── Entry point ───────────────────────────────────────────────────────────────
def main():
    p = argparse.ArgumentParser(description="VirtualCapture TUI monitor")
    p.add_argument("--pc1", default="http://192.168.2.59:18080",
                   help="PC1 (sender) agent URL")
    p.add_argument("--pc2", default="http://127.0.0.1:18080",
                   help="PC2 (receiver) agent URL")
    args = p.parse_args()

    state = State(args.pc1, args.pc2)
    try:
        run_tui(state)
    except KeyboardInterrupt:
        pass

if __name__ == "__main__":
    main()
