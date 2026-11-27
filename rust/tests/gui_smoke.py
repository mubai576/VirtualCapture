"""Exercise real GPUI role selection, persistence and host controls in private roots.
Only loopback control HTTP; no stream, packet transmission or registry operations.
Needs an interactive Windows desktop.
"""
import argparse
import ctypes as c
from ctypes import wintypes as w
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request
import uuid
from PIL import ImageGrab

ROOT = Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--package", type=Path, help="Exercise an extracted portable package instead of debug outputs")
args = parser.parse_args()
NATIVE = args.package.resolve() if args.package else ROOT / "rust/target/debug"
EXE = NATIVE / "VirtualCapture.exe"
RUN = ROOT / "run" / ("rust-gui-smoke-" + uuid.uuid4().hex[:10])
RUN.mkdir(parents=True)
user = c.windll.user32
kernel = c.windll.kernel32
user.SetProcessDPIAware()
user.GetWindowThreadProcessId.argtypes = [w.HWND, c.POINTER(w.DWORD)]
user.SetForegroundWindow.argtypes = [w.HWND]
user.GetForegroundWindow.restype = w.HWND
user.GetClientRect.argtypes = [w.HWND, c.POINTER(w.RECT)]
user.ClientToScreen.argtypes = [w.HWND, c.POINTER(w.POINT)]
user.SetWindowPos.argtypes = [w.HWND, w.HWND, c.c_int, c.c_int, c.c_int, c.c_int, w.UINT]
user.GetDpiForWindow.argtypes = [w.HWND]
user.PostMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
user.GetWindowTextW.argtypes = [w.HWND, w.LPWSTR, c.c_int]
user.SendMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
user.SendMessageW.restype = c.c_ssize_t
user.GetClassLongPtrW.argtypes = [w.HWND, c.c_int]
user.GetClassLongPtrW.restype = c.c_ssize_t
kernel.GetCurrentThreadId.restype = w.DWORD
user.AttachThreadInput.argtypes = [w.DWORD, w.DWORD, w.BOOL]
user.AttachThreadInput.restype = w.BOOL
user.PeekMessageW.argtypes = [c.POINTER(w.MSG), w.HWND, w.UINT, w.UINT, w.UINT]
user.PeekMessageW.restype = w.BOOL
CALLBACK = c.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)


def find_window(pid, title_prefix="VirtualCapture"):
    found = []
    @CALLBACK
    def visit(hwnd, _):
        owner = w.DWORD()
        user.GetWindowThreadProcessId(hwnd, c.byref(owner))
        title = c.create_unicode_buffer(512)
        user.GetWindowTextW(hwnd, title, 512)
        if owner.value == pid and title.value.startswith(title_prefix) and user.IsWindowVisible(hwnd):
            found.append(hwnd)
        return True
    user.EnumWindows(visit, 0)
    return found[0] if found else None


def focus_window(hwnd, timeout=2):
    """Activate the test window despite Windows foreground-lock restrictions."""
    # AttachThreadInput 要求调用线程已有输入队列；先窥视（不移除）消息以建立队列。
    message = w.MSG()
    user.PeekMessageW(c.byref(message), None, 0, 0, 0)
    current_thread = kernel.GetCurrentThreadId()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        foreground = user.GetForegroundWindow()
        foreground_thread = user.GetWindowThreadProcessId(foreground, None) if foreground else 0
        target_thread = user.GetWindowThreadProcessId(hwnd, None)
        attached = []
        for thread_id in dict.fromkeys((foreground_thread, target_thread)):
            if thread_id and thread_id != current_thread:
                if user.AttachThreadInput(current_thread, thread_id, True):
                    attached.append(thread_id)
        try:
            user.ShowWindow(hwnd, 9)  # SW_RESTORE
            user.BringWindowToTop(hwnd)
            user.SetForegroundWindow(hwnd)
            user.SetActiveWindow(hwnd)
            user.SetFocus(hwnd)
        finally:
            for thread_id in reversed(attached):
                user.AttachThreadInput(current_thread, thread_id, False)
        if user.GetForegroundWindow() == hwnd:
            return True
        time.sleep(.05)
    return user.GetForegroundWindow() == hwnd


