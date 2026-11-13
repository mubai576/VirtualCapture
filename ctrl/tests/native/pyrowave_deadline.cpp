#define VC_HAS_PYROWAVE 1
#include "../../../common/pyrowave_codec.h"
#include <chrono>
#include <thread>
#include <string>

static std::string blocked;
static void stage(const char* name) {
  if (blocked == name) {
    fprintf(stderr, "injected blocked %s\n", name); fflush(stderr);
    std::this_thread::sleep_for(std::chrono::seconds(30));
  }
}
extern "C" {
pyrowave_result pyrowave_create_default_device(pyrowave_device* d) {
  stage("create"); *d = reinterpret_cast<pyrowave_device>(1); return PYROWAVE_SUCCESS;
}
void pyrowave_device_destroy(pyrowave_device) { stage("destroy"); }
pyrowave_result pyrowave_encoder_create(const pyrowave_encoder_create_info*, pyrowave_encoder* e) {
  *e = reinterpret_cast<pyrowave_encoder>(1); return PYROWAVE_SUCCESS;
}
void pyrowave_encoder_destroy(pyrowave_encoder) {}
pyrowave_result pyrowave_decoder_create(const pyrowave_decoder_create_info*, pyrowave_decoder* d) {
  *d = reinterpret_cast<pyrowave_decoder>(1); return PYROWAVE_SUCCESS;
}
void pyrowave_decoder_destroy(pyrowave_decoder) { stage("reset"); }
pyrowave_result pyrowave_encoder_encode_cpu(pyrowave_encoder, const pyrowave_cpu_buffer*, const pyrowave_rate_control*) {
  stage("encode"); return PYROWAVE_SUCCESS;
}
pyrowave_result pyrowave_encoder_compute_num_packets(pyrowave_encoder, size_t, size_t* count) {
  stage("packets"); *count = 1; return PYROWAVE_SUCCESS;
}
pyrowave_result pyrowave_encoder_packetize(pyrowave_encoder, pyrowave_packet* packet, size_t, size_t* count, void*, size_t) {
  *count = 1; packet->offset = 0; packet->size = 16; return PYROWAVE_SUCCESS;
}
pyrowave_result pyrowave_decoder_push_packet(pyrowave_decoder, const void*, size_t) { return PYROWAVE_SUCCESS; }
bool pyrowave_decoder_decode_is_ready(pyrowave_decoder, bool) { return true; }
pyrowave_result pyrowave_decoder_decode_cpu_buffer_synchronous(pyrowave_decoder, const pyrowave_cpu_buffer*) {
  stage("decode"); return PYROWAVE_SUCCESS;
}
}
int main(int argc, char** argv) {
  blocked = argc > 1 ? argv[1] : "normal";
  VCStream stream;
  vcpyro::Codec codec;
  bool decode = blocked == "decode" || blocked == "reset";
  codec.open(stream, !decode, 400000);
  std::vector<uint8_t> frame(stream.frame_bytes());
  if (blocked == "reset") codec.reset_decoder();
  else if (decode) codec.decode(frame.data(), 16, frame.data(), frame.size());
  else { size_t size = 0; codec.encode(frame.data(), frame.size(), size); }
  codec.close();
  return blocked == "normal" ? 0 : 1;
}
