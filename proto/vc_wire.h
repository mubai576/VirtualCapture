// 包头唯一真相源（28B冻结，仅owner可改），收发共用
#pragma once
#include <stdint.h>

#define VC_ETHERTYPE 0x88B5 // 链路层协议号
#define VC_MAGIC 0x56434150u // 魔数'VCAP'
#define VC_FRAME_W 640 // 帧宽
#define VC_FRAME_H 640 // 帧高
#define VC_FRAME_BYTES (640*640*3/2) // 单帧614400字节NV12
#define VC_MAX_JUMBO 9000 // 最大MTU

#pragma pack(push,1)
typedef struct {
  uint32_t magic;       // 魔数，定值VCAP
  uint32_t frame_id;    // 帧号，递增
  uint16_t seq;         // 包序号0..total-1
  uint16_t total;       // 每帧总包数（9000约69）
  uint64_t qpc_ns;      // 发端戳，只算同机抖动
  uint32_t payload_crc; // 本包载荷CRC
  uint16_t payload_len; // 本包载荷长度
  uint8_t  flags;       // bit0关键帧，其余预留
  uint8_t  pattern;     // 测试源类型，见pattern.h（原reserved，仍28B）
} vc_pkt_hdr_t; // 共28B，尺寸顺序冻结
#pragma pack(pop)

// counter自检公式（收发共用）：
//   Y=(x+y+fid)&0xFF，UV=0x80^(fid&0xFF)
// 收端按帧号重算哈希，无原图可判错。
