#pragma once
// Windows Graphics Capture 采集后端（显示器或指定窗口）
// 与 DDA 同构：BGRX → 整数 BT.601 NV12，全程整数无浮点。
// 失败一律显式报错，不静默回退 DDA；独占全屏无法采集（WGC 本身拿不到）。
// 注意：WGC 仍经 DWM 合成，不消除合成延迟；窗口模式默认包含光标。
#include <cstdint>
#include <cstdio>
#include <string>
#include <vector>
#include <atomic>
#include <mutex>
#include <chrono>
#include <memory>
#include <cwctype>
#include "pattern.h"
#include "dda_gpu.h"
#ifdef _WIN32
#include <d3d11.h>
#include <dxgi1_2.h>
#include <dwmapi.h>
#include <winrt/base.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Foundation.Metadata.h>
#include <winrt/Windows.Graphics.h>
#include <winrt/Windows.Graphics.Capture.h>
#include <winrt/Windows.Graphics.DirectX.h>
#include <winrt/Windows.Graphics.DirectX.Direct3D11.h>
#include <windows.graphics.capture.interop.h>
#include <windows.graphics.directx.direct3d11.interop.h>
#endif

// WGC 帧纹理永远等于帧池尺寸，而 ContentSize 才是窗口真正有定义的像素区域。
// 两个矩形都只描述“可以安全读取的源区域”，协商输出仍是 stream.width/height。
struct WGCSourceRect {
  int w = 0, h = 0;
  bool valid() const { return w >= 2 && h >= 2; }
};
struct WGCCopyRect {
  int x = 0, y = 0, w = 0, h = 0;
  bool valid() const { return w >= 2 && h >= 2; }
};

// 采集线程丢弃的旧帧计数。底层原子由 WGCSharedState 与 WGCCap 共同持有：
// 即使 WGCCap 已析构，仍在排队的回调也只访问这个独立原子，不会碰 WGCCap 本体。
struct WGCSkippedCounter {
  std::shared_ptr<std::atomic<uint64_t>> value{std::make_shared<std::atomic<uint64_t>>(0)};
  uint64_t load() const { return value ? value->load(std::memory_order_relaxed) : 0; }
  uint64_t fetch_add(uint64_t n, std::memory_order order = std::memory_order_relaxed) {
    return value ? value->fetch_add(n, order) : 0;
  }
};

#ifdef _WIN32
// FrameArrived/Closed 回调只捕获这个堆对象的 shared_ptr，绝不捕获 WGCCap 的
// this。shutdown() 撤销 token 并关闭帧池后，已排队但尚未执行的 delegate 仍可
// 安全运行：它持有的 shared_ptr 保证事件句柄、pending 帧和标志位存活到回调结束。
struct WGCSharedState {
  using FramePool = winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool;
  using Frame = winrt::Windows::Graphics::Capture::Direct3D11CaptureFrame;

  std::mutex frame_mu;
  std::timed_mutex pool_mu; // serialize TryGetNextFrame with Recreate, including queued delegates
  Frame pending{nullptr};
  HANDLE frame_event = nullptr;
  std::atomic<bool> closing{false};
  std::atomic<bool> recreating{false};
  std::atomic<bool> item_closed{false};
  std::atomic<bool> callback_failed{false};
  std::atomic<long> callback_hr{0};
  std::atomic<int> inflight{0};
  std::shared_ptr<std::atomic<uint64_t>> skipped;

  struct CallbackScope {
    std::atomic<int>& n;
    explicit CallbackScope(std::atomic<int>& v) : n(v) { n.fetch_add(1, std::memory_order_acq_rel); }
    ~CallbackScope() { n.fetch_sub(1, std::memory_order_acq_rel); }
  };

  ~WGCSharedState() {
    if (pending) { try { pending.Close(); } catch (...) {} pending = nullptr; }
    if (frame_event) { CloseHandle(frame_event); frame_event = nullptr; }
  }

  void signal_frame() noexcept {
    HANDLE ev = nullptr;
    { std::lock_guard<std::mutex> lock(frame_mu); ev = frame_event; }
    if (ev) SetEvent(ev);
  }

  void on_item_closed() noexcept {
    CallbackScope scope(inflight);
    item_closed.store(true, std::memory_order_release);
    signal_frame();
  }

