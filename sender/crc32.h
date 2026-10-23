// 包载荷CRC32（IEEE多项式），查表法
// 初值全1，输出异或全1
#pragma once
#include <stdint.h>
#include <stddef.h>

static uint32_t vc_crc32_table[256];
static int vc_crc32_table_ready = 0;

// 建表：只建一次
static inline void vc_crc32_init(void) {
  if (vc_crc32_table_ready) return;
  for (uint32_t i = 0; i < 256; i++) {
    uint32_t c = i;
    for (int k = 0; k < 8; k++) c = (c & 1) ? (0xEDB88320u ^ (c >> 1)) : (c >> 1);
    vc_crc32_table[i] = c;
  }
  vc_crc32_table_ready = 1;
}

// 算整段CRC
static inline uint32_t vc_crc32(const uint8_t *data, size_t len) {
  vc_crc32_init();
  uint32_t c = 0xFFFFFFFFu;
  for (size_t i = 0; i < len; i++) c = vc_crc32_table[(c ^ data[i]) & 0xFF] ^ (c >> 8);
  return c ^ 0xFFFFFFFFu;
}
