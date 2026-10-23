"""
test_anim.py — 高速动画窗口，用于 DDA 采集测试。
目标：以 180+ fps 持续更新桌面内容，让 DDA AcquireNextFrame 有足够帧率。
运行方式：python scripts/test_anim.py
关闭：Esc 或直接关窗口
"""
import tkinter as tk
import time
import math
import random

W, H = 960, 720  # 窗口尺寸，足够大让裁剪区域内容丰富

class AnimWin:
    def __init__(self):
        self.root = tk.Tk()
        self.root.title("VirtualCapture DDA Test (close=Esc)")
        self.root.geometry(f"{W}x{H}+100+100")
        self.root.resizable(False, False)
        self.canvas = tk.Canvas(self.root, width=W, height=H, bg="#000010", highlightthickness=0)
        self.canvas.pack()
        self.root.bind("<Escape>", lambda e: self.root.destroy())
        self.t0 = time.perf_counter()
        self.frame = 0
        self.fps_display = 0
        self.fps_last = time.perf_counter()
        self.fps_count = 0
        # 随机球
        self.balls = [
            {"x": random.uniform(50, W-50), "y": random.uniform(50, H-50),
             "vx": random.uniform(-4, 4) or 2, "vy": random.uniform(-4, 4) or 2,
             "r": random.randint(20, 50),
             "hue": random.randint(0, 360)}
            for _ in range(14)
        ]
        self._draw()

    def _hsv_hex(self, h, s=1.0, v=1.0):
        h = h % 360
        hi = int(h / 60) % 6
        f = (h / 60) - int(h / 60)
        p = v * (1 - s); q = v * (1 - f * s); t_v = v * (1 - (1 - f) * s)
        r, g, b = [(v,t_v,p),(q,v,p),(p,v,t_v),(p,q,v),(t_v,p,v),(v,p,q)][hi]
        return "#%02x%02x%02x" % (int(r*255), int(g*255), int(b*255))

    def _draw(self):
        c = self.canvas
        c.delete("all")
        now = time.perf_counter()
        t = now - self.t0

        # 背景渐变色带（每帧移动）
        stripe_w = 40
        shift = int(t * 120) % stripe_w
        for x in range(-stripe_w + shift, W + stripe_w, stripe_w):
            hue = (x / W * 180 + t * 30) % 360
            c.create_rectangle(x, 0, x + stripe_w // 2, H,
                                fill=self._hsv_hex(hue, 0.6, 0.25), outline="")

        # 移动球
        for b in self.balls:
            b["x"] += b["vx"]; b["y"] += b["vy"]
            if b["x"] - b["r"] < 0: b["x"] = b["r"]; b["vx"] = abs(b["vx"])
            if b["x"] + b["r"] > W: b["x"] = W - b["r"]; b["vx"] = -abs(b["vx"])
            if b["y"] - b["r"] < 0: b["y"] = b["r"]; b["vy"] = abs(b["vy"])
            if b["y"] + b["r"] > H: b["y"] = H - b["r"]; b["vy"] = -abs(b["vy"])
            b["hue"] = (b["hue"] + 2) % 360
            x, y, r = b["x"], b["y"], b["r"]
            col = self._hsv_hex(b["hue"])
            c.create_oval(x-r, y-r, x+r, y+r, fill=col, outline="white", width=2)

        # 旋转扇形
        cx, cy = W // 2, H // 2
        for i in range(8):
            ang = t * 180 + i * 45
            x2 = cx + 120 * math.cos(math.radians(ang))
            y2 = cy + 120 * math.sin(math.radians(ang))
            c.create_line(cx, cy, x2, y2, fill=self._hsv_hex(ang % 360), width=3)

        # 噪声块（强迫DDA刷新整个画面）
        for _ in range(30):
            nx = random.randint(0, W-20); ny = random.randint(0, H-20)
            c.create_rectangle(nx, ny, nx+8, ny+8,
                                fill=self._hsv_hex(random.randint(0,360)), outline="")

        # 帧计数 / fps
        self.fps_count += 1
        if now - self.fps_last >= 1.0:
            self.fps_display = self.fps_count
            self.fps_count = 0
            self.fps_last = now
        c.create_text(10, 10, anchor="nw", text=f"frame={self.frame}  fps={self.fps_display}",
                      fill="white", font=("Consolas", 12))
        self.frame += 1

        # 立刻调度下一帧（after 0 = 尽可能快）
        self.root.after(0, self._draw)

    def run(self):
        self.root.mainloop()

if __name__ == "__main__":
    AnimWin().run()