  void on_frame(const FramePool& sender) {
    CallbackScope scope(inflight);
    if (closing.load(std::memory_order_relaxed) || recreating.load(std::memory_order_acquire)) return;
    std::lock_guard<std::timed_mutex> pool_lock(pool_mu);
    if (closing.load(std::memory_order_relaxed) || recreating.load(std::memory_order_acquire)) return;
    try {
      auto frame = sender.TryGetNextFrame();
      if (!frame) return;
      HANDLE ev = nullptr;
      {
        std::lock_guard<std::mutex> lock(frame_mu);
        if (closing.load(std::memory_order_relaxed)) { try { frame.Close(); } catch (...) {} return; }
        if (pending) {
          try { pending.Close(); } catch (...) {}
          if (skipped) skipped->fetch_add(1, std::memory_order_relaxed);
        }
        pending = frame;
        ev = frame_event;
      }
      if (ev) SetEvent(ev);
    } catch (const winrt::hresult_error& e) {
      if (!closing.load(std::memory_order_relaxed)) {
        callback_hr.store((long)e.code(), std::memory_order_release);
        callback_failed.store(true, std::memory_order_release);
      }
      signal_frame();
    } catch (...) {
      if (!closing.load(std::memory_order_relaxed)) {
        callback_hr.store((long)E_FAIL, std::memory_order_release);
        callback_failed.store(true, std::memory_order_release);
      }
      signal_frame();
    }
  }
};
#endif

struct WGCCap {
  // E_CLOSED/E_CALLBACK 必须与 E_TIMEOUT 不同：前者是采集终结，后者只是本帧没有新图像。
  // sender（vc_sender.cpp / encoded_sender.h）只把 E_TIMEOUT 当作非致命，因此新值会
  // 走现有的 fatal 分支并打印 err_msg；无需改调用方。
  enum Err { OK=0, E_NODDA=1, E_FULLSCREEN=2, E_FORMAT=3, E_SMALL=4, E_LOST=5, E_TIMEOUT=6, E_INTERNAL=7, E_NOOUTPUT=8, E_CLOSED=9, E_CALLBACK=10 };

  // 纯几何/错误映射辅助函数（无 WinRT 依赖，回归测试直接调用）：
  // 只保留 ContentSize 内、且不超过纹理边界的有效源尺寸。
  static WGCSourceRect content_rect(int content_w, int content_h, int tex_w, int tex_h) {
    WGCSourceRect r;
    if (content_w < 2 || content_h < 2 || tex_w < 2 || tex_h < 2) return r;
    r.w = content_w < tex_w ? content_w : tex_w;
    r.h = content_h < tex_h ? content_h : tex_h;
    if (r.w < 2 || r.h < 2) { r.w = r.h = 0; }
    return r;
  }
  // 尺寸变化后必须重建帧池；最小化/无效尺寸不重建，只等下一帧。
  static bool needs_pool_recreate(int width, int height, int pool_w, int pool_h) {
    if (width < 2 || height < 2) return false;
    return width != pool_w || height != pool_h;
  }
  // CPU 路径实际拷贝的源矩形：裁切时取中心窗口，其余变换取整个有效内容区。
  static WGCCopyRect frame_copy_rect(const VCTransform& t, const VCStream& s, const WGCSourceRect& src) {
    WGCCopyRect r;
    if (!src.valid()) return r;
    if (s.transform == "crop") {
      r.x = (int)t.ox; r.y = (int)t.oy;
      r.w = (int)s.width; r.h = (int)s.height;
    } else {
      r.w = src.w; r.h = src.h;
    }
    if (r.x < 0 || r.y < 0 || r.x + r.w > src.w || r.y + r.h > src.h) return WGCCopyRect{};
    return r;
  }
  // 等待后没有可用帧时的错误优先级：Closed > 回调终结错误 > 空闲超时。
  static Err no_frame_error(bool item_closed, bool callback_failed) {
    if (item_closed) return E_CLOSED;
    if (callback_failed) return E_CALLBACK;
    return E_TIMEOUT;
  }
  // RO_E_CLOSED：采集项/帧池已关闭，映射为 E_CLOSED 而不是内部错误。
  static bool closed_hresult(long hr) { return hr == (long)0x80000013L; }
  Err err_code = E_NODDA;
  std::string err_msg = "uninitialized";
  int out_w = 0, out_h = 0;
  VCStream stream;
  WGCSkippedCounter source_skipped{}; // 采集线程丢弃的旧帧（最新帧语义）
  uint64_t last_image_qpc = 0;
  uint64_t last_present_qpc = 0; // 帧时间戳（100ns→QPC ticks），0=未知
  uint64_t last_convert_qpc = 0; // 不含等待新帧
  double gpu_accum_ms = 0, last_gpu_ms = 0;
  uint64_t gpu_samples = 0;
  bool want_gpu = false;
  DDAGpu* gpu = nullptr;
  int output = 0;              // 显示器索引（窗口模式忽略）
  std::string window_title;    // 非空=按窗口标题子串采集
  bool include_cursor = true;
#ifdef _WIN32
  winrt::com_ptr<ID3D11Device> dev;
  winrt::com_ptr<ID3D11DeviceContext> ctx;
  winrt::com_ptr<ID3D11Texture2D> staging;
  int stag_w = 0, stag_h = 0;
  ID3D11Query* qDisjoint = nullptr;
  ID3D11Query* qStart = nullptr;
  ID3D11Query* qEnd = nullptr;
  bool gpu_qok = false;
  std::string gpu_qmsg = "uninitialized";
  winrt::Windows::Graphics::DirectX::Direct3D11::IDirect3DDevice rtdev{nullptr};
  winrt::Windows::Graphics::Capture::GraphicsCaptureItem item{nullptr};
  winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool pool{nullptr};
  winrt::Windows::Graphics::Capture::GraphicsCaptureSession session{nullptr};
  winrt::event_token frame_token{};
  winrt::event_token item_closed_token{};
  HWND target_window = nullptr;
  DWORD target_process = 0;
  // 回调共享状态；shutdown() 用新实例替换它，旧实例由在途/排队 delegate 持有到结束。
  std::shared_ptr<WGCSharedState> state = std::make_shared<WGCSharedState>();
  using CallbackScope = WGCSharedState::CallbackScope; // 回归测试直接驱动
  int pool_w = 0, pool_h = 0; // 帧池尺寸（帧纹理尺寸），resize 检测基准

