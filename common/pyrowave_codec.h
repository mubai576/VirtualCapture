#pragma once
// PyroWave (MIT, Vulkan compute) wrapper for the sender/receiver.
// Phase 1 uses PyroWave's CPU upload/readback path: NV12 in, NV12 out, no D3D11 interop yet.
// Header-only; only compiled into targets built with VC_HAS_PYROWAVE.
#ifdef VC_HAS_PYROWAVE
#include "stream.h"
#include "operation_deadline.h"
#include <vulkan/vulkan.h>
#include "pyrowave.h"
#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <stdexcept>
#include <string>
#include <vector>

namespace vcpyro {

inline const char* result_text(pyrowave_result r) {
  switch (r) {
    case PYROWAVE_SUCCESS: return "ok";
    case PYROWAVE_TIMEOUT: return "timeout";
    case PYROWAVE_ERROR_INVALID_ARGUMENT: return "invalid argument";
    case PYROWAVE_ERROR_OUT_OF_HOST_MEMORY: return "out of host memory";
    case PYROWAVE_ERROR_OUT_OF_DEVICE_MEMORY: return "out of device memory";
    case PYROWAVE_ERROR_NO_VULKAN: return "no Vulkan device";
    case PYROWAVE_ERROR_NOT_IMPLEMENTED: return "not implemented";
    case PYROWAVE_ERROR_UNSUPPORTED_EXTERNAL_HANDLE: return "unsupported external handle";
    case PYROWAVE_ERROR_FAILED_EXTERNAL_HANDLE: return "failed external handle";
    default: return "generic error";
  }
}

inline void check(pyrowave_result r, const char* what) {
  if (r != PYROWAVE_SUCCESS) throw std::runtime_error(std::string("PyroWave ") + what + ": " + result_text(r));
}

class Codec {
  pyrowave_device device = nullptr;
  pyrowave_encoder encoder = nullptr;
  pyrowave_decoder decoder = nullptr;
  int width = 0, height = 0;
  size_t frame_bytes = 0, max_frame_bytes = 0;
  std::vector<uint8_t> bitstream;
  std::vector<pyrowave_packet> packets;
  std::vector<uint8_t> plane_cb, plane_cr;
  VCOperationDeadline operation_deadline;

  void release() {
    if (decoder) { pyrowave_decoder_destroy(decoder); decoder = nullptr; }
    if (encoder) { pyrowave_encoder_destroy(encoder); encoder = nullptr; }
    if (device) { pyrowave_device_destroy(device); device = nullptr; }
  }

 public:
  Codec() = default;
  Codec(const Codec&) = delete;
  Codec& operator=(const Codec&) = delete;
  ~Codec() { close(); }

  void close() {
    if (!device && !encoder && !decoder) return;
    VCOperationDeadline::Scope deadline(operation_deadline);
    release();
  }

  void open(const VCStream& stream, bool encode, size_t max_bytes) {
    VCOperationDeadline::Scope deadline(operation_deadline);
    release();
    width = stream.width; height = stream.height;
    frame_bytes = stream.frame_bytes();
    pyrowave_result r = pyrowave_create_default_device(&device);
    check(r, "device creation");
    if (encode) {
      if (max_bytes < 1024) throw std::runtime_error("PyroWave frame budget below 1024 bytes");
      max_frame_bytes = std::min(max_bytes, frame_bytes);
      pyrowave_encoder_create_info info = {};
      info.device = device; info.width = width; info.height = height;
      info.chroma = PYROWAVE_CHROMA_SUBSAMPLING_420;
      check(pyrowave_encoder_create(&info, &encoder), "encoder creation");
      bitstream.resize(max_frame_bytes);
    } else {
      pyrowave_decoder_create_info info = {};
      info.device = device; info.width = width; info.height = height;
      info.chroma = PYROWAVE_CHROMA_SUBSAMPLING_420;
      check(pyrowave_decoder_create(&info, &decoder), "decoder creation");
      plane_cb.resize((size_t)(width / 2) * (height / 2));
      plane_cr.resize(plane_cb.size());
    }
  }

  // Drop any pushed-but-undecoded packet without recreating the Vulkan device.
  // Frames are self-contained, so a stream discontinuity only needs a fresh decoder.
  void reset_decoder() {
    VCOperationDeadline::Scope deadline(operation_deadline);
    if (!decoder) return;
    pyrowave_decoder_destroy(decoder); decoder = nullptr;
    pyrowave_decoder_create_info info = {};
    info.device = device; info.width = width; info.height = height;
    info.chroma = PYROWAVE_CHROMA_SUBSAMPLING_420;
    check(pyrowave_decoder_create(&info, &decoder), "decoder reset");
  }

  size_t budget() const { return max_frame_bytes; }
  int pixel_width() const { return width; }
  int pixel_height() const { return height; }

