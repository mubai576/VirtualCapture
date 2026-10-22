// dda.h - T-05/T-10 DDA capture minimal (Windows D3D11 Desktop Duplication).
// Borderless desktop -> center 640x640 crop -> NV12, integer BT.601, no float anywhere.
// Contract: NO silent black frames. Every failure sets an explicit err; caller must exit non-zero.
// Non-Windows: stub (init fails E_NODDA, selftest SKIP).
#pragma once
#include <cstdint>
#include <cstdio>
#include <string>
#include <vector>
#include "pattern.h"
#ifdef _WIN32
#include <d3d11.h>
#include <dxgi1_2.h>
#endif
struct DDACap {
  enum Err { OK=0, E_NODDA=1, E_FULLSCREEN=2, E_FORMAT=3, E_SMALL=4, E_LOST=5, E_TIMEOUT=6, E_INTERNAL=7, E_NOOUTPUT=8 };
  Err err_code = E_NODDA;
  std::string err_msg = "uninitialized";
  int out_w = 0, out_h = 0;
#ifdef _WIN32
  ID3D11Device* dev = nullptr;
  ID3D11DeviceContext* ctx = nullptr;
  IDXGIOutputDuplication* dup = nullptr;
  ID3D11Texture2D* staging = nullptr;
  int stag_w = 0, stag_h = 0;
#endif
  const char* err() const { return err_msg.c_str(); }
  static inline int clamp8(int c){ return c<0?0:(c>255?255:c); }
  bool init(std::string& detail);
  void shutdown();
  bool capture(uint8_t* Y, uint8_t* UV);
  ~DDACap(){ shutdown(); }
};
int dda_selftest();
#ifdef _WIN32
bool DDACap::init(std::string& detail){
  shutdown();
  err_code = E_NODDA; err_msg = "D3D11CreateDevice failed";
  HRESULT hr = D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, NULL, 0,
    D3D11_SDK_VERSION, &dev, NULL, &ctx);
  if(FAILED(hr)||!dev||!ctx){ shutdown(); return false; }
  IDXGIDevice* dxgi_dev = nullptr;
  IDXGIAdapter* adapter = nullptr;
  IDXGIOutput* output = nullptr;
  IDXGIOutput1* output1 = nullptr;
  hr = dev->QueryInterface(__uuidof(IDXGIDevice), (void**)&dxgi_dev);
  if(FAILED(hr)||!dxgi_dev){ shutdown(); err_msg = "no DXGI device"; return false; }
  hr = dxgi_dev->GetAdapter(&adapter); dxgi_dev->Release();
  if(FAILED(hr)||!adapter){ shutdown(); err_msg = "no DXGI adapter"; return false; }
  hr = adapter->EnumOutputs(0, &output); adapter->Release();
  if(FAILED(hr)||!output){ shutdown(); err_code = E_NOOUTPUT; err_msg = "no outputs headless"; return false; }
  DXGI_OUTPUT_DESC od; output->GetDesc(&od); out_w = (int)od.DesktopCoordinates.right - (int)od.DesktopCoordinates.left;
  out_h = (int)od.DesktopCoordinates.bottom - (int)od.DesktopCoordinates.top;
  hr = output->QueryInterface(__uuidof(IDXGIOutput1), (void**)&output1); output->Release();
  if(FAILED(hr)||!output1){ shutdown(); err_msg = "Output1 unavailable"; return false; }
  hr = output1->DuplicateOutput(dev, &dup); output1->Release();
  if(hr==DXGI_ERROR_UNSUPPORTED||hr==E_ACCESSDENIED){ shutdown(); err_code = E_FULLSCREEN; err_msg = "exclusive fullscreen or protected output"; return false; }
  if(FAILED(hr)||!dup){ shutdown(); err_code = E_INTERNAL; err_msg = "DuplicateOutput failed"; return false; }
  char db[64]; snprintf(db, sizeof(db), "%dx%d", out_w, out_h); detail = db;
  err_code = OK; err_msg = "ok"; return true;
}
void DDACap::shutdown(){
#ifdef _WIN32
  if(staging){ staging->Release(); staging = nullptr; }
  stag_w = 0; stag_h = 0;
  if(dup){ dup->Release(); dup = nullptr; }
  if(ctx){ ctx->Release(); ctx = nullptr; }
  if(dev){ dev->Release(); dev = nullptr; }
#endif
}
static inline void dda_bgrx_to_nv12(const uint8_t* base, unsigned pitch, int w, int h, uint8_t* Y, uint8_t* UV){
  int ox = (w - 640) / 2, oy = (h - 640) / 2;
  for(int y = 0; y < 640; y++){
    const uint8_t* s0 = base + (unsigned)(oy + y) * pitch + (unsigned)ox * 4u;
    uint8_t* dy = Y + (unsigned)y * 640u;
    for(int x = 0; x < 640; x++){ int b = s0[x*4], g = s0[x*4+1], r = s0[x*4+2]; dy[x] = (uint8_t)((77*r+150*g+29*b)>>8); }
    if((y & 1) == 0){
      const uint8_t* s1 = base + (unsigned)(oy + y + 1) * pitch + (unsigned)ox * 4u;
      uint8_t* duv = UV + (unsigned)(y >> 1) * 640u;
      for(int x = 0; x < 640; x += 2){
        int r = (int)s0[x*4+2]+(int)s0[x*4+6]+(int)s1[x*4+2]+(int)s1[x*4+6];
        int g = (int)s0[x*4+1]+(int)s0[x*4+5]+(int)s1[x*4+1]+(int)s1[x*4+5];
        int b = (int)s0[x*4]+(int)s0[x*4+4]+(int)s1[x*4]+(int)s1[x*4+4];
        r >>= 2; g >>= 2; b >>= 2;
        duv[x] = (uint8_t)DDACap::clamp8(((-43*r-84*g+127*b)>>8)+128);
        duv[x+1] = (uint8_t)DDACap::clamp8(((127*r-106*g-21*b)>>8)+128);
      }
    }
  }
}
bool DDACap::capture(uint8_t* Y, uint8_t* UV){
#ifdef _WIN32
  if(!dup){ err_code = E_NODDA; err_msg = "not initialized"; return false; }
  DXGI_OUTDUPL_FRAME_INFO info; IDXGIResource* res = nullptr;
  HRESULT hr = dup->AcquireNextFrame(500, &info, &res);
  if(hr == DXGI_ERROR_WAIT_TIMEOUT){ err_code = E_TIMEOUT; err_msg = "AcquireNextFrame timeout (output idle?)"; return false; }
  if(hr == DXGI_ERROR_ACCESS_LOST || hr == DXGI_ERROR_INVALID_CALL){ err_code = E_LOST; err_msg = "output lost (mode change?)"; return false; }
  if(FAILED(hr) || !res){ err_code = E_INTERNAL; err_msg = "AcquireNextFrame failed"; return false; }
  ID3D11Texture2D* tex = nullptr;
  hr = res->QueryInterface(__uuidof(ID3D11Texture2D), (void**)&tex); res->Release();
  if(FAILED(hr) || !tex){ dup->ReleaseFrame(); err_code = E_INTERNAL; err_msg = "frame is not a 2D texture"; return false; }
  D3D11_TEXTURE2D_DESC d; tex->GetDesc(&d);
  int w = (int)d.Width, h = (int)d.Height;
  if(d.Format != DXGI_FORMAT_B8G8R8A8_UNORM){ tex->Release(); dup->ReleaseFrame(); err_code = E_FORMAT; err_msg = "only B8G8R8A8 supported"; return false; }
  if(w < 640 || h < 640){ tex->Release(); dup->ReleaseFrame(); err_code = E_SMALL; err_msg = "desktop smaller than 640"; return false; }
  if(!staging || stag_w != w || stag_h != h){
    if(staging){ staging->Release(); staging = nullptr; }
    D3D11_TEXTURE2D_DESC sd; sd.Width = d.Width; sd.Height = d.Height; sd.MipLevels = 1; sd.ArraySize = 1;
    sd.Format = d.Format; sd.SampleDesc.Count = 1; sd.SampleDesc.Quality = 0;
    sd.Usage = D3D11_USAGE_STAGING; sd.BindFlags = 0; sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ; sd.MiscFlags = 0;
    hr = dev->CreateTexture2D(&sd, NULL, &staging);
    if(FAILED(hr) || !staging){ tex->Release(); dup->ReleaseFrame(); err_code = E_INTERNAL; err_msg = "staging texture failed"; return false; }
    stag_w = w; stag_h = h;
  }
  ctx->CopyResource(staging, tex); tex->Release();
  D3D11_MAPPED_SUBRESOURCE mp;
  hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &mp);
  if(FAILED(hr)){ dup->ReleaseFrame(); err_code = E_INTERNAL; err_msg = "staging map failed"; return false; }
  dda_bgrx_to_nv12((const uint8_t*)mp.pData, (unsigned)mp.RowPitch, w, h, Y, UV);
  ctx->Unmap(staging, 0);
  dup->ReleaseFrame();
  err_code = OK; err_msg = "ok"; return true;
