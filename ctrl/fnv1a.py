"""fnv1a.py - FNV-1a 64-bit Python reference (PLAN 2.3 frozen)."""
MASK = 0xFFFFFFFFFFFFFFFF
INIT = 14695981039346656037
PRIME = 1099511628211

def fnv1a64(data: bytes) -> int:
    h = INIT
    for b in data:
        h ^= b
        h = (h * PRIME) & MASK
    return h

def fnv1a64_hex(data: bytes) -> str:
    return "%016x" % fnv1a64(data)

if __name__ == "__main__":
    assert fnv1a64(b"") == INIT, "empty vector failed"
    assert fnv1a64(b"a") == 0xAF63DC4C8601EC8C, "single-a failed"
    assert fnv1a64_hex(b"hello") == fnv1a64_hex(b"hello")
    assert fnv1a64(b"hello") != fnv1a64(b"helln")
    print("PASS: fnv1a vectors ok (empty/a/determinism)")
