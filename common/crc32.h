// 包载荷CRC32（IEEE多项式 0xEDB88320），slice-by-8 查表（结果与逐字节查表逐位一致）
// 初值全1，输出异或全1。vc_crc32_ref 为逐字节参考实现，selftest 比对两者。
#pragma once
#include <stdint.h>
#include <stddef.h>
#include <string.h>

static uint32_t vc_crc32_table[8][256];
static int vc_crc32_table_ready = 0;

// 建表：只建一次（[0]=经典逐字节表，[k]=前进k字节）
static inline void vc_crc32_init(void) {
  if (vc_crc32_table_ready) return;
  for (uint32_t i = 0; i < 256; i++) {
    uint32_t c = i;
    for (int k = 0; k < 8; k++) c = (c & 1) ? (0xEDB88320u ^ (c >> 1)) : (c >> 1);
    vc_crc32_table[0][i] = c;
  }
  for (uint32_t i = 0; i < 256; i++)
    for (int t = 1; t < 8; t++)
      vc_crc32_table[t][i] = (vc_crc32_table[t - 1][i] >> 8) ^ vc_crc32_table[0][vc_crc32_table[t - 1][i] & 0xFF];
  vc_crc32_table_ready = 1;
}

// 逐字节参考实现（selftest 用）
static inline uint32_t vc_crc32_ref(const uint8_t *data, size_t len) {
  vc_crc32_init();
  uint32_t c = 0xFFFFFFFFu;
  for (size_t i = 0; i < len; i++) c = vc_crc32_table[0][(c ^ data[i]) & 0xFF] ^ (c >> 8);
  return c ^ 0xFFFFFFFFu;
}

// 算整段CRC（slice-by-8，小端）
static inline uint32_t vc_crc32(const uint8_t *data, size_t len) {
  vc_crc32_init();
  uint32_t c = 0xFFFFFFFFu;
  while (len >= 8) {
    uint32_t lo, hi;
    memcpy(&lo, data, 4); memcpy(&hi, data + 4, 4);
    lo ^= c;
    c = vc_crc32_table[7][lo & 0xFF] ^ vc_crc32_table[6][(lo >> 8) & 0xFF] ^
        vc_crc32_table[5][(lo >> 16) & 0xFF] ^ vc_crc32_table[4][lo >> 24] ^
        vc_crc32_table[3][hi & 0xFF] ^ vc_crc32_table[2][(hi >> 8) & 0xFF] ^
        vc_crc32_table[1][(hi >> 16) & 0xFF] ^ vc_crc32_table[0][hi >> 24];
    data += 8; len -= 8;
  }
  while (len--) c = vc_crc32_table[0][(c ^ *data++) & 0xFF] ^ (c >> 8);
  return c ^ 0xFFFFFFFFu;
}