def screenshot(hwnd, name):
    # GPUI's GPU surface cannot be captured with PrintWindow. Put only our window
    # above others briefly, then crop its opaque client area and restore z-order.
    assert user.SetWindowPos(hwnd, w.HWND(-1), 0, 0, 0, 0, 0x0013)
    try:
        # 繁忙桌面上前台可能被其他应用短暂抢走；有界重试，但截图前仍必须是本窗口前台。
        deadline = time.monotonic() + 8
        while True:
            focus_window(hwnd)
            time.sleep(.3)
            if user.GetForegroundWindow() == hwnd:
                break
            assert time.monotonic() < deadline, (
                "目标窗口未获得前台，拒绝把其他应用截图作为 GPUI 证据",
                hwnd, user.GetForegroundWindow())
        bounds = w.RECT()
        assert user.GetClientRect(hwnd, c.byref(bounds))
        origin = w.POINT(0, 0)
        assert user.ClientToScreen(hwnd, c.byref(origin))
        image = ImageGrab.grab(bbox=(origin.x, origin.y, origin.x + bounds.right, origin.y + bounds.bottom), all_screens=True)
        image.save(RUN / name)
        colors = image.getcolors(256)
        assert colors is None or len(colors) > 8, (
            "Window screenshot was blank", {
                "evidence": str(RUN / name), "colors": colors,
                "client_origin": (origin.x, origin.y), "client_size": image.size,
                "minimized": bool(user.IsIconic(hwnd)),
            })
        return image
    finally:
        user.SetWindowPos(hwnd, w.HWND(-2), 0, 0, 0, 0, 0x0013)


def click(hwnd, x, y):
    scale = user.GetDpiForWindow(hwnd) / 96
    point = w.POINT(round(x * scale), round(y * scale))
    position = (point.y << 16) | point.x
    assert user.PostMessageW(hwnd, 0x0201, 1, position)
    assert user.PostMessageW(hwnd, 0x0202, 0, position)
    time.sleep(.2)


# Screenshot pixels are physical; clicks take logical coordinates, so divide by scale.
def border_pixel(pixel):
    return abs(pixel[0] - 225) <= 4 and abs(pixel[1] - 230) <= 4 and abs(pixel[2] - 237) <= 4


def input_border_pixel(pixel):
    return border_pixel(pixel) or (pixel[0] < 150 and 60 < pixel[1] < 190 and pixel[2] > 210)


def dark_pixel(pixel):
    return sum(pixel[:3]) < 520


def runs(image, y, x0, x1, predicate, min_len):
    px = image.load()
    found, start = [], None
    x = x0
    for x in range(x0, x1):
        if predicate(px[x, y]):
            if start is None:
                start = x
        elif start is not None:
            if x - start >= min_len:
                found.append((start, x - 1))
            start = None
    if start is not None and x1 - start >= min_len:
        found.append((start, x1 - 1))
    return found


def card_top(image):
    """First long card border below the fixed header: proves no heading/banner remains."""
    for y in range(90, 420):
        if runs(image, y, 330, min(1240, image.width - 10), border_pixel, 700):
            return y
    raise AssertionError("main settings card border not found")


def stage_top(image):
    """Top edge of the dark output bar; moves down when the update panel opens."""
    px = image.load()
    for y in range(120, image.height - 2):
        count = 0
        for x in range(330, image.width - 40):
            if sum(px[x, y]) < 200:
                count += 1
        if count > 700:
            return y
    raise AssertionError("dark format bar not found")


def text_clusters(image, y0, y1, x0, x1, merge=12):
    """Merged dark-text clusters between x0..x1, left to right."""
    px = image.load()
    clusters, start, last = [], None, None
    for x in range(x0, x1):
        if any(dark_pixel(px[x, y]) for y in range(y0, y1)):
            if start is None:
                start = x
            last = x
        elif start is not None and x - last > merge:
            clusters.append((start, last))
            start = None
    if start is not None:
        clusters.append((start, last))
    return clusters


def header_clusters(image, scale):
    """Merged dark-text clusters on the right of the header, left to right."""
    return text_clusters(image, int(18 * scale), int(56 * scale), int(image.width * .6), image.width - 8)