  static uint64_t ticks100_to_qpc(uint64_t t) {
    LARGE_INTEGER f; QueryPerformanceFrequency(&f);
    return (t / 10000000ull) * (uint64_t)f.QuadPart +
           ((t % 10000000ull) * (uint64_t)f.QuadPart) / 10000000ull;
  }
  static std::wstring widen(const std::string& s) {
    if (s.empty()) return L"";
    int n = MultiByteToWideChar(CP_UTF8, 0, s.c_str(), (int)s.size(), nullptr, 0);
    if (n <= 0) return L"";
    std::wstring w((size_t)n, L'\0');
    MultiByteToWideChar(CP_UTF8, 0, s.c_str(), (int)s.size(), &w[0], n);
    return w;
  }
  struct FindWindow { std::wstring needle; HWND hwnd = nullptr; };
  static BOOL CALLBACK enum_window(HWND h, LPARAM lp) {
    FindWindow* f = (FindWindow*)lp;
    if (!IsWindowVisible(h) || IsIconic(h)) return TRUE;
    int len = GetWindowTextLengthW(h);
    if (len <= 0) return TRUE;
    std::wstring title((size_t)len + 1, L'\0');
    int got = GetWindowTextW(h, &title[0], len + 1);
    if (got <= 0) return TRUE;
    title.resize((size_t)got);
    std::wstring low = title;
    for (auto& c : low) c = (wchar_t)towlower(c);
    if (low.find(f->needle) == std::wstring::npos) return TRUE;
    BOOL cloaked = FALSE;
    if (SUCCEEDED(DwmGetWindowAttribute(h, DWMWA_CLOAKED, &cloaked, sizeof(cloaked))) && cloaked) return TRUE;
    DWORD pid = 0; GetWindowThreadProcessId(h, &pid);
    if (pid == GetCurrentProcessId()) return TRUE;
    RECT r{}; if (!GetWindowRect(h, &r) || r.right - r.left < 2 || r.bottom - r.top < 2) return TRUE;
    f->hwnd = h; return FALSE;
  }
  static bool output_monitor(ID3D11Device* d, int index, HMONITOR& hmon, int& w, int& h) {
    winrt::com_ptr<IDXGIDevice> dxgi;
    if (FAILED(d->QueryInterface(__uuidof(IDXGIDevice), dxgi.put_void())) || !dxgi) return false;
    winrt::com_ptr<IDXGIAdapter> adapter;
    if (FAILED(dxgi->GetAdapter(adapter.put())) || !adapter) return false;
    winrt::com_ptr<IDXGIOutput> out;
    if (FAILED(adapter->EnumOutputs((UINT)index, out.put())) || !out) return false;
    DXGI_OUTPUT_DESC od{};
    if (FAILED(out->GetDesc(&od))) return false;
    hmon = od.Monitor;
    w = (int)od.DesktopCoordinates.right - (int)od.DesktopCoordinates.left;
    h = (int)od.DesktopCoordinates.bottom - (int)od.DesktopCoordinates.top;
    return w >= 2 && h >= 2;
  }
  bool recreate_pool(int w, int h);
#endif
  bool init(std::string& detail);
  void shutdown();
  bool capture(uint8_t* Y, uint8_t* UV);
  bool start_gpu(std::string& why);
  const char* err() const { return err_msg.c_str(); }
  ~WGCCap() { shutdown(); }
};

#ifdef _WIN32
inline bool WGCCap::start_gpu(std::string& why) {
  if (!dev || !ctx) { why = "gpu-convert before init"; return false; }
  if (gpu) { want_gpu = true; why = "already started"; return true; }
  gpu = new DDAGpu();
  if (!gpu) { why = "out of memory"; return false; }
  if (!gpu->init(dev.get(), ctx.get(), qDisjoint, qStart, qEnd, stream)) {
    why = gpu->err(); delete gpu; gpu = nullptr; return false;
  }
  want_gpu = true; why = "gpu-convert on (compute shader)"; return true;
}

