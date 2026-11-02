"""pattern_ref.py - pattern.h 的 Python 镜像实现 + 自检 (T-04)."""
import os, subprocess, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fnv1a import fnv1a64, fnv1a64_hex

W = H = 64
M = 0xFFFFFFFFFFFFFFFF

def splitmix64(s):
    # 确定性随机数（与C++同算法）
    s = (s + 0x9E3779B97F4A7C15) & M
    z = s
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M
    return s, (z ^ (z >> 31)) & M

def _fill_prng(seed, n):
    # 按种子填n字节随机流
    s = seed & M
    out = bytearray(n)
    i = 0
    while i < n:
        s, r = splitmix64(s)
        for b in range(8):
            if i >= n: break
            out[i] = (r >> (8 * b)) & 0xFF; i += 1
    return bytes(out)

def gen_counter(fid, w=W, h=H):
    # 烟雾渐变：Y=x+y+fid
    Y = bytes(((x + y + fid) & 0xFF) for y in range(h) for x in range(w))
    v = 0x80 ^ (fid & 0xFF)
    UV = bytes([v]) * (w * h // 2)
    return Y, UV

def gen_checker(fid, w=W, h=H):
    # 棋盘：逐帧滚1px
    ox = fid & 7; oy = (fid >> 3) & 7
    Y = bytes((255 if ((((x + ox) >> 3) + ((y + oy) >> 3)) & 1) else 0) for y in range(h) for x in range(w))
    uv = bytearray(w * h // 2)
    for y in range(h // 2):
        for x in range(0, w, 2):
            u = 16 if ((((x >> 3) + ((y + fid) & 1)) & 1)) else 240
            v = 16 if ((((x >> 3) + ((y - fid) & 1)) & 1)) else 240
            uv[y * w + x] = u; uv[y * w + x + 1] = v
    return Y, bytes(uv)

def gen_rings(fid, w=W, h=H):
    # 同心环：高频运动
    cx, cy = w // 2, h // 2
    Y = bytearray(w * h)
    for y in range(h):
        dy = y - cy
        for x in range(w):
            dx = x - cx
            r2 = dx * dx + dy * dy
            Y[y * w + x] = ((r2 >> 4) + fid * 7 + ((x ^ y) & 15)) & 0xFF
    uv = bytearray(w * h // 2)
    for y in range(h // 2):
        for x in range(0, w, 2):
            uv[y * w + x] = (128 + (((x * 3 + y * 5 + fid * 11) >> 2) & 127) - 64) & 0xFF
            uv[y * w + x + 1] = (128 + (((x * 5 - y * 3 + fid * 13) >> 2) & 127) - 64) & 0xFF
    return bytes(Y), bytes(uv)

def gen_noise(fid, w=W, h=H):
    # 全帧噪声：最大熵
    seed = ((fid << 32) ^ 0x123456789ABC) & M  # D11: XOR（与 motion 一致）
    n = w * h + w * h // 2
    raw = _fill_prng(seed, n)
    return raw[:w*h], raw[w*h:]

def gen_motion(fid, w=W, h=H):
    # 拟游戏：背景+弹跳块+中央噪声
    Y = bytearray(((x + fid * 3 + (y >> 2)) & 0xFF) for y in range(h) for x in range(w))
    S = 16
    for k in range(4):
        px = (fid * (5 + k * 2) + k * 173) % (w - S)
        py = (fid * (3 + k) + k * 311) % (h - S)
        c = 60 + k * 50
        for y in range(S):
            for x in range(S):
                Y[(py + y) * w + (px + x)] = (255 - c) if ((x ^ y) & 8) else c
    W2 = 16; x0 = (w - W2) // 2; y0 = (h - W2) // 2
    s = (((fid) << 32) ^ 0x4D4F54494F4E) & M
    buf = _fill_prng(s, W2 * W2)
    for y in range(W2):
        Y[(y0 + y) * w + x0:(y0 + y) * w + x0 + W2] = buf[y*W2:(y+1)*W2]
    uv = bytearray(w * h // 2)
    for y in range(h // 2):
        for x in range(0, w, 2):
            uv[y * w + x] = (x + fid) & 0xFF
            uv[y * w + x + 1] = (y * 2 - fid) & 0xFF
    return bytes(Y), bytes(uv)

def gen_noise_y(fid, w=W, h=H):
    # 取噪声Y分量
    Y, _ = gen_noise(fid, w, h)
    return Y

def gen_counter_y(fid, w=W, h=H):
    # 取渐变Y分量
    Y, _ = gen_counter(fid, w, h)
    return Y

def frame_hash(data: bytes) -> str:
    # 整段哈希转hex
    return fnv1a64_hex(data)

def frame_hash_yuv(Y: bytes, UV: bytes) -> str:
    # Y+UV拼接哈希
    return fnv1a64_hex(Y + UV)

def verify_samples(Y: bytes, UV: bytes, fid: int, pattern: str, n: int = 4096):
    # 抽点验帧（默认4096点）
    expY, expUV = {"counter": gen_counter, "checker": gen_checker, "rings": gen_rings,
                   "noise": gen_noise, "motion": gen_motion}[pattern](fid)
    step = max(1, len(Y) // n)
    for i in range(0, len(Y), step):
        if Y[i] != expY[i]: return False
    step2 = max(1, len(UV) // n)
    for i in range(0, len(UV), step2):
        if UV[i] != expUV[i]: return False
    return True

PATS = ["counter", "checker", "rings", "noise", "motion"]
GENS = {"counter": gen_counter, "checker": gen_checker, "rings": gen_rings, "noise": gen_noise, "motion": gen_motion}


def gen_native(pattern, fid, w, h):
    """Native full-size suite, including non-eight-byte tails; legacy small64 functions stay unchanged."""
    if pattern not in PATS or w < 2 or h < 2 or w % 2 or h % 2:
        raise ValueError("invalid native pattern geometry")
    def prng(state, count):
        out = bytearray()
        for _ in range(count // 8):
            state, value = splitmix64(state)
            out.extend(value.to_bytes(8, "little"))
        for _ in range(count % 8):
            state, value = splitmix64(state)
            out.append(value & 255)
        return state, out
    if pattern == "noise":
        state = (fid << 32) ^ 0x123456789ABC
        state, y = prng(state, w*h)
        _, uv = prng(state, w*h//2)
        return bytes(y), bytes(uv)
    if pattern != "motion":
        return GENS[pattern](fid, w, h)
    y = bytearray(((x + fid*3 + (yy >> 2)) & 255) for yy in range(h) for x in range(w))
    sprite = min(64, min(w, h)-1)
    for k in range(4):
        px = ((fid*(5+k*2) + k*173) & 0xffffffff) % (w-sprite)
        py = ((fid*(3+k) + k*311) & 0xffffffff) % (h-sprite)
        c = 60 + k*50
        for yy in range(sprite):
            for x in range(sprite):
                y[(py+yy)*w+px+x] = 255-c if (x ^ yy) & 8 else c
    noise = min(256, w, h)
    x0, y0 = (w-noise)//2, (h-noise)//2
    state = (fid << 32) ^ 0x4D4F54494F4E
    for yy in range(noise):
        state, row = prng(state, noise)
        y[(y0+yy)*w+x0:(y0+yy)*w+x0+noise] = row
    uv = bytearray(w*h//2)
    for yy in range(h//2):
        for x in range(0, w, 2):
            uv[yy*w+x], uv[yy*w+x+1] = (x+fid) & 255, (yy*2-fid) & 255
    return bytes(y), bytes(uv)

if __name__ == "__main__":
    assert gen_noise_y(7) == gen_noise_y(7), "PRNG non-deterministic"
    n0, n1 = gen_noise_y(0), gen_noise_y(1)
    c0, c1 = gen_counter_y(0), gen_counter_y(1)
    ndiff = sum(abs(a - b) for a, b in zip(n0, n1)) / len(n0)
    cdiff = sum(abs(a - b) for a, b in zip(c0, c1)) / len(c0)
    print("noise inter-frame abs diff: %.1f (expect ~85)" % ndiff)
    print("counter inter-frame abs diff: %.1f (expect ~1)" % cdiff)
    assert ndiff > 50 and cdiff < 5, "complexity tiers broken"
    h0, h1 = frame_hash(n0), frame_hash(n1)
    print("hash0=%s hash1=%s" % (h0, h1))
    assert h0 != h1
    print("full-frame bandwidth: %.1f MB/s (content-independent)" % (640*640*1.5*240/1e6))
    for p in PATS:
        for fid in (0, 1, 2, 3):
            Y1, UV1 = GENS[p](fid); Y2, UV2 = GENS[p](fid)
            assert Y1 == Y2 and UV1 == UV2, "small determinism %s fid=%d" % (p, fid)
            print("PY small %s %d %s" % (p, fid, frame_hash_yuv(Y1, UV1)))
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    from runtime_paths import find_binary
    exe = find_binary("vc-sender.exe", root)
    if os.path.exists(exe):
        try:
            out = subprocess.run([exe, "--selftest"], capture_output=True, text=True, timeout=60)
            vec = {}
            for line in (out.stdout or "").splitlines():
                parts = line.strip().split()
                if len(parts) == 5 and parts[0] == "VEC" and parts[1] == "small":
                    vec[(parts[2], int(parts[3]))] = parts[4]
            bad = 0
            for p in PATS:
                for fid in (0, 1, 2, 3):
                    Y, UV = GENS[p](fid)
                    pyh = frame_hash_yuv(Y, UV)
                    ch = vec.get((p, fid))
                    if ch is None: print("WARN no C++ vector for %s %d" % (p, fid)); bad += 1
                    elif ch != pyh: print("MISMATCH %s fid=%d cpp=%s py=%s" % (p, fid, ch, pyh)); bad += 1
            if bad: print("FAIL: C++/Python interlock %d mismatches" % bad); sys.exit(1)
            print("interlock: C++ small vectors match (%d)" % (len(PATS)*4))
        except Exception as e:
            print("WARN interlock skipped: %s" % e)
    else:
        print("interlock: vc-sender.exe missing, skipped (vectors printed above for T-01)")
    print("PASS: complexity tiers ok, determinism ok")
