// Optional live WGC resize/close driver. Spawns ctrl/tests/native/wgc_live_window.exe
// (a separate process, so WGC's own-process filter does not hide it), captures the
// window through WGCCap, resizes it with SetWindowPos, then closes it.
//
// Exit codes: 0 = PASS, 1 = FAIL, 2 = SKIP (no interactive desktop / capture
// unavailable), 3 = SKIP GPU (hardware path unavailable). No registry, network,
// virtual camera, or deployed capture is touched.
#include <windows.h>
#include <chrono>
#include <cstdio>
#include <string>
#include <thread>
#include <vector>

#include "wgc.h"
#include "stream.h"

static int g_failures = 0;
#define CHECK(cond) do { \
  if (!(cond)) { std::printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); ++g_failures; } \
} while (0)

static std::wstring widen_ascii(const std::string& s) {
  return std::wstring(s.begin(), s.end());
}

static long long now_ms() {
  return std::chrono::duration_cast<std::chrono::milliseconds>(
      std::chrono::steady_clock::now().time_since_epoch()).count();
}

static uint64_t nonzero(const std::vector<uint8_t>& y) {
  uint64_t n = 0;
  for (uint8_t v : y) n += (v != 0);
  return n;
}

struct FramePoll {
  WGCCap& cap;
  std::vector<uint8_t>& y;
  std::vector<uint8_t>& uv;
  bool frame(uint64_t* nz = nullptr, int timeout_ms = 5000) {
    const long long deadline = now_ms() + timeout_ms;
    while (now_ms() < deadline) {
      if (cap.capture(y.data(), uv.data())) {
        if (nz) *nz = nonzero(y);
        return true;
      }
      if (cap.err_code != WGCCap::E_TIMEOUT) {
        std::printf("WGC-LIVE: capture error %d: %s\n", (int)cap.err_code, cap.err());
        return false;
      }
      std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    return false;
  }
  bool resized(int old_w, int old_h, uint64_t* nz) {
    const long long deadline = now_ms() + 5000;
    while (now_ms() < deadline) {
      if (!frame(nz, 1000)) return false;
      if (cap.pool_w != old_w || cap.pool_h != old_h) return true;
      std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    return false;
  }
  bool wait_closed(bool& got_closed, int timeout_ms = 5000) {
    got_closed = false;
    const long long deadline = now_ms() + timeout_ms;
    while (now_ms() < deadline) {
      if (!cap.capture(y.data(), uv.data())) {
        if (cap.err_code == WGCCap::E_CLOSED) { got_closed = true; return true; }
        if (cap.err_code != WGCCap::E_TIMEOUT) return false;
      }
      std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    return false;
  }
};

int main(int argc, char** argv) {
  if (argc < 2) {
    std::printf("usage: wgc-live <wgc-live-window.exe> [--gpu]\n");
    return 2;
  }
  const bool want_gpu = (argc > 2 && std::string(argv[2]) == "--gpu");
  const std::string title = "VC-WGC-Live-" + std::to_string((unsigned long)GetCurrentProcessId()) +
                            "-" + std::to_string((unsigned long long)GetTickCount64());
  const std::wstring wtitle = widen_ascii(title);

  std::wstring cmd = L"\"" + widen_ascii(argv[1]) + L"\" \"" + wtitle + L"\"";
  STARTUPINFOW si{}; si.cb = sizeof(si);
  PROCESS_INFORMATION pi{};
  if (!CreateProcessW(nullptr, cmd.data(), nullptr, nullptr, FALSE, CREATE_NO_WINDOW,
                      nullptr, nullptr, &si, &pi)) {
    std::printf("WGC-LIVE: SKIP cannot spawn window process\n");
    return 2;
  }
  auto finish = [&](int code) {
    if (pi.hThread) CloseHandle(pi.hThread);
    if (pi.hProcess) {
      if (WaitForSingleObject(pi.hProcess, 0) == WAIT_TIMEOUT) {
        PostMessageW(FindWindowW(nullptr, wtitle.c_str()), WM_CLOSE, 0, 0);
        if (WaitForSingleObject(pi.hProcess, 2000) == WAIT_TIMEOUT) {
          TerminateProcess(pi.hProcess, 1); // only the private test child created above
          WaitForSingleObject(pi.hProcess, 2000);
        }
      }
      CloseHandle(pi.hProcess);
    }
    return code;
  };

  HWND hwnd = nullptr;
  for (int i = 0; i < 500 && !hwnd; ++i) {
    hwnd = FindWindowW(nullptr, wtitle.c_str());
    if (!hwnd) std::this_thread::sleep_for(std::chrono::milliseconds(10));
  }
  if (!hwnd) {
    std::printf("WGC-LIVE: SKIP window process did not create a window\n");
    return finish(2);
  }

  WGCCap cap;
  cap.window_title = title;
  cap.include_cursor = false;
  cap.stream.width = 640;
  cap.stream.height = 640;
  cap.stream.transform = "stretch"; // tolerate arbitrary source sizes while output stays fixed
  std::string detail;
  if (!cap.init(detail)) {
    const bool unavailable = cap.err_code == WGCCap::E_NODDA || cap.err_code == WGCCap::E_NOOUTPUT ||
                             cap.err_code == WGCCap::E_LOST;
    std::printf("WGC-LIVE: %s init: %s\n", unavailable ? "SKIP" : "FAIL", cap.err());
    return finish(unavailable ? 2 : 1);
  }
  // Successful init must have applied the requested 1ms interval before capture.
  CHECK(cap.session.MinUpdateInterval().count() == WGCCap::min_update_interval_100ns);
  std::printf("WGC-LIVE: MinUpdateInterval=%lld (100ns)\n",
              (long long)cap.session.MinUpdateInterval().count());
  if (want_gpu) {
    std::string why;
    if (!cap.start_gpu(why)) {
      std::printf("WGC-LIVE: SKIP gpu-convert: %s\n", why.c_str());
      return finish(3);
    }
  }

  std::vector<uint8_t> y(cap.stream.y_bytes()), uv(cap.stream.uv_bytes());
  FramePoll poll{cap, y, uv};
  const int initial_pool_w = cap.pool_w, initial_pool_h = cap.pool_h;
  uint64_t nz = 0;
  if (!poll.frame(&nz)) {
    std::printf("WGC-LIVE: FAIL no initial frame (last: %s)\n", cap.err());
    return finish(1);
  }
  CHECK(nz > 0);

  // Grow the captured window. ContentSize (not item.Size) must drive the pool.
  SetWindowPos(hwnd, nullptr, 0, 0, 1000, 720, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
  if (!poll.resized(initial_pool_w, initial_pool_h, &nz)) {
    std::printf("WGC-LIVE: FAIL no frame after grow (last: %s)\n", cap.err());
    return finish(1);
  }
  CHECK(nz > 0);
  const int grown_pool_w = cap.pool_w, grown_pool_h = cap.pool_h;
  CHECK(grown_pool_w != initial_pool_w || grown_pool_h != initial_pool_h);
  CHECK(cap.out_w == grown_pool_w && cap.out_h == grown_pool_h);
  CHECK(cap.stream.width == 640 && cap.stream.height == 640);
  CHECK(y.size() == cap.stream.y_bytes() && uv.size() == cap.stream.uv_bytes());

  // Shrink it again: another ContentSize-driven pool change.
  SetWindowPos(hwnd, nullptr, 0, 0, 760, 560, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
  if (!poll.resized(grown_pool_w, grown_pool_h, &nz)) {
    std::printf("WGC-LIVE: FAIL no frame after shrink (last: %s)\n", cap.err());
    return finish(1);
  }
  CHECK(nz > 0);
  CHECK(cap.pool_w != grown_pool_w || cap.pool_h != grown_pool_h);
  CHECK(cap.stream.width == 640 && cap.stream.height == 640);

  // Destroy the captured window; the Closed event must surface as E_CLOSED,
  // never as an idle E_TIMEOUT.
  PostMessageW(hwnd, WM_CLOSE, 0, 0);
  bool got_closed = false;
  if (!poll.wait_closed(got_closed)) {
    std::printf("WGC-LIVE: FAIL close did not produce E_CLOSED (last: %s, code=%d)\n",
                cap.err(), (int)cap.err_code);
    return finish(1);
  }
  CHECK(got_closed);
  CHECK(cap.err_code == WGCCap::E_CLOSED);
  CHECK(cap.err_code != WGCCap::E_TIMEOUT);

  if (g_failures == 0) {
    std::printf("WGC-LIVE: PASS mode=%s initial=%dx%d grown=%dx%d shrunk=%dx%d out=640x640 closed=E_CLOSED\n",
                want_gpu ? "gpu" : "cpu", initial_pool_w, initial_pool_h,
                grown_pool_w, grown_pool_h, cap.pool_w, cap.pool_h);
    return finish(0);
  }
  std::printf("WGC-LIVE: FAIL (%d checks)\n", g_failures);
  return finish(1);
}
