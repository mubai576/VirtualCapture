#include "../../../common/operation_deadline.h"
#include "../../../common/codec_session.h"
#include <cassert>
#include <stdexcept>
#include <string>

int main(int argc, char** argv) {
  using namespace std::chrono_literals;
  const std::string mode = argc > 1 ? argv[1] : "normal";
  VCOperationDeadline timer(100ms);
  if (mode == "hang") {
    VCOperationDeadline::Scope operation(timer);
    std::this_thread::sleep_for(10s);
    return 1;
  }
  // Idle time must not trigger the watchdog; successful/error paths must disarm.
  std::this_thread::sleep_for(150ms);
  for (int i = 0; i < 5; ++i) {
    { VCOperationDeadline::Scope operation(timer); std::this_thread::sleep_for(5ms); }
    std::this_thread::sleep_for(110ms);
    try {
      VCOperationDeadline::Scope operation(timer);
      throw std::runtime_error("operation failed");
    } catch (const std::runtime_error&) {}
  }
  std::this_thread::sleep_for(150ms);
  // Re-arming must replace, not inherit, the previous deadline.
  { VCOperationDeadline::Scope operation(timer); std::this_thread::sleep_for(70ms); }
  { VCOperationDeadline::Scope operation(timer); std::this_thread::sleep_for(70ms); }
  for (const auto* codec : {"mjpeg", "pyrowave"}) {
    VCCodec options;
    options.codec = codec;
    options.backend = options.mjpeg() ? "turbojpeg:cpu" : "pyrowave:vulkan";
    options.bitrate = 50000000;
    options.max_bitrate = 20000000;
    if (!options.valid(9000)) return 2;
    options.max_bitrate = 0;
    if (options.valid(9000)) return 3;
  }
  return 0;
}
