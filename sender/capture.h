#pragma once
// 采集后端选择器：Raw/MJPEG 发送循环只依赖这层，DDA 与 WGC 二选一，绝不静默回退。
#include <cstdint>
#include <string>
#include "dda.h"
#include "wgc.h"

struct CaptureHandle {
  DDACap* dda = nullptr;
  WGCCap* wgc = nullptr;
  bool active() const { return dda || wgc; }
  bool capture(uint8_t* Y, uint8_t* UV) { return dda ? dda->capture(Y, UV) : (wgc ? wgc->capture(Y, UV) : false); }
  int err_code() const { return dda ? (int)dda->err_code : (wgc ? (int)wgc->err_code : 0); }
  const char* err() const { return dda ? dda->err() : (wgc ? wgc->err() : "no capture backend"); }
  uint64_t last_present_qpc() const { return dda ? dda->last_present_qpc : (wgc ? wgc->last_present_qpc : 0); }
  uint64_t last_convert_qpc() const { return dda ? dda->last_convert_qpc : (wgc ? wgc->last_convert_qpc : 0); }
  uint64_t source_skipped() const { return dda ? dda->source_skipped : (wgc ? wgc->source_skipped.load() : 0); }
  double gpu_accum_ms() const { return dda ? dda->gpu_accum_ms : (wgc ? wgc->gpu_accum_ms : 0); }
  uint64_t gpu_samples() const { return dda ? dda->gpu_samples : (wgc ? wgc->gpu_samples : 0); }
#ifdef _WIN32
  bool start_gpu(std::string& why) { return dda ? dda->start_gpu(why) : (wgc ? wgc->start_gpu(why) : false); }
#endif
};
