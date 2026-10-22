// vc_wire.h — 唯一的包格式真相源，收发两端共用
#pragma once
#include <stdint.h>

#define VC_ETHERTYPE 0x88B5
#define VC_MAGIC 0x56434150u // 'VCAP'
#define VC_FRAME_W 640
#define VC_FRAME_H 640
#define VC_FRAME_BYTES (640*640*3/2) // 614400 NV12
#define VC_MAX_JUMBO 9000

#pragma pack(push,1)
typedef struct {
  uint32_t magic;       // VC_MAGIC
  uint32_t frame_id;    // 单调递增
  uint16_t seq;         // 本帧内包序号 0..total-1
  uint16_t total;       // 本帧总包数（~69 @9000）
  uint64_t qpc_ns;      // 发送端QPC时间戳（仅同机抖动分析，跨机不比绝对值）
  uint32_t payload_crc; // 本包payload CRC32
  uint16_t payload_len; // 本包有效载荷长度
  uint8_t  flags;       // bit0: key frame(pattern起点), 预留
  uint8_t  pattern;     // vc_pattern_t，见 sender/pattern.h（原reserved，仍28B）
} vc_pkt_hdr_t; // 28B（magic4+fid4+seq2+total2+qpc8+crc4+len2+flags1+pattern1）
#pragma pack(pop)

// pattern=counter 自校验规则（收发共用，MVP正确性之根）：
//   Y(x,y) = (x + y + frame_id) & 0xFF, U = V = 0x80 ^ (frame_id & 0xFF)
// PC2按frame_id重算期望NV12哈希，无需原图即可判 hash_mismatch。
