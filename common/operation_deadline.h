#pragma once
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <mutex>
#include <thread>

// A hung driver call cannot be unwound safely: neither destroying its GPU objects
// nor joining the blocked worker is bounded. End only this native process with a
// distinct failure status, without stack unwinding or potentially blocking I/O.
class VCOperationDeadline {
  std::mutex mutex;
  std::condition_variable cv;
  bool stopping = false, armed = false;
  unsigned long long generation = 0;
  std::chrono::steady_clock::time_point deadline;
  const std::chrono::milliseconds timeout;
  std::thread worker;

  void watch() {
    std::unique_lock<std::mutex> lock(mutex);
    while (!stopping) {
      cv.wait(lock, [this] { return stopping || armed; });
      if (stopping) break;
      const auto current = generation;
      const auto until = deadline;
      if (!cv.wait_until(lock, until, [this, current] {
            return stopping || !armed || generation != current;
          })) {
        // 124 = codec operation timeout; no cleanup can safely run on stuck GPU work.
        std::_Exit(124);
      }
    }
  }
  void arm() {
    std::lock_guard<std::mutex> lock(mutex);
    deadline = std::chrono::steady_clock::now() + timeout;
    ++generation;
    armed = true;
    // MJPEG backends also contain an unopened PyroWave wrapper; allocate no
    // watchdog thread until a real PyroWave operation starts.
    if (!worker.joinable()) worker = std::thread([this] { watch(); });
    cv.notify_one();
  }
  void disarm() {
    std::lock_guard<std::mutex> lock(mutex);
    armed = false;
    ++generation;
    cv.notify_one();
  }
 public:
  explicit VCOperationDeadline(std::chrono::milliseconds limit = std::chrono::milliseconds(5000))
      : timeout(limit) {}
  VCOperationDeadline(const VCOperationDeadline&) = delete;
  VCOperationDeadline& operator=(const VCOperationDeadline&) = delete;
  ~VCOperationDeadline() {
    { std::lock_guard<std::mutex> lock(mutex); stopping = true; cv.notify_one(); }
    if (worker.joinable()) worker.join();
  }
  class Scope {
    VCOperationDeadline& owner;
   public:
    explicit Scope(VCOperationDeadline& value) : owner(value) { owner.arm(); }
    Scope(const Scope&) = delete;
    Scope& operator=(const Scope&) = delete;
    ~Scope() { owner.disarm(); }
  };
};
