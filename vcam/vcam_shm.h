#pragma once
// vcam_shm.h -- receiver -> vcam frame handoff, layout v2 (2026-10-28, latency work).
// Canonical layout; every reader/writer includes or mirrors this file (NOT proto/: proto/ is owner-frozen).
// Mirrors: monitor/monitor.cpp, scripts/p4_4_shm_verify.py, scripts/p4_4_vcam_verify.py.
//
// v2 = multi-slot ring + in-place reassembly + event wake (replaces v1 single-slot copy + polling).
//   [0,64)                      vc_shm_hdr_t
//   [64, 64+64*VC_SHM_SLOTS)    vc_shm_slot_t[VC_SHM_SLOTS]
//   [VC_SHM_DATA_OFF + i*VC_FRAME_BYTES)  slot i frame bytes (NV12 614400, 4K aligned)
//
// Writer (receiver, single process):
//   - reassembles packets DIRECTLY into a free slot: slot.seq++ (odd = filling), hash_fid = ~0.
//   - never fills the slot named by hdr.latest_slot.
//   - complete: fill frame_id/qpc_*; slot.seq++ (even = ready); hdr.latest_slot = i (store first);
//     n = ++hdr.latest_seq ; ResetEvent(evt[(n+1)&1]); SetEvent(evt[n&1]).
//   - free-slot choice is LRU among slots that are not filling / not latest / not being hashed,
//     so a reader still copying the previous latest has several frame times before reuse.
//   - AFTER publish (off the latency path): hash computed, slot.hash written, then slot.hash_fid = frame_id.
// Reader (vcam / monitor / verifiers), read-only view, no LOCK-prefixed ops (D11):
//   ls = latest_seq; i = latest_slot; s1 = slot[i].seq (skip if odd); copy data; s2 = slot[i].seq;
//   accept iff s1 == s2. A publish is identified by the token (i, s1) -- dedupe on it, not on ls.
//   slot.hash is valid for that copy only iff hash_fid == frame_id (read inside s1..s2).
//   Waiting for the publish after ls: WaitForSingleObject(evt[(ls+1)&1], timeout) -- manual-reset pair,
//   safe for multiple readers; a missed edge (two publishes inside one wake-up) is bounded by the timeout.
// Event names: "<mapping name>.evt0" / "<mapping name>.evt1" (manual-reset, created by writer).
// Mapping name (default): Local\VirtualCaptureVcamFrame (session-local, no admin).
#include <stdint.h>
#include "../sender/pattern.h"  // VC_Y_BYTES / VC_UV_BYTES
#include "../proto/vc_wire.h"    // VC_FRAME_BYTES (614400)

#define VC_SHM_MAGIC    0x32414356u   /* "VCA2" little-endian; v1 was 0x5643414d */
#define VC_SHM_VERSION  2u
#define VC_SHM_SLOTS    12u
#define VC_SHM_DATA_OFF 4096u
#define VC_SHM_SIZE     ((size_t)VC_SHM_DATA_OFF + (size_t)VC_SHM_SLOTS * (size_t)VC_FRAME_BYTES)
#define VC_SHM_HASH_NONE 0xFFFFFFFFu

#pragma pack(push,1)
typedef struct {
  uint32_t magic;          // VC_SHM_MAGIC
  uint32_t version;        // VC_SHM_VERSION
  uint32_t slots;          // VC_SHM_SLOTS
  uint32_t frame_bytes;    // VC_FRAME_BYTES
  uint32_t data_off;       // VC_SHM_DATA_OFF
  int32_t  latest_slot;    // -1 = nothing published yet
  int32_t  latest_seq;     // publish counter (0 = none), monotonically increasing
  uint32_t writer_pid;
  uint8_t  reserved[32];
} vc_shm_hdr_t;            // 64 B
typedef struct {
  int32_t  seq;            // odd = filling, even = stable
  uint32_t frame_id;
  uint64_t qpc_first_ns;   // receiver QPC: first packet of this frame processed
  uint64_t qpc_pub_ns;     // receiver QPC: published (same clock as vcam on PC2)
  uint64_t hash;           // FNV-1a64 of frame, valid iff hash_fid == frame_id
  uint32_t hash_fid;
  uint8_t  reserved[28];
} vc_shm_slot_t;           // 64 B
#pragma pack(pop)

#ifdef __cplusplus
static_assert(sizeof(vc_shm_hdr_t) == 64, "vc_shm_hdr_t");
static_assert(sizeof(vc_shm_slot_t) == 64, "vc_shm_slot_t");
static_assert(64 + 64 * VC_SHM_SLOTS <= VC_SHM_DATA_OFF, "slot table overflows data offset");
#endif
static inline vc_shm_slot_t* vc_shm_slot(uint8_t* base, uint32_t i) {
  return (vc_shm_slot_t*)(base + 64 + 64 * (size_t)i);
}
static inline uint8_t* vc_shm_data(uint8_t* base, uint32_t i) {
  return base + VC_SHM_DATA_OFF + (size_t)i * (size_t)VC_FRAME_BYTES;
}

// ---- vcam -> receiver feedback ring (latency measurement only, same machine QPC) ----
// Mapping "<frame mapping name>.vcam", created/written by vcam (one or more instances), read by receiver.
// Writer: k = InterlockedIncrement(&widx) - 1; e = &ring[k % cap]; e->n = 0; fill; e->n = k + 1 (last).
// Reader: accept entry k iff e->n == k + 1 before and after reading the fields.
// vseg = deliver_ns - pub_ns = receiver publish -> vcam hands the sample downstream (IMemInputPin::Receive).
#define VC_VSTAT_MAGIC 0x54535656u   /* "VVST" */
#define VC_VSTAT_CAP   1024u
#define VC_VSTAT_FLAG_REPEAT 1u      /* re-delivery of an already delivered publish (stall) */
#pragma pack(push,1)
typedef struct {
  uint32_t magic;
  int32_t  widx;           // total entries ever written (LONG, Interlocked by writers)
  uint32_t cap;            // VC_VSTAT_CAP
  uint32_t reserved;
} vc_vstat_hdr_t;          // 16 B
typedef struct {
  uint32_t n;              // k + 1 when entry k is complete
  uint32_t frame_id;
  uint64_t pub_ns;         // slot.qpc_pub_ns of the delivered frame
  uint64_t deliver_ns;     // vcam QPC ns right before Receive()
  uint32_t flags;          // VC_VSTAT_FLAG_*
  uint32_t reserved;
} vc_vstat_entry_t;        // 32 B
#pragma pack(pop)
#define VC_VSTAT_SIZE (sizeof(vc_vstat_hdr_t) + (size_t)VC_VSTAT_CAP * sizeof(vc_vstat_entry_t))

// QPC -> ns without the c*1e9 overflow (c*1e9 wraps after ~30 min uptime at 10 MHz).
static inline uint64_t vc_qpc_to_ns(uint64_t c, uint64_t freq) {
  return (c / freq) * 1000000000ull + (c % freq) * 1000000000ull / freq;
}