def find_input(image, y0, y1, x0, x1, scale):
    """Locate a bordered input box by matching its top and bottom border runs."""
    min_len, max_len = int(180 * scale), int(520 * scale)
    for y in range(y0, y1):
        for left, right in runs(image, y, x0, x1, input_border_pixel, min_len):
            if right - left > max_len:
                continue
            for bottom in range(y + int(24 * scale), y + int(33 * scale)):
                if any(abs(a - left) <= 3 and abs(b - right) <= 3
                       for a, b in runs(image, bottom, x0, x1, input_border_pixel, min_len)):
                    return {"left": left, "right": right, "top": y, "bottom": bottom}
    return None


def window_icon(hwnd):
    # WM_GETICON first, then the window class icon registered from the exe resource.
    for kind in (0, 1, 2):
        icon = user.SendMessageW(hwnd, 0x007F, kind, 0)
        if icon:
            return icon
    for index in (-14, -34):
        icon = user.GetClassLongPtrW(hwnd, index)
        if icon:
            return icon
    return 0


def wait_for_window(proc, role):
    expected = f"VirtualCapture · {role}"
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        assert proc.poll() is None, f"Desktop exited before {expected}: rc={proc.returncode}"
        hwnd = find_window(proc.pid, expected)
        if hwnd:
            focus_window(hwnd)
            time.sleep(.5)
            return hwnd
        time.sleep(.1)
    raise AssertionError(f"Window did not enter {expected}; evidence: {RUN}")


def wait_for_role(role):
    path = RUN / "vc-startup.json"
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            preference = json.loads(path.read_text(encoding="utf-8"))
            if preference == {"schema": 1, "role": role}:
                return preference
        except (FileNotFoundError, json.JSONDecodeError):
            pass
        time.sleep(.1)
    raise AssertionError(f"UI did not persist role {role}; evidence: {RUN}")


def close_desktop(proc, hwnd):
    assert user.PostMessageW(hwnd, 0x0010, 0, 0)  # WM_CLOSE
    assert proc.wait(timeout=15) == 0, (proc.returncode, str(RUN))


def outlined_buttons(image, scale):
    """Locate actual button borders; never assume a fixed chooser position."""
    def edge(pixel):
        return border_pixel(pixel) or (pixel[0] < 120 and 50 < pixel[1] < 150 and pixel[2] > 200)

    buttons = []
    min_len, max_len = int(55 * scale), int(230 * scale)
    for top in range(image.height - int(42 * scale)):
        for left, right in runs(image, top, 0, image.width, edge, min_len):
            if right - left > max_len:
                continue
            for bottom in range(top + int(25 * scale), top + int(40 * scale)):
                if any(abs(a - left) <= 3 * scale and abs(b - right) <= 3 * scale
                       for a, b in runs(image, bottom, max(0, left - 5), min(image.width, right + 6), edge, min_len)):
                    center = ((left + right) / 2, (top + bottom) / 2)
                    if not any(abs(center[0] - x) < 12 * scale and abs(center[1] - y) < 20 * scale
                               for x, y in buttons):
                        buttons.append(center)
                    break
    return sorted(buttons)


def choose_role(hwnd, role, image):
    scale = user.GetDpiForWindow(hwnd) / 96
    buttons = outlined_buttons(image, scale)
    assert len(buttons) == 2, ("Role chooser must show two buttons", buttons, str(RUN))
    x, y = buttons[0 if role == "capture" else 1]
    click(hwnd, x / scale, y / scale)
    return wait_for_role(role)


def wait_for_health(port):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        try:
            with opener.open(f"http://127.0.0.1:{port}/health", timeout=.5) as response:
                return json.load(response)
        except (OSError, urllib.error.URLError):
            time.sleep(.1)
    raise AssertionError(f"Capture health did not become ready on 127.0.0.1:{port}")


def require_listener_closed(port):
    with socket.socket() as sock:
        sock.settimeout(.5)
        assert sock.connect_ex(("127.0.0.1", port)) != 0, "采集后台退出后仍在监听"


def require_config_unchanged(path, before):
    after = json.loads(path.read_text(encoding="utf-8"))
    stable = lambda config: {key: value for key, value in config.items() if key != "adapter"}
    assert stable(after) == stable(before), ("角色选择改写了启动角色以外的配置", before, after)