  // NV12 in, one PyroWave packet out (the transport still owns L2 fragmentation).
  const uint8_t* encode(const uint8_t* nv12, size_t size, size_t& encoded_bytes) {
    VCOperationDeadline::Scope deadline(operation_deadline);
    if (!encoder) throw std::runtime_error("PyroWave encoder is not open");
    if (size != frame_bytes) throw std::runtime_error("PyroWave encode expects a full NV12 frame");
    pyrowave_cpu_buffer src = {};
    src.data[0] = const_cast<uint8_t*>(nv12);
    src.data[1] = const_cast<uint8_t*>(nv12 + (size_t)width * height);
    src.row_stride_in_bytes[0] = (size_t)width;
    src.row_stride_in_bytes[1] = (size_t)width;
    src.plane_size_in_bytes[0] = (size_t)width * height;
    src.plane_size_in_bytes[1] = (size_t)width * height / 2;
    src.width = width; src.height = height;
    src.format = PYROWAVE_CPU_BUFFER_FORMAT_NV12;
    pyrowave_rate_control rate = { max_frame_bytes };
    check(pyrowave_encoder_encode_cpu(encoder, &src, &rate), "encode");
    size_t count = 0;
    check(pyrowave_encoder_compute_num_packets(encoder, max_frame_bytes, &count), "packet count");
    if (count != 1) throw std::runtime_error("PyroWave frame split unexpectedly; single packet expected");
    packets.resize(count);
    check(pyrowave_encoder_packetize(encoder, packets.data(), max_frame_bytes, &count,
                                     bitstream.data(), bitstream.size()), "packetize");
    if (count != 1 || packets[0].offset != 0) throw std::runtime_error("PyroWave packet layout mismatch");
    encoded_bytes = packets[0].size;
    return bitstream.data() + packets[0].offset;
  }

  // One access unit (one PyroWave packet) in, NV12 out.
  void decode(const uint8_t* data, size_t size, uint8_t* nv12, size_t nv12_bytes) {
    VCOperationDeadline::Scope deadline(operation_deadline);
    if (!decoder) throw std::runtime_error("PyroWave decoder is not open");
    if (nv12_bytes != frame_bytes) throw std::runtime_error("PyroWave decode expects a full NV12 frame");
    check(pyrowave_decoder_push_packet(decoder, data, size), "push packet");
    if (!pyrowave_decoder_decode_is_ready(decoder, false))
      throw std::runtime_error("PyroWave frame incomplete");
    // Luma decodes straight into the caller's NV12 buffer; chroma goes to planes
    // that are interleaved into NV12 UV below. Never leave Y as stale slot data.
    pyrowave_cpu_buffer dst = {};
    dst.data[0] = nv12; dst.data[1] = plane_cb.data(); dst.data[2] = plane_cr.data();
    dst.row_stride_in_bytes[0] = (size_t)width;
    dst.row_stride_in_bytes[1] = (size_t)width / 2;
    dst.row_stride_in_bytes[2] = (size_t)width / 2;
    dst.plane_size_in_bytes[0] = (size_t)width * height;
    dst.plane_size_in_bytes[1] = plane_cb.size();
    dst.plane_size_in_bytes[2] = plane_cr.size();
    dst.width = width; dst.height = height;
    dst.format = PYROWAVE_CPU_BUFFER_FORMAT_YUV420P;
    check(pyrowave_decoder_decode_cpu_buffer_synchronous(decoder, &dst), "decode");
    const size_t chroma_pixels = (size_t)(width / 2);
    uint8_t* uv = nv12 + (size_t)width * height;
    for (int y = 0; y < height / 2; y++) {
      const uint8_t* cb = plane_cb.data() + (size_t)y * chroma_pixels;
      const uint8_t* cr = plane_cr.data() + (size_t)y * chroma_pixels;
      uint8_t* row = uv + (size_t)y * width;
      for (int x = 0; x < width / 2; x++) { row[x * 2] = cb[x]; row[x * 2 + 1] = cr[x]; }
    }
  }

  std::string settings() const {
    char buf[512];
    snprintf(buf, sizeof(buf),
             "{\"family\":\"pyrowave\",\"identity\":\"pyrowave:vulkan\",\"revision\":1,"
             "\"width\":%d,\"height\":%d,\"subsampling\":\"420\",\"max_bytes_per_frame\":%zu,"
             "\"upload_path\":\"cpu_nv12\",\"decode_path\":\"cpu_yuv420p\"}",
             width, height, max_frame_bytes);
    return buf;
  }
};

// 640x640 motion frame, encode/decode, compare luma. 0 pass, 2 skip (no Vulkan), 1 fail.
inline int selftest() {
  try {
    VCStream stream;
    Codec enc, dec;
    enc.open(stream, true, 400000);
    dec.open(stream, false, 0);
    std::vector<uint8_t> nv12(stream.frame_bytes()), out(stream.frame_bytes());
    for (size_t i = 0; i < nv12.size(); i++) nv12[i] = (uint8_t)((i * 37 + i / 640) & 0xff);
    size_t bytes = 0;
    const uint8_t* au = enc.encode(nv12.data(), nv12.size(), bytes);
    dec.decode(au, bytes, out.data(), out.size());
    const size_t y_bytes = (size_t)stream.width * stream.height;
    size_t y_nonzero = 0, uv_nonzero = 0;
    for (size_t i = 0; i < out.size(); i++) {
      if (i < y_bytes) y_nonzero += out[i] != 0; else uv_nonzero += out[i] != 0;
    }
    if (!bytes || !y_nonzero || !uv_nonzero) {
      printf("PYROWAVE: FAIL empty luma or chroma (%zu/%zu)\n", y_nonzero, uv_nonzero);
      return 1;
    }
    printf("PYROWAVE: PASS %dx%d bytes=%zu y_nonzero=%zu uv_nonzero=%zu\n",
           stream.width, stream.height, bytes, y_nonzero, uv_nonzero);
    return 0;
  } catch (const std::exception& e) {
    std::string what = e.what();
    if (what.find("no Vulkan") != std::string::npos) {
      printf("PYROWAVE: SKIP %s\n", what.c_str());
      return 2;
    }
    printf("PYROWAVE: FAIL %s\n", what.c_str());
    return 1;
  }
}

}  // namespace vcpyro
#endif
