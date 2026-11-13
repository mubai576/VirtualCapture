// Private window process for the optional live WGC regression.
// Creates exactly one visible top-level window with the title passed in argv[1]
// and pumps messages until WM_CLOSE. No registry, network, or virtual camera.
#include <windows.h>
#include <string>

static std::wstring widen_ascii(const std::string& s) {
  return std::wstring(s.begin(), s.end());
}

static LRESULT CALLBACK wnd_proc(HWND h, UINT msg, WPARAM wp, LPARAM lp) {
  switch (msg) {
    case WM_ERASEBKGND:
      return 1;
    case WM_PAINT: {
      PAINTSTRUCT ps{};
      HDC dc = BeginPaint(h, &ps);
      RECT r{};
      GetClientRect(h, &r);
      HBRUSH brush = CreateSolidBrush(RGB(30, 120, 200));
      FillRect(dc, &r, brush);
      DeleteObject(brush);
      EndPaint(h, &ps);
      return 0;
    }
    case WM_CLOSE:
      DestroyWindow(h);
      return 0;
    case WM_DESTROY:
      PostQuitMessage(0);
      return 0;
    default:
      return DefWindowProcW(h, msg, wp, lp);
  }
}

int main(int argc, char** argv) {
  const std::wstring title = widen_ascii(argc > 1 ? std::string(argv[1]) : std::string("VC-WGC-Live"));
  WNDCLASSEXW wc{};
  wc.cbSize = sizeof(wc);
  wc.lpfnWndProc = wnd_proc;
  wc.hInstance = GetModuleHandleW(nullptr);
  wc.hCursor = LoadCursorW(nullptr, MAKEINTRESOURCEW(32512));
  wc.lpszClassName = L"VCWgcLiveWindow";
  if (!RegisterClassExW(&wc)) return 1;
  HWND h = CreateWindowExW(0, L"VCWgcLiveWindow", title.c_str(), WS_OVERLAPPEDWINDOW,
                           100, 100, 900, 700, nullptr, nullptr, wc.hInstance, nullptr);
  if (!h) return 1;
  ShowWindow(h, SW_SHOWNOACTIVATE);
  UpdateWindow(h);
  MSG msg{};
  while (GetMessageW(&msg, nullptr, 0, 0) > 0) {
    TranslateMessage(&msg);
    DispatchMessageW(&msg);
  }
  return 0;
}
