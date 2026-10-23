// 整帧哈希FNV-1a64（PLAN §1.4冻结）
// 每字节异或后乘质数；输出小写16位hex
#pragma once
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>

// 算单段哈希
static inline uint64_t vc_fnv1a64(const uint8_t *data, size_t len) {
  uint64_t h = 14695981039346656037ull; // 初值
  for (size_t i = 0; i < len; i++) {
    h ^= (uint64_t)data[i];
    h *= 1099511628211ull; // 质数
  }
  return h;
}

// 算Y+UV两段拼接哈希（免拷贝）
static inline uint64_t vc_fnv1a64_two(const uint8_t *a, size_t na, const uint8_t *b, size_t nb) {
  uint64_t h = 14695981039346656037ull;
  for (size_t i = 0; i < na; i++) { h ^= (uint64_t)a[i]; h *= 1099511628211ull; }
  for (size_t i = 0; i < nb; i++) { h ^= (uint64_t)b[i]; h *= 1099511628211ull; }
  return h;
}

// 转16位小写hex
static inline void vc_fnv1a64_hex(uint64_t h, char out16[17]) {
  static const char *dig = "0123456789abcdef";
  for (int i = 15; i >= 0; i--) { out16[i] = dig[h & 0xF]; h >>= 4; }
  out16[16] = 0;
}