def exercise_roles():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    config_path = RUN / "vc-rust-config.json"
    original_config = json.loads(config_path.read_text(encoding="utf-8"))
    preference_path = RUN / "vc-startup.json"
    assert not preference_path.exists()
    processes = []

    def launch(name, *extra):
        with (RUN / f"{name}.log").open("wb") as log:
            proc = subprocess.Popen(
                [str(EXE), "--root", str(RUN), "--native-dir", str(NATIVE),
                 "--capture-bind", "127.0.0.1", "--capture-port", str(port), *extra],
                cwd=NATIVE, stdout=log, stderr=log,
            )
        processes.append(proc)
        return proc

    try:
        proc = launch("roles-first-start")
        hwnd = wait_for_window(proc, "选择本机角色")
        assert window_icon(hwnd), "Window/exe icon resource is missing"
        assert not preference_path.exists(), "角色未确认时不应写启动偏好"
        require_config_unchanged(config_path, original_config)
        require_listener_closed(port)
        chooser = screenshot(hwnd, "role-chooser.png")
        choose_role(hwnd, "capture", chooser)
        hwnd = wait_for_window(proc, "采集端")
        health = wait_for_health(port)
        assert health["ok"] and health["role"] == "capture" and health["pid"] == proc.pid, health
        assert Path(health["root"]).samefile(RUN), health
        screenshot(hwnd, "capture-role.png")
        require_config_unchanged(config_path, original_config)
        close_desktop(proc, hwnd)
        require_listener_closed(port)

        proc = launch("roles-saved-capture")
        hwnd = wait_for_window(proc, "采集端")
        assert wait_for_health(port)["pid"] == proc.pid
        scale = user.GetDpiForWindow(hwnd) / 96
        deadline = time.monotonic() + 8
        while True:
            image = screenshot(hwnd, "saved-capture.png")
            buttons = outlined_buttons(image, scale)
            if len(buttons) == 1:
                break
            assert time.monotonic() < deadline, ("采集页未渲染重新选择角色按钮", buttons, str(RUN))
            time.sleep(.3)
        click(hwnd, buttons[0][0] / scale, buttons[0][1] / scale)
        hwnd = wait_for_window(proc, "选择本机角色")
        require_listener_closed(port)
        assert wait_for_role("capture"), "打开重选页不应改写原角色"
        require_config_unchanged(config_path, original_config)
        chooser = screenshot(hwnd, "role-reselect.png")
        choose_role(hwnd, "receive", chooser)
        hwnd = wait_for_window(proc, "接收端")
        require_config_unchanged(config_path, original_config)
        require_listener_closed(port)
        image = screenshot(hwnd, "receive-role.png")
        clusters = header_clusters(image, scale)
        assert len(clusters) >= 4, ("接收端缺少重新选择角色入口", clusters)
        click(hwnd, sum(clusters[-4]) / 2 / scale, 36)
        hwnd = wait_for_window(proc, "选择本机角色")
        chooser = screenshot(hwnd, "receive-reselect.png")
        assert wait_for_role("receive")
        choose_role(hwnd, "receive", chooser)
        hwnd = wait_for_window(proc, "接收端")
        close_desktop(proc, hwnd)

        proc = launch("roles-saved-receive")
        hwnd = wait_for_window(proc, "接收端")
        screenshot(hwnd, "saved-receiver.png")
        require_listener_closed(port)
        close_desktop(proc, hwnd)

        proc = launch("roles-explicit-reselect", "--select-role")
        hwnd = wait_for_window(proc, "选择本机角色")
        screenshot(hwnd, "explicit-role-chooser.png")
        close_desktop(proc, hwnd)
        require_config_unchanged(config_path, original_config)
        preference = wait_for_role("receive")
        return {
            "startup_preference": preference, "control_config_unchanged_by_roles": True,
            "capture_http": {key: health[key] for key in ("ok", "role", "pid", "root")},
            "role_screenshots": ["role-chooser.png", "capture-role.png", "saved-capture.png",
                                 "role-reselect.png", "receive-role.png", "receive-reselect.png",
                                 "saved-receiver.png", "explicit-role-chooser.png"],
        }
    finally:
        for proc in processes:
            if proc.poll() is None:
                hwnd = find_window(proc.pid)
                if hwnd:
                    user.PostMessageW(hwnd, 0x0010, 0, 0)
                try:
                    proc.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()


