// 测试源（生成/校验/python镜像三端一致）
// 约束：纯整数，同帧号必同帧，主校验靠哈希对账
// 纯整数，三端bit一致，不用浮点
// 同帧号必同帧，可重放，不传参考帧
// 主校验靠哈希对账，重算只兜底

#pragma once
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#define VC_PW 640
#define VC_PH 640
#define VC_Y_BYTES (640*640)
#define VC_UV_BYTES (640*640/2)

typedef enum : uint8_t {
  VC_PAT_COUNTER   = 0, // 烟雾：低渐变
  VC_PAT_CHECKER   = 1, // 棋盘：滚1px查色度错
  VC_PAT_RINGS     = 2, // 同心环：高频运动
  VC_PAT_NOISE     = 3, // 噪声：防压缩作弊
  VC_PAT_MOTION    = 4, // 拟游戏：默认压力源
} vc_pattern_t;

// 随机数：splitmix64，够240Hz用
static inline uint64_t vc_splitmix64(uint64_t *s) {
  uint64_t z = (*s += 0x9E3779B97F4A7C15ull);
  z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ull;
  z = (z ^ (z >> 27)) * 0x94D049BB133111EBull;
  return z ^ (z >> 31);
}

// 生成NV12
// 烟雾渐变帧
static inline void vc_gen_counter(uint32_t fid, uint8_t *Y, uint8_t *UV) {
  for (int y = 0; y < VC_PH; y++)
    for (int x = 0; x < VC_PW; x++)
      Y[y*VC_PW+x] = (uint8_t)(x + y + fid);
  uint8_t v = (uint8_t)(0x80 ^ (fid & 0xFF));
  for (size_t i = 0; i < VC_UV_BYTES; i++) UV[i] = v;
}

// 棋盘滚动帧
static inline void vc_gen_checker(uint32_t fid, uint8_t *Y, uint8_t *UV) {
  int ox = fid & 7, oy = (fid >> 3) & 7; // 每帧滚，保证DDA脏矩形=全帧
  for (int y = 0; y < VC_PH; y++)
    for (int x = 0; x < VC_PW; x++)
      Y[y*VC_PW+x] = ((((x+ox)>>3) + ((y+oy)>>3)) & 1) ? 255 : 0;
  for (int y = 0; y < VC_PH/2; y++)
    for (int x = 0; x < VC_PW; x += 2) {
      uint8_t u = ((((x>>3)+((y+fid)&1)) & 1)) ? 16 : 240;
      uint8_t v = ((((x>>3)+((y-fid)&1)) & 1)) ? 16 : 240;
      UV[(y*VC_PW+x)] = u; UV[(y*VC_PW+x)+1] = v;
    }
}

// 同心环帧
static inline void vc_gen_rings(uint32_t fid, uint8_t *Y, uint8_t *UV) {
  // 环：r2=(x-cx)^2+(y-cy)^2，Y=(r2>>4)+fid*7 锯齿，高频验证下采样混叠是否符合预期
  const int cx = VC_PW/2, cy = VC_PH/2;
  for (int y = 0; y < VC_PH; y++) {
    int dy = y - cy;
    for (int x = 0; x < VC_PW; x++) {
      int dx = x - cx;
      uint32_t r2 = (uint32_t)(dx*dx + dy*dy);
      Y[y*VC_PW+x] = (uint8_t)((r2 >> 4) + fid * 7 + ((x ^ y) & 15));
    }
  }
  for (int y = 0; y < VC_PH/2; y++)
    for (int x = 0; x < VC_PW; x += 2) {
      UV[y*VC_PW+x]   = (uint8_t)(128 + (((x*3 + y*5 + fid*11) >> 2) & 127) - 64);
      UV[y*VC_PW+x+1] = (uint8_t)(128 + (((x*5 - y*3 + fid*13) >> 2) & 127) - 64);
    }
}

static inline void vc_gen_noise(uint32_t fid, uint8_t *Y, uint8_t *UV) {
  uint64_t s = ((uint64_t)fid << 32) | 0x123456789ABCull;
  uint8_t *p = Y; size_t n = VC_Y_BYTES;
  while (n >= 8) { uint64_t r = vc_splitmix64(&s); memcpy(p, &r, 8); p += 8; n -= 8; }
  while (n--) *p++ = (uint8_t)vc_splitmix64(&s);
  p = UV; n = VC_UV_BYTES;
  while (n >= 8) { uint64_t r = vc_splitmix64(&s); memcpy(p, &r, 8); p += 8; n -= 8; }
  while (n--) *p++ = (uint8_t)vc_splitmix64(&s);
}

// 拟游戏压力帧
static inline void vc_gen_motion(uint32_t fid, uint8_t *Y, uint8_t *UV) {
  // 背景：逐帧横滚3px渐变（杀DDA静态优化）+ 4弹跳色块 + 中央256x256噪声窗
  for (int y = 0; y < VC_PH; y++)
    for (int x = 0; x < VC_PW; x++)
      Y[y*VC_PW+x] = (uint8_t)(x + fid * 3 + (y >> 2));
  // 4块64弹跳
  const int S = 64;
  for (int k = 0; k < 4; k++) {
    int px = (int)((fid * (5 + k * 2) + k * 173) % (VC_PW - S));
    int py = (int)((fid * (3 + k) + k * 311) % (VC_PH - S));
    uint8_t c = (uint8_t)(60 + k * 50);
    for (int y = 0; y < S; y++)
      for (int x = 0; x < S; x++)
        Y[(py+y)*VC_PW + (px+x)] = ((x ^ y) & 8) ? (uint8_t)(255 - c) : c;
  }
  // 中央噪声窗 256x256（Y），其余UV滚动画
  {
    uint64_t s = ((uint64_t)fid << 32) ^ 0x4D4F54494F4EULL; // 'MOTION'
    const int W2 = 256, x0 = (VC_PW - W2) / 2, y0 = (VC_PH - W2) / 2;
    for (int y = 0; y < W2; y++) {
      uint8_t *row = &Y[(y0 + y) * VC_PW + x0];
      size_t n = W2;
      while (n >= 8) { uint64_t r = vc_splitmix64(&s); memcpy(row, &r, 8); row += 8; n -= 8; }
      while (n--) *row++ = (uint8_t)vc_splitmix64(&s);
    }
  }
  for (int y = 0; y < VC_PH / 2; y++)
    for (int x = 0; x < VC_PW; x += 2) {
      UV[y * VC_PW + x]     = (uint8_t)(x + fid);
      UV[y * VC_PW + x + 1] = (uint8_t)(y * 2 - fid);
    }
}
