#pragma once
// vcam_shm.h -- receiver -> vcam latest-1-frame handoff (P4-3/P4-4).
// Canonical layout; both sides include this file (NOT proto/: proto/ is owner-frozen).
// Single slot, overwrite ("only latest frame"): writer seq++ (odd = writing) -> copy ->
// seq++ (even = ready). Reader: s1=seq (skip if odd) -> copy header+payload ->
// s2=seq, accept iff s1==s2 && !(s1&1).
// Mapping name (canonical narrow form): Local\VirtualCaptureVcamFrame
// (session-local namespace: no admin, no Global\ privilege).
// Total mapping size = sizeof(vc_shm_hdr_t) + VC_FRAME_BYTES (614400 NV12).
#include <stdint.h>
#include "../sender/pattern.h"  // VC_Y_BYTES / VC_UV_BYTES
#include "../proto/vc_wire.h"    // VC_FRAME_BYTES (614400)

#define VC_SHM_MAGIC 0x5643414du
#define VC_SHM_SIZE (sizeof(vc_shm_hdr_t) + (size_t)VC_FRAME_BYTES)

#pragma pack(push,1)
typedef struct {
  uint32_t magic;
  int32_t  seq;
  uint32_t frame_id;
  uint64_t hash;
  uint32_t size;
} vc_shm_hdr_t;
#pragma pack(pop)
