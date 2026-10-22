#pragma once
// fnv1a.h — FNV-1a 64-bit, PLAN §2.3 frozen.
// init 14695981039346656037 (0xcbf29ce484222325), prime 1099511628211 (0x100000001b3)
// per byte: h ^= b; h *= prime. Output lowercase 16 hex, zero-padded.
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>

static inline uint64_t vc_fnv1a64(const uint8_t *data, size_t len) {
  uint64_t h = 14695981039346656037ull;
  for (size_t i = 0; i < len; i++) {
    h ^= (uint64_t)data[i];
    h *= 1099511628211ull;
  }
  return h;
}

// two-part (Y + UV) without concatenating
static inline uint64_t vc_fnv1a64_two(const uint8_t *a, size_t na, const uint8_t *b, size_t nb) {
  uint64_t h = 14695981039346656037ull;
  for (size_t i = 0; i < na; i++) { h ^= (uint64_t)a[i]; h *= 1099511628211ull; }
  for (size_t i = 0; i < nb; i++) { h ^= (uint64_t)b[i]; h *= 1099511628211ull; }
  return h;
}

static inline void vc_fnv1a64_hex(uint64_t h, char out16[17]) {
  static const char *dig = "0123456789abcdef";
  for (int i = 15; i >= 0; i--) { out16[i] = dig[h & 0xF]; h >>= 4; }
  out16[16] = 0;
}
