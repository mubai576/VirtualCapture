"""Independent Python mirror of the Rust integer NV12 test-pattern source.

Standard-library-only reference for the frozen pattern contract
(`rust/crates/vc-stream/src/pattern.rs`): splitmix64 PRNG, five integer-only
patterns, 32-bit wrapping semantics for `frame_id`, row-local PRNG grouping in
the motion noise window, and the non-eight-byte tail rule.  It has no
dependency on the product binaries, the legacy C++ tree, `ctrl/` or any build
output.

`gen_native(pattern, fid, w, h)` is the full-size suite used by the Rust
smoke/perf harnesses.  The small 64x64 `gen_*` helpers remain for historical
sample checks and the complexity-tier self-check.
"""
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))
from fnv1a import fnv1a64, fnv1a64_hex  # noqa: E402

W = H = 64
M = 0xFFFFFFFFFFFFFFFF


def splitmix64(s):
    # Deterministic 64-bit wrapping PRNG, identical to the Rust implementation.
    s = (s + 0x9E3779B97F4A7C15) & M
    z = s
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M
    return s, (z ^ (z >> 31)) & M


def _fill_prng(seed, n):
    # Fill n bytes from one continuous splitmix64 stream.
    s = seed & M
    out = bytearray(n)
    i = 0
    while i < n:
        s, r = splitmix64(s)
        for b in range(8):
            if i >= n:
                break
            out[i] = (r >> (8 * b)) & 0xFF
            i += 1
    return bytes(out)


