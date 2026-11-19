"""Exercise the real GPUI window against an isolated failing HTTP fixture.
No capture/receiver/registry operations. Needs an interactive Windows desktop.
"""
import argparse
import ctypes as c
from ctypes import wintypes as w
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import subprocess
import threading
import time
import uuid
from PIL import ImageGrab

ROOT = Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--package", type=Path, help="Exercise an extracted portable package instead of debug outputs")
args = parser.parse_args()
NATIVE = args.package.resolve() if args.package else ROOT / "build/Release"
HOST = NATIVE / "vc-host.exe" if args.package else ROOT / "rust/target/debug/vc-host.exe"
RUN = ROOT / "run" / ("rust-gui-smoke-" + uuid.uuid4().hex[:10])
RUN.mkdir(parents=True)
user = c.windll.user32
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
CALLBACK = c.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)


def find_window(pid):
    found = []
    @CALLBACK
    def visit(hwnd, _):
        owner = w.DWORD()
        user.GetWindowThreadProcessId(hwnd, c.byref(owner))
        title = c.create_unicode_buffer(512)
        user.GetWindowTextW(hwnd, title, 512)
        if owner.value == pid and title.value == "VirtualCapture" and user.IsWindowVisible(hwnd):
            found.append(hwnd)
        return True
    user.EnumWindows(visit, 0)
    return found[0] if found else None


def screenshot(hwnd, name):
    # GPUI's GPU surface cannot be captured with PrintWindow. Put only our window
    # above others briefly, then crop its opaque client area and restore z-order.
    assert user.SetWindowPos(hwnd, w.HWND(-1), 0, 0, 0, 0, 0x0013)
    try:
        time.sleep(.3)
        bounds = w.RECT()
        assert user.GetClientRect(hwnd, c.byref(bounds))
        origin = w.POINT(0, 0)
        assert user.ClientToScreen(hwnd, c.byref(origin))
        image = ImageGrab.grab(bbox=(origin.x, origin.y, origin.x + bounds.right, origin.y + bounds.bottom), all_screens=True)
        assert len(image.getcolors(256) or []) > 8 or image.getcolors(256) is None, "Window screenshot was blank"
        image.save(RUN / name)
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