def main():
    requested = threading.Event()
    requests = []
    class FailingCapture(BaseHTTPRequestHandler):
        def do_GET(self):
            requests.append(self.path)
            data = json.dumps({"error": "GUI smoke: capture unavailable (503)."}).encode()
            self.send_response(503)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            requested.set()
        def log_message(self, *_):
            pass

    with ThreadingHTTPServer(("127.0.0.1", 0), FailingCapture) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        address = f"127.0.0.1:{server.server_port}"
        assert EXE.is_file(), f"Missing {EXE}; build the workspace or pass --package"
        # Preserve capture-side diagnostics while independently recording what the
        # host UI must retain. No packets or registry operations are performed.
        listed = subprocess.run([
            str(EXE), "capture", "--root", str(RUN), "--native-dir", str(NATIVE), "--list-adapters",
        ], capture_output=True, encoding="utf-8", timeout=40, check=True)
        adapters = json.loads(listed.stdout)
        physical = [item for item in adapters if item.get("hardware_interface") is True
                    and item.get("virtual") is False
                    and (item.get("status") or "").strip().lower() == "up"
                    and item["name"].lower() != r"\Device\NPF_Loopback".lower()]
        (RUN / "adapters.json").write_text(json.dumps({"all": adapters, "physical": physical}, ensure_ascii=False, indent=2), encoding="utf-8")
        # A saved virtual selection must be cleared, never quietly replaced.
        (RUN / "vc-rust-config.json").write_text(json.dumps({
            "version": 1, "peer_url": "", "adapter": r"\Device\NPF_Loopback",
        }), encoding="utf-8")
        roles = exercise_roles()
        with (RUN / "host.log").open("wb") as log:
            ready_path = RUN / "update-ready.json"
            proc = subprocess.Popen(
                [str(EXE), "--root", str(RUN), "--native-dir", str(NATIVE),
                 "--update-ready", str(ready_path)],
                cwd=NATIVE, stdout=log, stderr=log,
            )
        hwnd = None
        try:
            hwnd = wait_for_window(proc, "接收端")
            assert window_icon(hwnd), "Window/exe icon resource is missing"
            receipts = list((RUN / "run").glob(f"host-*-{proc.pid}.launch.json"))
            assert len(receipts) == 1, receipts
            receipt = json.loads(receipts[0].read_text(encoding="utf-8"))
            assert receipt["pid"] == proc.pid and Path(receipt["root"]).samefile(RUN), receipt
            assert Path(receipt["exe"]).samefile(EXE), receipt
            for stream in ("stdout", "stderr"):
                logfile = Path(receipt[stream])
                assert logfile.is_file() and logfile.parent.samefile(RUN / "run"), receipt
            deadline = time.monotonic() + 10
            while not ready_path.exists():
                assert time.monotonic() < deadline, "First-frame update-ready marker was not written"
                time.sleep(.1)
            ready = json.loads(ready_path.read_text(encoding="utf-8"))
            assert ready["pid"] == proc.pid and ready["root"] and "build" in ready, ready
            initial = screenshot(hwnd, "initial.png")
            scale = user.GetDpiForWindow(hwnd) / 96
            top = card_top(initial)
            assert abs(top - 100 * scale) <= 8 * scale, ("主区大标题或横幅仍然存在", top / scale)
            px = initial.load()
            assert not any(dark_pixel(px[x, y]) for y in range(int(80 * scale), top - int(4 * scale))
                           for x in range(330, 1240)), "主区上方仍有多余标题或横幅"
            stage = stage_top(initial)
            clusters = header_clusters(initial, scale)
            assert len(clusters) >= 3, ("找不到『更新』页签", clusters)
            updates_x = (clusters[-3][0] + clusters[-3][1]) / 2 / scale
            # Open the compact update panel; pairing is removed, so no token input
            # or token file may appear.
            click(hwnd, updates_x, 36)
            time.sleep(.5)
            opened = screenshot(hwnd, "updates.png")
            assert stage_top(opened) - stage > 120 * scale, ("更新面板没有展开", stage, stage_top(opened))
            box = find_input(opened, top + int(8 * scale), top + int(380 * scale),
                             int(opened.width * .6), opened.width - 12, scale)
            assert box is None, ("更新面板仍显示令牌输入框", box)
            assert not (RUN / "update-peer-token.json").exists(), "更新面板生成了配对令牌文件"
            # Collapsing the panel must restore the exact layout the fixed click
            # coordinates below were measured against.
            click(hwnd, updates_x, 36)
            time.sleep(.5)
            assert abs(stage_top(screenshot(hwnd, "updates-closed.png")) - stage) <= 6 * scale, "收起更新面板后布局没有复原"
            # Presets must update the actual persisted draft, not only their appearance.
            click(hwnd, 658, 192)  # 720p
            click(hwnd, 554, 252)  # 120 fps
            deadline = time.monotonic() + 5
            while True:
                config = json.loads((RUN / "vc-rust-config.json").read_text(encoding="utf-8"))
                capture = config.get("capture", {})
                if capture.get("width") == 1280 and capture.get("height") == 720 and capture.get("fps") == 120:
                    break
                assert time.monotonic() < deadline, ("Preset interaction did not save", capture)
                time.sleep(.1)
            time.sleep(.5)
            screenshot(hwnd, "presets-720p.png")
            click(hwnd, 580, 192)  # restore 640 square
            click(hwnd, 627, 252)  # restore 240 fps
            time.sleep(.5)
            restored = json.loads((RUN / "vc-rust-config.json").read_text(encoding="utf-8"))["capture"]
            assert (restored["width"], restored["height"], restored["fps"]) == (640, 640, 240), restored
            click(hwnd, 1200, 740)  # expand real diagnostic table
            screenshot(hwnd, "details.png")
            click(hwnd, 1200, 740)  # collapse table
            assert user.SetWindowPos(hwnd, None, 0, 0, 1096, 779, 0x0016)
            time.sleep(.4)
            screenshot(hwnd, "compact.png")
            assert user.SetWindowPos(hwnd, None, 0, 0, 1296, 939, 0x0016)
            time.sleep(.4)
            click(hwnd, 252, 264)  # enumerate real host adapters
            deadline = time.monotonic() + 40
            while True:
                draft = json.loads((RUN / "vc-rust-config.json").read_text(encoding="utf-8"))
                if draft.get("adapter") == "":
                    break
                assert time.monotonic() < deadline, ("Saved virtual selection was not cleared", draft.get("adapter"))
                time.sleep(.1)
            time.sleep(1)
            screenshot(hwnd, "physical-adapters.png")
            # Deliver native pointer/character messages to this HWND only. Windows may
            # deny foreground activation to a background runner; never type into another app.
            click(hwnd, 130, 165)
            for character in address:
                assert user.PostMessageW(hwnd, 0x0102, ord(character), 0)
            time.sleep(.3)
            click(hwnd, 130, 213)
            if not requested.wait(10):
                screenshot(hwnd, "interaction-failed.png")
                raise AssertionError(f"GUI Connect did not reach the isolated HTTP fixture; evidence: {RUN}")
            assert requests == ["/health"], requests
            time.sleep(1)
            screenshot(hwnd, "connection-error.png")
            user.PostMessageW(hwnd, 0x0010, 0, 0)
            proc.wait(timeout=15)
            assert proc.returncode == 0, (proc.returncode, (RUN / "host.log").read_text(errors="replace"))
            report = {"ok": True, "window": "VirtualCapture", "process_exit": proc.returncode, "address": address, "requests": requests, "response": 503, "screenshots": [*roles["role_screenshots"], "initial.png", "updates.png", "updates-closed.png", "presets-720p.png", "details.png", "compact.png", "physical-adapters.png", "connection-error.png"], "physical_adapters": [item.get("label") or item["description"] for item in physical], "excluded_adapters": len(adapters) - len(physical), "checks": ["application icon resource", "first-frame update-ready marker", "role selection, persistence, restart and both in-app reselect controls", "capture HTTP binds to requested loopback port and closes before switching", "control config unaffected by role selection", "main heading and cleanup banner removed", "update panel opens and closes without shifting layout", "update panel has no pairing token input or token file", "720p and 120fps saved", "640 square and 240fps restored", "diagnostic table expansion", "compact window layout", "real host adapter refresh", "saved virtual selection cleared", "connection HTTP error", "normal window close"], "roles": roles, "scope": "real native GPUI controls, role persistence and host workbench; no data stream, physical NIC transmission or DShow registration"}
            (RUN / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
            print(json.dumps({"run": str(RUN), **report}, ensure_ascii=False))
        finally:
            if proc.poll() is None:
                if hwnd:
                    user.PostMessageW(hwnd, 0x0010, 0, 0)
                try:
                    proc.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
            server.shutdown()
            worker.join(timeout=5)


if __name__ == "__main__":
    main()