def gen_counter(fid, w=W, h=H):
    # Gradient smoke: Y=x+y+fid.
    Y = bytes(((x + y + fid) & 0xFF) for y in range(h) for x in range(w))
    v = 0x80 ^ (fid & 0xFF)
    UV = bytes([v]) * (w * h // 2)
    return Y, UV


def gen_checker(fid, w=W, h=H):
    # 8x8 checkerboard that scrolls by one pixel per frame.
    ox = fid & 7
    oy = (fid >> 3) & 7
    Y = bytes((255 if ((((x + ox) >> 3) + ((y + oy) >> 3)) & 1) else 0)
              for y in range(h) for x in range(w))
    uv = bytearray(w * h // 2)
    for y in range(h // 2):
        for x in range(0, w, 2):
            u = 16 if ((((x >> 3) + ((y + fid) & 1)) & 1)) else 240
            v = 16 if ((((x >> 3) + ((y - fid) & 1)) & 1)) else 240
            uv[y * w + x] = u
            uv[y * w + x + 1] = v
    return Y, bytes(uv)


def gen_rings(fid, w=W, h=H):
    # Concentric high-frequency rings.
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
    # Full-frame splitmix64 noise: Y and UV share one PRNG stream.
    seed = ((fid << 32) ^ 0x123456789ABC) & M  # D11: XOR, high bits keep fid entropy
    n = w * h + w * h // 2
    raw = _fill_prng(seed, n)
    return raw[:w * h], raw[w * h:]


def gen_motion(fid, w=W, h=H):
    # Small historical motion generator (16-pixel sprites/window).
    Y = bytearray(((x + fid * 3 + (y >> 2)) & 0xFF) for y in range(h) for x in range(w))
    S = 16
    for k in range(4):
        px = (fid * (5 + k * 2) + k * 173) % (w - S)
        py = (fid * (3 + k) + k * 311) % (h - S)
        c = 60 + k * 50
        for y in range(S):
            for x in range(S):
                Y[(py + y) * w + (px + x)] = (255 - c) if ((x ^ y) & 8) else c
    W2 = 16
    x0 = (w - W2) // 2
    y0 = (h - W2) // 2
    s = (((fid) << 32) ^ 0x4D4F54494F4E) & M
    buf = _fill_prng(s, W2 * W2)
    for y in range(W2):
        Y[(y0 + y) * w + x0:(y0 + y) * w + x0 + W2] = buf[y * W2:(y + 1) * W2]
    uv = bytearray(w * h // 2)
    for y in range(h // 2):
        for x in range(0, w, 2):
            uv[y * w + x] = (x + fid) & 0xFF
            uv[y * w + x + 1] = (y * 2 - fid) & 0xFF
    return bytes(Y), bytes(uv)


def gen_noise_y(fid, w=W, h=H):
    Y, _ = gen_noise(fid, w, h)
    return Y


def gen_counter_y(fid, w=W, h=H):
    Y, _ = gen_counter(fid, w, h)
    return Y


def frame_hash(data: bytes) -> str:
    return fnv1a64_hex(data)


def frame_hash_yuv(Y: bytes, UV: bytes) -> str:
    return fnv1a64_hex(Y + UV)


def verify_samples(Y: bytes, UV: bytes, fid: int, pattern: str, n: int = 4096):
    # Sample-check a 64x64 frame against the small generators.
    expY, expUV = GENS[pattern](fid)
    step = max(1, len(Y) // n)
    for i in range(0, len(Y), step):
        if Y[i] != expY[i]:
            return False
    step2 = max(1, len(UV) // n)
    for i in range(0, len(UV), step2):
        if UV[i] != expUV[i]:
            return False
    return True


PATS = ["counter", "checker", "rings", "noise", "motion"]
GENS = {"counter": gen_counter, "checker": gen_checker, "rings": gen_rings,
        "noise": gen_noise, "motion": gen_motion}


def gen_native(pattern, fid, w, h):
    """Full-size integer pattern with non-eight-byte tails and true wrapping.

    Matches the Rust source byte for byte for every even 2..8192 geometry.
    """
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
        state, y = prng(state, w * h)
        _, uv = prng(state, w * h // 2)
        return bytes(y), bytes(uv)
    if pattern != "motion":
        return GENS[pattern](fid, w, h)
    y = bytearray(((x + fid * 3 + (yy >> 2)) & 255) for yy in range(h) for x in range(w))
    sprite = min(64, min(w, h) - 1)
    for k in range(4):
        px = ((fid * (5 + k * 2) + k * 173) & 0xffffffff) % (w - sprite)
        py = ((fid * (3 + k) + k * 311) & 0xffffffff) % (h - sprite)
        c = 60 + k * 50
        for yy in range(sprite):
            for x in range(sprite):
                y[(py + yy) * w + px + x] = 255 - c if (x ^ yy) & 8 else c
    noise = min(256, w, h)
    x0, y0 = (w - noise) // 2, (h - noise) // 2
    state = (fid << 32) ^ 0x4D4F54494F4E
    for yy in range(noise):
        state, row = prng(state, noise)
        y[(y0 + yy) * w + x0:(y0 + yy) * w + x0 + noise] = row
    uv = bytearray(w * h // 2)
    for yy in range(h // 2):
        for x in range(0, w, 2):
            uv[yy * w + x], uv[yy * w + x + 1] = (x + fid) & 255, (yy * 2 - fid) & 255
    return bytes(y), bytes(uv)


# Frozen pattern-contract vectors from the Rust source tests (the constants are
# part of the contract, not produced by this module).  These keep the oracle
# honest without needing a C++ compiler or any product binary.
GOLDEN_640_FRAME0 = {
    "counter": "7a14c003d9778b25",
    "checker": "795284f57bddc325",
    "rings": "8c955154249d36e5",
    "noise": "239d969142660385",
    "motion": "f6f53d2aeac3c9dc",
}
GOLDEN_MOTION_100X70_FRAME0 = "25ae678e1d721f60"
GOLDEN_MOTION_2X2_FRAME0 = "b6bb87fb336efe47"
DIRECT_MOTION_2X2 = (
    (0, bytes((0x8B, 0x14, 0x3D, 0x50)), bytes((0x00, 0x00))),
    (1, bytes((0x90, 0xA1, 0xEB, 0xD8)), bytes((0x01, 0xFF))),
)


def self_check(full=True):
    assert gen_noise_y(7) == gen_noise_y(7), "PRNG non-deterministic"
    n0, n1 = gen_noise_y(0), gen_noise_y(1)
    c0, c1 = gen_counter_y(0), gen_counter_y(1)
    ndiff = sum(abs(a - b) for a, b in zip(n0, n1)) / len(n0)
    cdiff = sum(abs(a - b) for a, b in zip(c0, c1)) / len(c0)
    assert ndiff > 50 and cdiff < 5, "complexity tiers broken"
    assert frame_hash(n0) != frame_hash(n1), "noise frames must differ"

    for pattern in PATS:
        for fid in (0, 1, 2, 3):
            Y1, UV1 = GENS[pattern](fid)
            Y2, UV2 = GENS[pattern](fid)
            assert Y1 == Y2 and UV1 == UV2, f"small determinism {pattern} fid={fid}"

    for fid, expected_y, expected_uv in DIRECT_MOTION_2X2:
        Y, UV = gen_native("motion", fid, 2, 2)
        assert (Y, UV) == (expected_y, expected_uv), f"2x2 motion fid={fid} bytes"

    cases = [
        ("motion", 0, 2, 2, GOLDEN_MOTION_2X2_FRAME0),
        ("motion", 0, 100, 70, GOLDEN_MOTION_100X70_FRAME0),
    ]
    if full:
        cases += [(pattern, 0, 640, 640, digest)
                  for pattern, digest in GOLDEN_640_FRAME0.items()]
    for pattern, fid, w, h, expected in cases:
        Y, UV = gen_native(pattern, fid, w, h)
        got = frame_hash_yuv(Y, UV)
        assert got == expected, f"{pattern} {w}x{h} fid={fid}: {got} != {expected}"

    for pattern in PATS:
        for Y, UV in (gen_native(pattern, 0, 418, 282),):
            assert len(Y) == 418 * 282 and len(UV) == 418 * 282 // 2
    return {
        "ok": True,
        "result": "PASS",
        "scope": "pure Python reference; no binaries or build outputs",
        "golden_640_frame0": {k: v for k, v in GOLDEN_640_FRAME0.items()} if full else None,
        "golden_motion_100x70_frame0": GOLDEN_MOTION_100X70_FRAME0,
        "golden_motion_2x2_frame0": GOLDEN_MOTION_2X2_FRAME0,
    }


if __name__ == "__main__":
    import json
    print(json.dumps(self_check(full=True)))