// 在 capture()（发送线程，非 FrameArrived 回调）中重建帧池。先撤销回调并等
// 在途回调退出；若 drain 超时则显式失败，绝不在回调可能仍持有旧池时 Recreate。
inline bool WGCCap::recreate_pool(int w, int h) {
  if (!pool || !rtdev || !state) { err_code = E_INTERNAL; err_msg = "WGC pool recreate without active pool"; return false; }
  auto st = state;
  st->recreating.store(true, std::memory_order_release);
  try {
    if (frame_token.value) { try { pool.FrameArrived(frame_token); } catch (...) {} frame_token = {}; }
    // Synchronize actual pool access, not a guessed quiet interval: queued
    // delegates may start at any time after token revocation.
    std::unique_lock<std::timed_mutex> pool_lock(st->pool_mu, std::defer_lock);
    if (!pool_lock.try_lock_for(std::chrono::seconds(1))) {
      st->recreating.store(false, std::memory_order_release);
      err_code = E_INTERNAL; err_msg = "WGC callback drain timed out before pool recreate";
      return false;
    }
    {
      std::lock_guard<std::mutex> lock(st->frame_mu);
      if (st->pending) { try { st->pending.Close(); } catch (...) {} st->pending = nullptr; }
      if (st->frame_event) ResetEvent(st->frame_event);
    }
    pool.Recreate(rtdev, winrt::Windows::Graphics::DirectX::DirectXPixelFormat::B8G8R8A8UIntNormalized,
                  2, winrt::Windows::Graphics::SizeInt32{ w, h });
    frame_token = pool.FrameArrived([st](auto&& sender, auto&&) { st->on_frame(sender); });
    pool_w = w; pool_h = h; out_w = w; out_h = h;
    st->recreating.store(false, std::memory_order_release);
    err_code = OK; err_msg = "ok";
    return true;
  } catch (const winrt::hresult_error& e) {
    st->recreating.store(false, std::memory_order_release);
    err_code = (st->item_closed.load(std::memory_order_acquire) || closed_hresult((long)e.code()))
                 ? E_CLOSED : E_INTERNAL;
    err_msg = std::string("WGC frame pool recreate failed: ") + winrt::to_string(e.message());
    return false;
  } catch (...) {
    st->recreating.store(false, std::memory_order_release);
    err_code = E_INTERNAL; err_msg = "WGC frame pool recreate failed";
    return false;
  }
}

