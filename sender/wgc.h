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

struct WGCCap {
  enum Err { OK=0, E_NODDA=1, E_FULLSCREEN=2, E_FORMAT=3, E_SMALL=4, E_LOST=5, E_TIMEOUT=6, E_INTERNAL=7, E_NOOUTPUT=8 };
  Err err_code = E_NODDA;
  std::string err_msg = "uninitialized";
  int out_w = 0, out_h = 0;
  VCStream stream;
  std::atomic<uint64_t> source_skipped{0}; // 采集线程丢弃的旧帧（最新帧语义）
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
  HANDLE frame_event = nullptr;
  std::mutex frame_mu;
  winrt::Windows::Graphics::Capture::Direct3D11CaptureFrame pending{nullptr};
  std::atomic<bool> closing{false};

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
  void on_frame(const winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool& sender) {
    if (closing.load(std::memory_order_relaxed)) return;
    try {
      auto frame = sender.TryGetNextFrame();
      if (!frame) return;
      std::lock_guard<std::mutex> lock(frame_mu);
      if (closing.load(std::memory_order_relaxed)) { try { frame.Close(); } catch (...) {} return; }
      if (pending) { try { pending.Close(); } catch (...) {} source_skipped.fetch_add(1); }
      pending = frame;
      if (frame_event) SetEvent(frame_event);
    } catch (...) {
      if (frame_event) SetEvent(frame_event);
    }
  }
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

inline bool WGCCap::init(std::string& detail) {
  shutdown();
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
    VCTransform test;
    if (!test.init(out_w, out_h, stream)) {
      shutdown(); err_code = E_SMALL; err_msg = "WGC source smaller than requested crop"; return false;
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
    frame_event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!frame_event) { shutdown(); err_code = E_INTERNAL; err_msg = "WGC frame event creation failed"; return false; }
    frame_token = pool.FrameArrived([this](auto&& sender, auto&&) { on_frame(sender); });
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
  closing.store(true);
  if (session) { try { session.Close(); } catch (...) {} session = nullptr; }
  if (pool) {
    if (frame_token.value) { try { pool.FrameArrived(frame_token); } catch (...) {} frame_token = {}; }
    try { pool.Close(); } catch (...) {} pool = nullptr;
  }
  { std::lock_guard<std::mutex> lock(frame_mu); if (pending) { try { pending.Close(); } catch (...) {} pending = nullptr; } }
  if (frame_event) { CloseHandle(frame_event); frame_event = nullptr; }
  if (qEnd) { qEnd->Release(); qEnd = nullptr; }
  if (qStart) { qStart->Release(); qStart = nullptr; }
  if (qDisjoint) { qDisjoint->Release(); qDisjoint = nullptr; }
  gpu_qok = false;
  if (gpu) { gpu->shutdown(); delete gpu; gpu = nullptr; } want_gpu = false;
  staging.attach(nullptr); stag_w = stag_h = 0;
  ctx.attach(nullptr); dev.attach(nullptr); rtdev = nullptr; item = nullptr;
  closing.store(false);
}

inline bool WGCCap::capture(uint8_t* Y, uint8_t* UV) {
  if (!pool) { err_code = E_NODDA; err_msg = "WGC not initialized"; return false; }
  if (frame_event) WaitForSingleObject(frame_event, 5);
  winrt::Windows::Graphics::Capture::Direct3D11CaptureFrame frame{nullptr};
  { std::lock_guard<std::mutex> lock(frame_mu); frame = pending; pending = nullptr; }
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
    int w = (int)d.Width, h = (int)d.Height;
    if (d.Format != DXGI_FORMAT_B8G8R8A8_UNORM) {
      close_frame(); err_code = E_FORMAT; err_msg = "WGC frame is not B8G8R8A8_UNORM"; return false;
    }
    VCTransform transform;
    if (!transform.init(w, h, stream)) {
      close_frame(); err_code = E_SMALL; err_msg = "unsupported WGC source/output crop or size"; return false;
    }
    bool cropped = stream.transform == "crop";
    int copy_w = cropped ? stream.width : w, copy_h = cropped ? stream.height : h;
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
      bool gok = gpu->run(tex.get(), w, h);
      if (stamped) { ctx->End(qEnd); ctx->End(qDisjoint); }
      close_frame();
      if (!gok) { err_code = E_INTERNAL; err_msg = gpu->err(); return false; }
      if (!gpu->readback(Y, UV)) { err_code = E_INTERNAL; err_msg = gpu->err(); return false; }
    } else {
      if (stamped) { ctx->Begin(qDisjoint); ctx->End(qStart); }
      { D3D11_BOX box{}; box.left = cropped ? transform.ox : 0; box.top = cropped ? transform.oy : 0;
        box.right = box.left + copy_w; box.bottom = box.top + copy_h; box.back = 1;
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
    close_frame(); err_code = E_LOST;
    err_msg = std::string("WGC frame failed: ") + winrt::to_string(e.message());
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