#else
  (void)Y; (void)UV;
  err_code = E_NODDA; err_msg = "non-Windows build"; return false;
#endif
}
#else
bool DDACap::init(std::string& detail){ (void)detail; shutdown(); err_code = E_NODDA; err_msg = "non-Windows build"; return false; }
bool DDACap::capture(uint8_t* Y, uint8_t* UV){ (void)Y; (void)UV; err_code = E_NODDA; err_msg = "non-Windows build"; return false; }
#endif
int dda_selftest(){
#ifdef _WIN32
  DDACap c; std::string detail;
  if(!c.init(detail)){
    if(c.err_code == DDACap::E_NOOUTPUT || c.err_code == DDACap::E_FULLSCREEN){ printf("DDA SKIP ok"); puts(c.err()); return 2; }
    printf("DDA: FAIL (%s)\n", c.err()); return 1;
  }
  std::vector<uint8_t> Y(VC_Y_BYTES), UV(VC_UV_BYTES);
  if(!c.capture(Y.data(), UV.data())){ printf("DDA: FAIL capture (%s)\n", c.err()); return 1; }
  uint64_t nz = 0; for(size_t i = 0; i < Y.size(); i++) nz += (Y[i] != 0);
  printf("DDA: PASS output=%dx%d center640 NV12 nonzero-Y=%llu\n", c.out_w, c.out_h, (unsigned long long)nz);
  return 0;
#else
  printf("DDA: SKIP (non-Windows build)\n"); return 2;
#endif
}