inline bool WGCCap::init(std::string& detail) {
  shutdown();
  auto st = state; // shutdown() 已创建新的回调状态
  err_code = E_NODDA; err_msg = "WGC unavailable";
  // CreateFreeThreaded 需要 MTA；整个进程调用一次。
  static std::once_flag once;
  try {
    std::call_once(once, [] { winrt::init_apartment(winrt::apartment_type::multi_threaded); });
  } catch (const winrt::hresult_error& e) {
    err_msg = std::string("WinRT apartment failed: ") + winrt::to_string(e.message());
    return false;
  }
  HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0, D3D11_SDK_VERSION, dev.put(), nullptr, ctx.put());
  if (FAILED(hr) || !dev || !ctx) { shutdown(); err_msg = "WGC D3D11CreateDevice(BGRA) failed"; return false; }
  winrt::com_ptr<IDXGIDevice> dxgi;
  if (FAILED(dev->QueryInterface(__uuidof(IDXGIDevice), dxgi.put_void())) || !dxgi) {
    shutdown(); err_msg = "WGC no DXGI device"; return false;
  }
  winrt::com_ptr<::IInspectable> inspectable;
  hr = CreateDirect3D11DeviceFromDXGIDevice(dxgi.get(), inspectable.put());
  if (FAILED(hr) || !inspectable) { shutdown(); err_msg = "WGC CreateDirect3D11Device failed"; return false; }
  rtdev = inspectable.as<winrt::Windows::Graphics::DirectX::Direct3D11::IDirect3DDevice>();
  try {
    auto interop = winrt::get_activation_factory<
      winrt::Windows::Graphics::Capture::GraphicsCaptureItem, IGraphicsCaptureItemInterop>();
    if (window_title.empty()) {
      HMONITOR hmon = nullptr; int sw = 0, sh = 0;
      if (!output_monitor(dev.get(), output, hmon, sw, sh)) {
        shutdown(); err_code = E_NOOUTPUT; err_msg = "WGC no matching monitor"; return false;
      }
      hr = interop->CreateForMonitor(hmon,
        winrt::guid_of<winrt::Windows::Graphics::Capture::GraphicsCaptureItem>(), winrt::put_abi(item));
    } else {
      FindWindow find;
      find.needle = widen(window_title);
      for (auto& c : find.needle) c = (wchar_t)towlower(c);
      EnumWindows(enum_window, (LPARAM)&find);
      if (!find.hwnd) {
        shutdown(); err_code = E_NOOUTPUT; err_msg = "WGC window not found: " + window_title; return false;
      }
      target_window = find.hwnd;
      GetWindowThreadProcessId(target_window, &target_process);
      hr = interop->CreateForWindow(find.hwnd,
        winrt::guid_of<winrt::Windows::Graphics::Capture::GraphicsCaptureItem>(), winrt::put_abi(item));
    }
    if (FAILED(hr) || !item) {
      shutdown(); err_code = E_LOST;
      err_msg = "WGC CreateForMonitor/Window failed (closed or protected content)";
      return false;
    }
    auto size = item.Size();
    out_w = size.Width; out_h = size.Height;
    pool_w = out_w; pool_h = out_h;
    VCTransform test;
    if (!test.init(out_w, out_h, stream)) {
      shutdown(); err_code = E_SMALL; err_msg = "WGC source smaller than requested crop"; return false;
    }
    // 窗口被销毁/采集项关闭是终结状态，必须与空闲超时区分开。
    try {
      item_closed_token = item.Closed([st](auto&&, auto&&) { st->on_item_closed(); });
    } catch (...) {
      shutdown(); err_code = E_CLOSED; err_msg = "WGC capture item already closed"; return false;
    }
    // 与 DDA 相同的 GPU 时间戳查询（尽力而为，失败只停诊断字段）。
    gpu_qmsg = "query create failed (wall-clock only)";
    {
      D3D11_QUERY_DESC qd{}; qd.Query = D3D11_QUERY_TIMESTAMP_DISJOINT;
      ID3D11Query* dj = nullptr;
      if (SUCCEEDED(dev->CreateQuery(&qd, &dj)) && dj) {
        qd.Query = D3D11_QUERY_TIMESTAMP;
        ID3D11Query* ts = nullptr; ID3D11Query* te = nullptr;
        if (SUCCEEDED(dev->CreateQuery(&qd, &ts)) && ts && SUCCEEDED(dev->CreateQuery(&qd, &te)) && te) {
          qDisjoint = dj; qStart = ts; qEnd = te;
          gpu_qok = true; gpu_qmsg = "timestamp+disjoint ok";
        } else {
          if (ts) ts->Release(); if (te) te->Release(); dj->Release();
        }
      }
    }
    pool = winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool::CreateFreeThreaded(
      rtdev, winrt::Windows::Graphics::DirectX::DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, item.Size());
    if (!pool) { shutdown(); err_code = E_INTERNAL; err_msg = "WGC frame pool creation failed"; return false; }
    st->frame_event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!st->frame_event) { shutdown(); err_code = E_INTERNAL; err_msg = "WGC frame event creation failed"; return false; }
    frame_token = pool.FrameArrived([st](auto&& sender, auto&&) { st->on_frame(sender); });
    session = pool.CreateCaptureSession(item);
    if (!session) { shutdown(); err_code = E_INTERNAL; err_msg = "WGC capture session failed"; return false; }
    try { session.IsCursorCaptureEnabled(include_cursor); } catch (...) {}
    try { session.IsBorderRequired(false); } catch (...) {} // Win10 无此属性，保持系统边框
    session.StartCapture();
  } catch (const winrt::hresult_error& e) {
    shutdown(); err_code = E_LOST;
    err_msg = std::string("WGC init failed: ") + winrt::to_string(e.message());
    return false;
  }
  char db[128];
  snprintf(db, sizeof(db), "%s%d:%dx%d", window_title.empty() ? "monitor " : "window ", output, out_w, out_h);
  detail = db;
  err_code = OK; err_msg = "ok";
  return true;
}

inline void WGCCap::shutdown() {
  auto old = state;
  if (old) {
    old->closing.store(true, std::memory_order_release);
    // 先断回调源。已经排队/在途的 delegate 各自持有 old 的 shared_ptr，
    // 因此这里无需（也不能）用有界 drain 来保证安全；它们只访问 old，
    // 不会再碰 WGCCap 或新会话的帧池。
    if (pool && frame_token.value) { try { pool.FrameArrived(frame_token); } catch (...) {} }
    frame_token = {};
    if (item && item_closed_token.value) { try { item.Closed(item_closed_token); } catch (...) {} }
    item_closed_token = {};
    if (session) { try { session.Close(); } catch (...) {} session = nullptr; }
    if (pool) { try { pool.Close(); } catch (...) {} pool = nullptr; }
    // old->frame_event / old->pending 由 old 的析构函数在最后一个引用释放时关闭。
  }
  if (qEnd) { qEnd->Release(); qEnd = nullptr; }
  if (qStart) { qStart->Release(); qStart = nullptr; }
  if (qDisjoint) { qDisjoint->Release(); qDisjoint = nullptr; }
  gpu_qok = false;
  if (gpu) { gpu->shutdown(); delete gpu; gpu = nullptr; } want_gpu = false;
  staging.attach(nullptr); stag_w = stag_h = 0;
  ctx.attach(nullptr); dev.attach(nullptr); rtdev = nullptr; item = nullptr;
  pool_w = pool_h = 0;
  target_window = nullptr; target_process = 0;
  // 下一次 init 使用全新状态；丢弃计数沿用同一个原子，保持累计统计。
  state = std::make_shared<WGCSharedState>();
  state->skipped = source_skipped.value;
}

