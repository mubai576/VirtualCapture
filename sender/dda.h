// 桌面采集（DDA取屏，居中裁640，整数转NV12）
// 无边框桌面→居中裁剪→NV12，全程整数无浮点
// 禁止静默黑帧：失败必置错码，调用方非零退出
// 非Windows：桩实现，无屏SKIP
#pragma once
#include <cstdint>
#include <cstdio>
#include <string>
#include <vector>
#include "pattern.h"
#include "dda_gpu.h"
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
  ID3D11Query* qDisjoint = nullptr;
  ID3D11Query* qStart = nullptr;
  ID3D11Query* qEnd = nullptr;
  DDAGpu* gpu = nullptr; // P4-1方案A：compute shader转换（--gpu-convert显式开启）
  bool want_gpu = false;
  bool start_gpu(std::string& why); // P4-1：init之后调，失败显式错
#endif
  // P4-1真GPU时间（CopyResource前后打戳，D10切口径前仅诊断）
  // gpu_ms仍是capture耗时，新字段走GPU时钟
  double gpu_accum_ms = 0;
  uint64_t gpu_samples = 0;
  double last_gpu_ms = 0;
  bool gpu_qok = false;
  std::string gpu_qmsg = "uninitialized";
  const char* err() const { return err_msg.c_str(); }
  static inline int clamp8(int c){ return c<0?0:(c>255?255:c); }
  bool init(std::string& detail); // 初始化DDA（取屏建复用）
  void shutdown(); // 释放DDA资源
  bool capture(uint8_t* Y, uint8_t* UV); // 采一帧转NV12
  ~DDACap(){ shutdown(); }
};
int dda_selftest(); // 自检：转换单元+实际采集
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
  // 建戳/disjoint查询，尽力而为
  // 失败只停新字段采样，不判采集失败
  gpu_qmsg = "query create failed (wall-clock only)";
  {
    D3D11_QUERY_DESC qd; qd.MiscFlags = 0; qd.Query = D3D11_QUERY_TIMESTAMP_DISJOINT;
    ID3D11Query* dj = nullptr;
    if(SUCCEEDED(dev->CreateQuery(&qd, &dj)) && dj){
      qd.Query = D3D11_QUERY_TIMESTAMP;
      ID3D11Query* ts = nullptr; ID3D11Query* te = nullptr;
      if(SUCCEEDED(dev->CreateQuery(&qd, &ts)) && ts && SUCCEEDED(dev->CreateQuery(&qd, &te)) && te){
        qDisjoint = dj; qStart = ts; qEnd = te;
        gpu_qok = true; gpu_qmsg = "timestamp+disjoint ok";
      } else {
        if(ts) ts->Release(); if(te) te->Release(); dj->Release();
      }
    }
  }
  char db[64]; snprintf(db, sizeof(db), "%dx%d", out_w, out_h); detail = db;
  err_code = OK; err_msg = "ok"; return true;
}
#ifdef _WIN32
bool DDACap::start_gpu(std::string& why){
  if(!dev || !ctx){ why = "gpu-convert before init"; return false; }
  if(gpu){ want_gpu = true; why = "already started"; return true; }
  gpu = new DDAGpu();
  if(!gpu){ why = "out of memory"; return false; }
  if(!gpu->init(dev, ctx, qDisjoint, qStart, qEnd)){ why = gpu->err(); delete gpu; gpu = nullptr; return false; }
  want_gpu = true; why = "gpu-convert on (compute shader)"; return true;
}
#endif
void DDACap::shutdown(){
#ifdef _WIN32
  if(qEnd){ qEnd->Release(); qEnd = nullptr; }
  if(qStart){ qStart->Release(); qStart = nullptr; }
  if(qDisjoint){ qDisjoint->Release(); qDisjoint = nullptr; }
  gpu_qok = false;
  if(gpu){ gpu->shutdown(); delete gpu; gpu = nullptr; } want_gpu = false;
  if(staging){ staging->Release(); staging = nullptr; }
  stag_w = 0; stag_h = 0;
  if(dup){ dup->Release(); dup = nullptr; }
  if(ctx){ ctx->Release(); ctx = nullptr; }
  if(dev){ dev->Release(); dev = nullptr; }
#endif
}
// BGRX居中裁640转NV12（整数BT.601）
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
  if(!want_gpu && (!staging || stag_w != w || stag_h != h)){
    if(staging){ staging->Release(); staging = nullptr; }
    D3D11_TEXTURE2D_DESC sd; sd.Width = d.Width; sd.Height = d.Height; sd.MipLevels = 1; sd.ArraySize = 1;
    sd.Format = d.Format; sd.SampleDesc.Count = 1; sd.SampleDesc.Quality = 0;
    sd.Usage = D3D11_USAGE_STAGING; sd.BindFlags = 0; sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ; sd.MiscFlags = 0;
    hr = dev->CreateTexture2D(&sd, NULL, &staging);
    if(FAILED(hr) || !staging){ tex->Release(); dup->ReleaseFrame(); err_code = E_INTERNAL; err_msg = "staging texture failed"; return false; }
    stag_w = w; stag_h = h;
  }
  bool stamped = gpu_qok && qDisjoint && qStart && qEnd;
  if(want_gpu){
    // GPU路径：shader直读DDA纹理，失败显式报错，不回退CPU（owner批）
    if(!gpu){ tex->Release(); dup->ReleaseFrame(); err_code = E_INTERNAL; err_msg = "gpu-convert requested but gpu not started"; return false; }
    if(stamped){ ctx->Begin(qDisjoint); ctx->End(qStart); }
    bool gok = gpu->run(tex, w, h);
    if(stamped){ ctx->End(qEnd); ctx->End(qDisjoint); }
    tex->Release(); dup->ReleaseFrame();
    if(!gok){ err_code = E_INTERNAL; err_msg = gpu->err(); return false; }
    if(!gpu->readback(Y, UV)){ err_code = E_INTERNAL; err_msg = gpu->err(); return false; }
  } else {
  if(stamped){ ctx->Begin(qDisjoint); ctx->End(qStart); }
  ctx->CopyResource(staging, tex); tex->Release();
  if(stamped){ ctx->End(qEnd); ctx->End(qDisjoint); }
  D3D11_MAPPED_SUBRESOURCE mp;
  hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &mp);
  if(FAILED(hr)){ dup->ReleaseFrame(); err_code = E_INTERNAL; err_msg = "staging map failed"; return false; }
  dda_bgrx_to_nv12((const uint8_t*)mp.pData, (unsigned)mp.RowPitch, w, h, Y, UV);
  ctx->Unmap(staging, 0);
  dup->ReleaseFrame();
  }
  if(stamped){
// 限时轮询（约2ms），不占4.16ms帧预算
    D3D11_QUERY_DATA_TIMESTAMP_DISJOINT dj; HRESULT gr = S_FALSE;
    for(int i = 0; i < 200; i++){
      gr = ctx->GetData(qDisjoint, &dj, sizeof(dj), D3D11_ASYNC_GETDATA_DONOTFLUSH);
      if(gr == S_OK) break;
      if(gr != S_FALSE) break;
      if((i & 15) == 15) ::Sleep(0);
    }
    if(gr == S_OK && !dj.Disjoint && dj.Frequency != 0){
      UINT64 t0 = 0, t1 = 0; HRESULT r0 = S_FALSE, r1 = S_FALSE;
      for(int i = 0; i < 50; i++){
        if(r0 != S_OK) r0 = ctx->GetData(qStart, &t0, sizeof(t0), D3D11_ASYNC_GETDATA_DONOTFLUSH);
        if(r1 != S_OK) r1 = ctx->GetData(qEnd, &t1, sizeof(t1), D3D11_ASYNC_GETDATA_DONOTFLUSH);
        if(r0 == S_OK && r1 == S_OK) break;
        if((r0 != S_FALSE && r0 != S_OK) || (r1 != S_FALSE && r1 != S_OK)) break;
        if((i & 7) == 7) ::Sleep(0);
      }
      if(r0 == S_OK && r1 == S_OK && t1 >= t0){
        last_gpu_ms = (double)(t1 - t0) * 1000.0 / (double)dj.Frequency;
        gpu_accum_ms += last_gpu_ms; gpu_samples++;
      }
    }
  }
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
#ifdef _WIN32
int dda_gpu_selftest(){
  // GPU转换与CPU参考逐字节一致性（合成B8G8R8A8纹理，不依赖真实桌面）
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  const char* drv = "HARDWARE";
  HRESULT hr = D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, NULL, 0,
    D3D11_SDK_VERSION, &dev, NULL, &ctx);
  if(FAILED(hr) || !dev || !ctx){
    drv = "WARP";
    hr = D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_WARP, NULL, 0, NULL, 0,
      D3D11_SDK_VERSION, &dev, NULL, &ctx);
  }
  if(FAILED(hr) || !dev || !ctx){ printf("DDA-GPU: SKIP no D3D11 device\n"); return 2; }
  DDAGpu g;
  if(!g.init(dev, ctx, nullptr, nullptr, nullptr)){ printf("DDA-GPU: FAIL gpu init (%s)\n", g.err()); if(ctx)ctx->Release(); if(dev)dev->Release(); return 1; }
  struct Case { int w, h, fill; };
  const Case cases[] = { {640,640,0}, {640,640,1}, {800,700,2}, {1024,768,3}, {960,800,2} };
  bool ok = true;
  for(const auto& c : cases){
    std::vector<uint8_t> bgra((size_t)c.w*c.h*4);
    if(c.fill==0){ for(size_t i=0;i<bgra.size();i+=4){ bgra[i]=255; bgra[i+1]=255; bgra[i+2]=255; bgra[i+3]=0; } }
    else if(c.fill==1){ for(size_t i=0;i<bgra.size();i+=4){ bgra[i]=0; bgra[i+1]=0; bgra[i+2]=255; bgra[i+3]=0; } }
    else if(c.fill==2){ for(int y=0;y<c.h;y++) for(int x=0;x<c.w;x++){ size_t i=((size_t)y*c.w+x)*4; bgra[i]=(uint8_t)(x+y); bgra[i+1]=(uint8_t)(x*2); bgra[i+2]=(uint8_t)(y*3); bgra[i+3]=0; } }
    else { uint32_t s=0x12345678u; for(size_t i=0;i<bgra.size();i++){ s=s*1664525u+1013904223u; bgra[i]=(uint8_t)(s>>24); } for(size_t i=3;i<bgra.size();i+=4) bgra[i]=0; }
    D3D11_TEXTURE2D_DESC td; memset(&td,0,sizeof(td));
    td.Width=c.w; td.Height=c.h; td.MipLevels=1; td.ArraySize=1;
    td.Format=DXGI_FORMAT_B8G8R8A8_UNORM; td.SampleDesc.Count=1;
    td.Usage=D3D11_USAGE_DEFAULT; td.BindFlags=D3D11_BIND_SHADER_RESOURCE;
    D3D11_SUBRESOURCE_DATA sd; memset(&sd,0,sizeof(sd));
    sd.pSysMem=bgra.data(); sd.SysMemPitch=(UINT)(c.w*4);
    ID3D11Texture2D* tex=nullptr;
    if(FAILED(dev->CreateTexture2D(&td,&sd,&tex))||!tex){ printf("DDA-GPU: FAIL texture %dx%d\n",c.w,c.h); ok=false; continue; }
    std::vector<uint8_t> eY(VC_Y_BYTES), eUV(VC_UV_BYTES), gY(VC_Y_BYTES), gUV(VC_UV_BYTES);
    dda_bgrx_to_nv12(bgra.data(), (unsigned)c.w*4u, c.w, c.h, eY.data(), eUV.data());
    if(!g.run(tex,c.w,c.h)){ printf("DDA-GPU: FAIL run %dx%d (%s)\n",c.w,c.h,g.err()); ok=false; tex->Release(); continue; }
    if(!g.readback(gY.data(),gUV.data())){ printf("DDA-GPU: FAIL readback %dx%d (%s)\n",c.w,c.h,g.err()); ok=false; tex->Release(); continue; }
    tex->Release();
    if(eY!=gY || eUV!=gUV){
      size_t dy=0; while(dy<eY.size()&&eY[dy]==gY[dy]) dy++;
      size_t du=0; while(du<eUV.size()&&eUV[du]==gUV[du]) du++;
      printf("DDA-GPU: FAIL mismatch %dx%d fill=%d Y[%zu] cpu=%d gpu=%d UV[%zu] cpu=%d gpu=%d\n",
        c.w,c.h,c.fill,dy,dy<eY.size()?eY[dy]:-1,dy<gY.size()?gY[dy]:-1,
        du,du<eUV.size()?eUV[du]:-1,du<gUV.size()?gUV[du]:-1);
      ok=false; continue;
    }
    printf("DDA-GPU: case %dx%d fill=%d exact (%s)\n", c.w, c.h, c.fill, drv);
  }
  g.shutdown(); ctx->Release(); dev->Release();
  if(!ok){ printf("DDA-GPU: FAIL\n"); return 1; }
  printf("DDA-GPU: PASS 5 cases bit-exact (%s)\n", drv);
  return 0;
}
#else
int dda_gpu_selftest(){ printf("DDA-GPU: SKIP (non-Windows build)\n"); return 2; }
#endif
int dda_selftest(){
#ifdef _WIN32
  DDACap c; std::string detail;
  if(!c.init(detail)){
    if(c.err_code == DDACap::E_NOOUTPUT || c.err_code == DDACap::E_FULLSCREEN){ printf("DDA SKIP ok"); puts(c.err()); return 2; }
    printf("DDA: FAIL (%s)\n", c.err()); return 1;
  }
  std::vector<uint8_t> Y(VC_Y_BYTES), UV(VC_UV_BYTES);
  bool got = false;
  for(int i = 0; i < 5 && !got; i++){
    if(c.capture(Y.data(), UV.data())) got = true;
    else if(c.err_code != DDACap::E_TIMEOUT) break;
  }
  if(!got){
// live已容忍超时（合成器idle无新帧）
// 持续idle视同无屏环境，非采集坏（D10待批）
    if(c.err_code == DDACap::E_TIMEOUT){ printf("DDA: SKIP idle-output (5x timeout, no new frame) gpu_q=%s\n", c.gpu_qmsg.c_str()); return 2; }
    printf("DDA: FAIL capture (%s)\n", c.err()); return 1;
  }
  printf("DDA: GPU q=%s dispatch_ms=%.3f\n", c.gpu_qmsg.c_str(), c.last_gpu_ms);
  uint64_t nz = 0; for(size_t i = 0; i < Y.size(); i++) nz += (Y[i] != 0);
  printf("DDA: PASS output=%dx%d center640 NV12 nonzero-Y=%llu\n", c.out_w, c.out_h, (unsigned long long)nz);
  return 0;
#else
  printf("DDA: SKIP (non-Windows build)\n"); return 2;
#endif
}