def label_right_of(image, box):
    """Dark label cluster (button text) right of the input box, same row."""
    clusters = text_clusters(image, box["top"] + 3, box["bottom"] - 2,
                             box["right"] + 12, image.width - 8, merge=8)
    return next((cluster for cluster in clusters if cluster[1] - cluster[0] >= 8), None)


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
        threading.Thread(target=server.serve_forever, daemon=True).start()
        address = f"127.0.0.1:{server.server_port}"
        # Preserve capture-side diagnostics while independently recording what the
        # host UI must retain. No packets or registry operations are performed.
        listed = subprocess.run([
            str(NATIVE / "vc-capture.exe" if args.package else ROOT / "rust/target/debug/vc-capture.exe"), "--root", str(RUN),
            "--native-dir", str(NATIVE), "--list-adapters",
        ], capture_output=True, encoding="utf-8", timeout=40, check=True)
        adapters = json.loads(listed.stdout)
        physical = [item for item in adapters if item.get("hardware_interface") is True
                    and item.get("virtual") is False
                    and item["name"].lower() != r"\Device\NPF_Loopback".lower()]
        (RUN / "adapters.json").write_text(json.dumps({"all": adapters, "physical": physical}, ensure_ascii=False, indent=2), encoding="utf-8")
        # A saved virtual selection must be cleared, never quietly replaced.
        (RUN / "vc-rust-config.json").write_text(json.dumps({
            "version": 1, "peer_url": "", "adapter": r"\Device\NPF_Loopback",
        }), encoding="utf-8")
        with (RUN / "host.log").open("wb") as log:
            ready_path = RUN / "update-ready.json"
            proc = subprocess.Popen([str(HOST), "--root", str(RUN), "--native-dir", str(NATIVE), "--update-ready", str(ready_path)], cwd=NATIVE, stdout=log, stderr=log)
        hwnd = None
        try:
            deadline = time.monotonic() + 15
            while not hwnd:
                assert proc.poll() is None, (RUN / "host.log").read_text(errors="replace")
                hwnd = find_window(proc.pid)
                assert time.monotonic() < deadline, "GPUI window did not appear"
                time.sleep(.1)
            user.SetForegroundWindow(hwnd)
            time.sleep(1)
            assert window_icon(hwnd), "Window/exe icon resource is missing"
            deadline = time.monotonic() + 10
            while not ready_path.exists():
                assert time.monotonic() < deadline, "First-frame update-ready marker was not written"
                time.sleep(.1)
            ready = json.loads(ready_path.read_text(encoding="utf-8"))
            assert ready["pid"] == proc.pid and ready["root"] and "build" in ready, ready
            initial = screenshot(hwnd, "initial.png")
            scale = user.GetDpiForWindow(hwnd) / 96
            # The former "采集工作台" heading and cleanup banner would push the card
            # down; assert the settings card starts directly under the header.
            top = card_top(initial)
            assert abs(top - 100 * scale) <= 8 * scale, ("主区大标题或横幅仍然存在", top / scale)
            px = initial.load()
            assert not any(dark_pixel(px[x, y]) for y in range(int(80 * scale), top - int(4 * scale))
                           for x in range(330, 1240)), "主区上方仍有多余标题或横幅"
            stage = stage_top(initial)
            clusters = header_clusters(initial, scale)
            assert len(clusters) >= 3, ("找不到『更新』页签", clusters)
            updates_x = (clusters[-3][0] + clusters[-3][1]) / 2 / scale
            # Open the compact update panel and pair a token through the real input.
            click(hwnd, updates_x, 36)
            time.sleep(.5)
            opened = screenshot(hwnd, "updates.png")
            assert stage_top(opened) - stage > 120 * scale, ("更新面板没有展开", stage, stage_top(opened))
            box = find_input(opened, top + int(8 * scale), top + int(380 * scale),
                             int(opened.width * .6), opened.width - 12, scale)
            assert box, "更新面板里找不到配对令牌输入框"
            token = "0123456789abcdef" * 4
            click(hwnd, (box["left"] + box["right"]) / 2 / scale, (box["top"] + box["bottom"]) / 2 / scale)
            for character in token:
                assert user.PostMessageW(hwnd, 0x0102, ord(character), 0)
            # The pair button is disabled until the value renders as a valid token;
            # the 350 ms ticker re-render enables it, so wait before clicking.
            time.sleep(1.0)
            typed = screenshot(hwnd, "updates-typed.png")
            pair = label_right_of(typed, box)
            assert pair, "更新面板里找不到配对按钮"
            print(json.dumps({"pair_box": box, "pair_label": pair, "scale": scale}), flush=True)
            click(hwnd, (pair[0] + pair[1]) / 2 / scale, (box["top"] + box["bottom"]) / 2 / scale)
            time.sleep(.5)
            screenshot(hwnd, "pair-clicked.png")
            token_file = RUN / "update-peer-token.json"
            deadline = time.monotonic() + 10
            while not token_file.exists():
                assert time.monotonic() < deadline, "配对令牌没有保存"
                time.sleep(.1)
            saved = json.loads(token_file.read_text(encoding="utf-8"))
            assert saved.get("token") == token, saved
            paired_im = screenshot(hwnd, "paired.png")
            # Paired hides the input; the compact "更换令牌" entry must reopen it.
            row = text_clusters(paired_im, box["top"] + 3, box["bottom"] - 2,
                                int(paired_im.width * .6), paired_im.width - 12)
            assert len(row) >= 2, ("已配对时找不到『更换令牌』入口", row)
            click(hwnd, (row[1][0] + row[1][1]) / 2 / scale, (box["top"] + box["bottom"]) / 2 / scale)
            time.sleep(.5)
            reopened = screenshot(hwnd, "updates-repair.png")
            box2 = find_input(reopened, top + int(8 * scale), top + int(380 * scale),
                              int(reopened.width * .6), reopened.width - 12, scale)
            assert box2, "『更换令牌』没有重新展开输入框"
            token2 = "fedcba9876543210" * 4
            click(hwnd, (box2["left"] + box2["right"]) / 2 / scale, (box2["top"] + box2["bottom"]) / 2 / scale)
            for character in token2:
                assert user.PostMessageW(hwnd, 0x0102, ord(character), 0)
            time.sleep(1.0)
            typed2 = screenshot(hwnd, "updates-repair-typed.png")
            pair2 = label_right_of(typed2, box2)
            assert pair2, "更换令牌后找不到配对按钮"
            click(hwnd, (pair2[0] + pair2[1]) / 2 / scale, (box2["top"] + box2["bottom"]) / 2 / scale)
            deadline = time.monotonic() + 10
            while True:
                current = json.loads(token_file.read_text(encoding="utf-8"))
                if current.get("token") == token2:
                    break
                assert time.monotonic() < deadline, ("重新配对没有保存新令牌", current.get("token"))
                time.sleep(.1)
            screenshot(hwnd, "repair-paired.png")
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
            report = {"ok": True, "window": "VirtualCapture", "process_exit": proc.returncode, "address": address, "requests": requests, "response": 503, "screenshots": ["initial.png", "updates.png", "updates-typed.png", "paired.png", "updates-repair.png", "updates-repair-typed.png", "repair-paired.png", "updates-closed.png", "presets-720p.png", "details.png", "compact.png", "physical-adapters.png", "connection-error.png"], "physical_adapters": [item.get("label") or item["description"] for item in physical], "excluded_adapters": len(adapters) - len(physical), "token_file": token_file.name, "checks": ["application icon resource", "first-frame update-ready marker", "main heading and cleanup banner removed", "update panel opens and closes without shifting layout", "peer update token paired and persisted", "paired state reopens token input and replaces token", "720p and 120fps saved", "640 square and 240fps restored", "diagnostic table expansion", "compact window layout", "real host adapter refresh", "saved virtual selection cleared", "connection HTTP error", "normal window close"], "scope": "real native GPUI controls, update panel pairing/re-pairing and draft persistence; no capture or DShow registration"}
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


if __name__ == "__main__":
    main()