inline bool WGCCap::capture(uint8_t* Y, uint8_t* UV) {
  if (!pool) { err_code = E_NODDA; err_msg = "WGC not initialized"; return false; }
  auto st = state;
  if (!st) { err_code = E_INTERNAL; err_msg = "WGC callback state missing"; return false; }

  // Some Windows/driver combinations do not promptly dispatch Closed. Also
  // check the exact original HWND/process; never reselect another matching title.
  if (target_window) {
    DWORD pid = 0;
    if (!IsWindow(target_window) || !GetWindowThreadProcessId(target_window, &pid) || pid != target_process)
      st->item_closed.store(true, std::memory_order_release);
  }
  // 终结状态优先于任何等待/超时：窗口关闭或回调抛出后，不能继续伪装成空闲。
  auto terminal_error = [&](const char* timeout_msg) -> bool {
    bool closed_flag = st->item_closed.load(std::memory_order_acquire);
    bool callback_error = st->callback_failed.load(std::memory_order_acquire);
    Err mapped = no_frame_error(closed_flag, callback_error);
    if (mapped == E_CLOSED) { err_code = E_CLOSED; err_msg = "WGC capture item closed"; return true; }
    if (mapped == E_CALLBACK) {
      char b[128];
      snprintf(b, sizeof(b), "WGC frame callback failed (hr=0x%08lX)",
               (unsigned long)st->callback_hr.load(std::memory_order_acquire));
      err_code = E_CALLBACK; err_msg = b; return true;
    }
    err_code = E_TIMEOUT; err_msg = timeout_msg ? timeout_msg : "no new WGC frame"; return false;
  };
  if (terminal_error("no new WGC frame")) return false;

  if (st->frame_event) WaitForSingleObject(st->frame_event, 5);
  if (terminal_error("no new WGC frame")) return false;
  winrt::Windows::Graphics::Capture::Direct3D11CaptureFrame frame{nullptr};
  { std::lock_guard<std::mutex> lock(st->frame_mu); frame = st->pending; st->pending = nullptr; }
  if (!frame) { err_code = E_TIMEOUT; err_msg = "no new WGC frame"; return false; }
  bool closed = false;
  auto close_frame = [&]() { if (!closed) { closed = true; try { frame.Close(); } catch (...) {} } };
  try {
    LARGE_INTEGER convert_start; QueryPerformanceCounter(&convert_start);
    winrt::com_ptr<Windows::Graphics::DirectX::Direct3D11::IDirect3DDxgiInterfaceAccess> access;
    winrt::check_hresult(winrt::get_unknown(frame.Surface())->QueryInterface(
      __uuidof(Windows::Graphics::DirectX::Direct3D11::IDirect3DDxgiInterfaceAccess), access.put_void()));
    winrt::com_ptr<ID3D11Texture2D> tex;
    winrt::check_hresult(access->GetInterface(__uuidof(ID3D11Texture2D), tex.put_void()));
    D3D11_TEXTURE2D_DESC d{}; tex->GetDesc(&d);
    int tex_w = (int)d.Width, tex_h = (int)d.Height;
    if (d.Format != DXGI_FORMAT_B8G8R8A8_UNORM) {
      close_frame(); err_code = E_FORMAT; err_msg = "WGC frame is not B8G8R8A8_UNORM"; return false;
    }
    // 帧纹理 = 帧池尺寸；ContentSize 才是窗口真正有定义的像素区域，也是
    // resize 的唯一权威触发点（item.Size() 可能一直停在初始值）。
    winrt::Windows::Graphics::SizeInt32 content_size{};
    try {
      content_size = frame.ContentSize();
    } catch (const winrt::hresult_error& e) {
      close_frame(); tex.attach(nullptr); access.attach(nullptr);
      err_code = (st->item_closed.load(std::memory_order_acquire) || closed_hresult((long)e.code()))
                   ? E_CLOSED : E_LOST;
      err_msg = std::string("WGC frame ContentSize failed: ") + winrt::to_string(e.message());
      return false;
    } catch (...) {
      close_frame(); tex.attach(nullptr); access.attach(nullptr);
      err_code = E_LOST; err_msg = "WGC frame ContentSize failed";
      return false;
    }
    const int content_w = (int)content_size.Width;
    const int content_h = (int)content_size.Height;
    if (needs_pool_recreate(content_w, content_h, pool_w, pool_h)) {
      // 先释放本帧纹理/帧，再在发送线程（非 FrameArrived）上重建帧池，
      // 本帧作废；下一帧才用新池处理。
      close_frame();
      frame = nullptr; // Close() 之外还要放掉 projected 对象的 COM 引用
      tex.attach(nullptr);
      access.attach(nullptr);
      // GPU 路径缓存了旧池纹理的 SRV/srcTex，Recreate 前必须解除引用。
      if (want_gpu && gpu) {
        if (ctx) { ID3D11ShaderResourceView* none[1] = { nullptr }; ctx->CSSetShaderResources(0, 1, none); }
        if (gpu->srv) { gpu->srv->Release(); gpu->srv = nullptr; }
        gpu->srcTex = nullptr;
      }
      if (!recreate_pool(content_w, content_h)) return false;
      err_code = E_TIMEOUT; err_msg = "WGC frame pool resized to ContentSize; waiting for a new frame";
      return false;
    }
    if (content_w > tex_w || content_h > tex_h) {
      // 帧纹理应等于池尺寸；出现更小的纹理说明池状态不一致，显式失败而不是裁切。
      close_frame(); tex.attach(nullptr); access.attach(nullptr);
      err_code = E_INTERNAL; err_msg = "WGC frame texture smaller than ContentSize";
      return false;
    }
    WGCSourceRect src = content_rect(content_w, content_h, tex_w, tex_h);
    if (!src.valid()) {
      close_frame(); terminal_error("WGC frame has no drawable content"); return false;
    }
    VCTransform transform;
    if (!transform.init(src.w, src.h, stream)) {
      close_frame(); err_code = E_SMALL; err_msg = "unsupported WGC source/output crop or size"; return false;
    }
    WGCCopyRect copy = frame_copy_rect(transform, stream, src);
    if (!copy.valid()) {
      close_frame(); err_code = E_SMALL; err_msg = "WGC frame copy falls outside content"; return false;
    }
    bool cropped = stream.transform == "crop";
    int copy_w = copy.w, copy_h = copy.h;
    if (!want_gpu && (!staging || stag_w != copy_w || stag_h != copy_h)) {
      staging.attach(nullptr);
      D3D11_TEXTURE2D_DESC sd{}; sd.Width = (UINT)copy_w; sd.Height = (UINT)copy_h;
      sd.MipLevels = 1; sd.ArraySize = 1; sd.Format = d.Format; sd.SampleDesc.Count = 1;
      sd.Usage = D3D11_USAGE_STAGING; sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
      HRESULT shr = dev->CreateTexture2D(&sd, nullptr, staging.put());
      if (FAILED(shr) || !staging) {
        close_frame(); err_code = E_INTERNAL; err_msg = "WGC staging texture failed"; return false;
      }
      stag_w = copy_w; stag_h = copy_h;
    }
    auto rel = frame.SystemRelativeTime();
    uint64_t rel100 = (uint64_t)rel.count();
    if (rel100) { last_present_qpc = ticks100_to_qpc(rel100); last_image_qpc = last_present_qpc; }
    bool stamped = gpu_qok && qDisjoint && qStart && qEnd;
    if (want_gpu) {
      if (!gpu) { close_frame(); err_code = E_INTERNAL; err_msg = "gpu-convert requested but gpu not started"; return false; }
      if (stamped) { ctx->Begin(qDisjoint); ctx->End(qStart); }
      bool gok = gpu->run(tex.get(), src.w, src.h);
      if (stamped) { ctx->End(qEnd); ctx->End(qDisjoint); }
      close_frame();
      if (!gok) { err_code = E_INTERNAL; err_msg = gpu->err(); return false; }
      if (!gpu->readback(Y, UV)) { err_code = E_INTERNAL; err_msg = gpu->err(); return false; }
    } else {
      if (stamped) { ctx->Begin(qDisjoint); ctx->End(qStart); }
      { D3D11_BOX box{}; box.left = (UINT)copy.x; box.top = (UINT)copy.y;
        box.right = (UINT)(copy.x + copy_w); box.bottom = (UINT)(copy.y + copy_h); box.back = 1;
        ctx->CopySubresourceRegion(staging.get(), 0, 0, 0, 0, tex.get(), 0, &box); }
      if (stamped) { ctx->End(qEnd); ctx->End(qDisjoint); }
      D3D11_MAPPED_SUBRESOURCE mp{};
      HRESULT mhr = ctx->Map(staging.get(), 0, D3D11_MAP_READ, 0, &mp);
      if (FAILED(mhr)) { close_frame(); err_code = E_INTERNAL; err_msg = "WGC staging map failed"; return false; }
      close_frame(); // Map 已保证 GPU 拷贝完成；释放 WGC 帧再跑 CPU 转换
      if (cropped) transform.init(copy_w, copy_h, stream);
      vc_bgrx_nv12((const uint8_t*)mp.pData, (size_t)mp.RowPitch, transform, Y, UV);
      ctx->Unmap(staging.get(), 0);
    }
    if (stamped) {
      D3D11_QUERY_DATA_TIMESTAMP_DISJOINT dj; HRESULT gr = S_FALSE;
      for (int i = 0; i < 200; i++) {
        gr = ctx->GetData(qDisjoint, &dj, sizeof(dj), D3D11_ASYNC_GETDATA_DONOTFLUSH);
        if (gr == S_OK) break;
        if (gr != S_FALSE) break;
        if ((i & 15) == 15) ::Sleep(0);
      }
      if (gr == S_OK && !dj.Disjoint && dj.Frequency != 0) {
        UINT64 t0 = 0, t1 = 0; HRESULT r0 = S_FALSE, r1 = S_FALSE;
        for (int i = 0; i < 50; i++) {
          if (r0 != S_OK) r0 = ctx->GetData(qStart, &t0, sizeof(t0), D3D11_ASYNC_GETDATA_DONOTFLUSH);
          if (r1 != S_OK) r1 = ctx->GetData(qEnd, &t1, sizeof(t1), D3D11_ASYNC_GETDATA_DONOTFLUSH);
          if (r0 == S_OK && r1 == S_OK) break;
          if ((r0 != S_FALSE && r0 != S_OK) || (r1 != S_FALSE && r1 != S_OK)) break;
          if ((i & 7) == 7) ::Sleep(0);
        }
        if (r0 == S_OK && r1 == S_OK && t1 >= t0) {
          last_gpu_ms = (double)(t1 - t0) * 1000.0 / (double)dj.Frequency;
          gpu_accum_ms += last_gpu_ms; gpu_samples++;
        }
      }
    }
    LARGE_INTEGER convert_end; QueryPerformanceCounter(&convert_end);
    last_convert_qpc = (uint64_t)(convert_end.QuadPart - convert_start.QuadPart);
    err_code = OK; err_msg = "ok"; return true;
  } catch (const winrt::hresult_error& e) {
    close_frame();
    Err mapped = no_frame_error(st->item_closed.load(std::memory_order_acquire),
                                st->callback_failed.load(std::memory_order_acquire));
    if (mapped == E_CLOSED) {
      err_code = E_CLOSED; err_msg = "WGC capture item closed";
    } else if (mapped == E_CALLBACK) {
      char b[128];
      snprintf(b, sizeof(b), "WGC frame callback failed (hr=0x%08lX)",
               (unsigned long)st->callback_hr.load(std::memory_order_acquire));
      err_code = E_CALLBACK; err_msg = b;
    } else {
      err_code = E_LOST;
      err_msg = std::string("WGC frame failed: ") + winrt::to_string(e.message());
    }
    return false;
  }
}
inline int wgc_selftest() {
  WGCCap c; std::string detail;
  if (!c.init(detail)) {
    if (c.err_code == WGCCap::E_NODDA || c.err_code == WGCCap::E_NOOUTPUT ||
        c.err_code == WGCCap::E_LOST) {
      printf("WGC: SKIP %s\n", c.err()); return 2;
    }
    printf("WGC: FAIL (%s)\n", c.err()); return 1;
  }
  std::vector<uint8_t> Y(VC_Y_BYTES), UV(VC_UV_BYTES);
  bool got = false;
  for (int i = 0; i < 10 && !got; i++) {
    if (c.capture(Y.data(), UV.data())) got = true;
    else if (c.err_code != WGCCap::E_TIMEOUT) break;
  }
  if (!got) {
    if (c.err_code == WGCCap::E_TIMEOUT) { printf("WGC: SKIP idle-output\n"); return 2; }
    printf("WGC: FAIL capture (%s)\n", c.err()); return 1;
  }
  uint64_t nz = 0; for (size_t i = 0; i < Y.size(); i++) nz += (Y[i] != 0);
  printf("WGC: PASS output=%dx%d center640 NV12 nonzero-Y=%llu\n", c.out_w, c.out_h, (unsigned long long)nz);
  return 0;
}
#else
inline bool WGCCap::init(std::string&) { shutdown(); err_code = E_NODDA; err_msg = "non-Windows build"; return false; }
inline void WGCCap::shutdown() {}
inline bool WGCCap::capture(uint8_t*, uint8_t*) { err_code = E_NODDA; err_msg = "non-Windows build"; return false; }
inline bool WGCCap::start_gpu(std::string& why) { why = "non-Windows build"; return false; }
inline int wgc_selftest() { printf("WGC: SKIP (non-Windows build)\n"); return 2; }
#endif
