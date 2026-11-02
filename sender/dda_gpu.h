#pragma once
// GPU转换：compute shader 居中裁640 + 整数BT.601直出NV12（P4-1方案A）
// 与 dda_bgrx_to_nv12 逐字节一致；任何失败一律显式报错，不静默回退CPU
// 非Windows：桩结构，selftest报SKIP
#include <cstdint>
#include <cstdio>
#include <string>
#include <vector>
#include "pattern.h"
#include "../common/stream.h"
#ifdef _WIN32
#include <d3d11.h>
#include <d3dcompiler.h>
#endif

#ifdef _WIN32
// HLSL：输入B8G8R8A8_UNORM用Load精确取数，反归一化 floor(v*255+0.5) 对8位精确
// 数学与 dda_bgrx_to_nv12 完全同构：Y=(77r+150g+29b)>>8；2x2均值(>>2)后
// U=((−43r−84g+127b)>>8)+128，V=((127r−106g−21b)>>8)+128，clamp8
static const char* kDdaGpuHLSText = R"HLSL(
Texture2D<float4> g_src : register(t0);
RWStructuredBuffer<uint> g_y : register(u0);
RWStructuredBuffer<uint> g_uv : register(u1);
cbuffer CC : register(b0) {
  uint ox,oy,rw,rh; uint dx,dy,dw,dh; uint width,height,pad0,pad1;
};
int C8(int v){ return clamp(v,0,255); }
int3 RGB(uint x,uint y){
  if(x<dx || y<dy || x>=dx+dw || y>=dy+dh) return int3(0,0,0);
  float4 c=g_src.Load(int3(ox+(x-dx)*rw/dw,oy+(y-dy)*rh/dh,0));
  return int3(c.xyz*255.0f+0.5f);
}
[numthreads(64,1,1)]
void CSY(uint3 tid : SV_DispatchThreadID){
  uint index=tid.x+tid.y*4194240u;
  uint off=index*4, n=width*height;
  if(off>=n) return;
  uint packed=0;
  for(uint k=0;k<4 && off+k<n;k++){
    uint pos=off+k;
    int3 c=RGB(pos%width,pos/width);
    packed |= uint((77*c.x+150*c.y+29*c.z)>>8)<<(k*8);
  }
  g_y[index]=packed;
}
[numthreads(64,1,1)]
void CSU(uint3 tid : SV_DispatchThreadID){
  uint index=tid.x+tid.y*4194240u;
  uint off=index*4, n=width*height/2;
  if(off>=n) return;
  uint packed=0;
  for(uint k=0;k<2 && off+k*2<n;k++){
    uint pos=off+k*2, x=pos%width, y=pos/width*2;
    int3 c=(RGB(x,y)+RGB(x+1,y)+RGB(x,y+1)+RGB(x+1,y+1))>>2;
    uint u=uint(C8(((-43*c.x-84*c.y+127*c.z)>>8)+128));
    uint v=uint(C8(((127*c.x-106*c.y-21*c.z)>>8)+128));
    packed |= u<<(k*16); packed |= v<<(k*16+8);
  }
  g_uv[index]=packed;
}
)HLSL";

struct DDAGpu {
  VCStream stream;
  ID3D11Device* dev = nullptr;
  ID3D11DeviceContext* ctx = nullptr;
  ID3D11Query *qDisjoint = nullptr, *qStart = nullptr, *qEnd = nullptr;
  ID3D11ComputeShader *csY = nullptr, *csU = nullptr;
  ID3D11Buffer *yBuf = nullptr, *uvBuf = nullptr;
  ID3D11Buffer *yStag = nullptr, *uvStag = nullptr;
  ID3D11UnorderedAccessView *yUAV = nullptr, *uvUAV = nullptr;
  ID3D11ShaderResourceView* srv = nullptr;
  ID3D11Buffer* cbuf = nullptr;
  ID3D11Texture2D* srcTex = nullptr; // 当前SRV绑定的源（不拥有）
  std::string errmsg = "uninitialized";
  const char* err() const { return errmsg.c_str(); }
  void shutdown() {
    if (srv) { srv->Release(); srv = nullptr; }
    if (cbuf) { cbuf->Release(); cbuf = nullptr; }
    if (yUAV) { yUAV->Release(); yUAV = nullptr; }
    if (uvUAV) { uvUAV->Release(); uvUAV = nullptr; }
    if (yStag) { yStag->Release(); yStag = nullptr; }
    if (uvStag) { uvStag->Release(); uvStag = nullptr; }
    if (yBuf) { yBuf->Release(); yBuf = nullptr; }
    if (uvBuf) { uvBuf->Release(); uvBuf = nullptr; }
    if (csU) { csU->Release(); csU = nullptr; }
    if (csY) { csY->Release(); csY = nullptr; }
    srcTex = nullptr; dev = nullptr; ctx = nullptr;
    qDisjoint = qStart = qEnd = nullptr;
  }
  ~DDAGpu() { shutdown(); }
  bool init(ID3D11Device* d, ID3D11DeviceContext* c,
            ID3D11Query* dj, ID3D11Query* ts, ID3D11Query* te, const VCStream& config=VCStream()) {
    shutdown();
    stream=config;
    if(!stream.valid()) { errmsg="invalid stream geometry"; return false; }
    if (!d || !c) { errmsg = "null device/context"; return false; }
    dev = d; ctx = c; qDisjoint = dj; qStart = ts; qEnd = te;
    ID3DBlob* blob = nullptr; ID3DBlob* errs = nullptr;
    HRESULT hr = D3DCompile(kDdaGpuHLSText, strlen(kDdaGpuHLSText), "dda_gpu", nullptr, nullptr,
                            "CSY", "cs_5_0", D3DCOMPILE_ENABLE_STRICTNESS, 0, &blob, &errs);
    if (FAILED(hr) || !blob) {
      errmsg = std::string("CSY compile failed: ") + (errs ? (const char*)errs->GetBufferPointer() : "?");
      if (errs) errs->Release(); return false;
    }
    hr = dev->CreateComputeShader(blob->GetBufferPointer(), blob->GetBufferSize(), nullptr, &csY);
    blob->Release();
    if (FAILED(hr) || !csY) { errmsg = "CreateComputeShader CSY failed"; return false; }
    blob = nullptr; errs = nullptr;
    hr = D3DCompile(kDdaGpuHLSText, strlen(kDdaGpuHLSText), "dda_gpu", nullptr, nullptr,
                    "CSU", "cs_5_0", D3DCOMPILE_ENABLE_STRICTNESS, 0, &blob, &errs);
    if (FAILED(hr) || !blob) {
      errmsg = std::string("CSU compile failed: ") + (errs ? (const char*)errs->GetBufferPointer() : "?");
      if (errs) errs->Release(); return false;
    }
    hr = dev->CreateComputeShader(blob->GetBufferPointer(), blob->GetBufferSize(), nullptr, &csU);
    blob->Release();
    if (FAILED(hr) || !csU) { errmsg = "CreateComputeShader CSU failed"; return false; }
    const size_t yN = (stream.y_bytes()+3)/4, uvN = (stream.uv_bytes()+3)/4;
    D3D11_BUFFER_DESC bd; memset(&bd, 0, sizeof(bd));
    bd.ByteWidth = (UINT)(yN * 4); bd.Usage = D3D11_USAGE_DEFAULT;
    bd.BindFlags = D3D11_BIND_UNORDERED_ACCESS; bd.MiscFlags = D3D11_RESOURCE_MISC_BUFFER_STRUCTURED;
    bd.StructureByteStride = 4;
    if (FAILED(dev->CreateBuffer(&bd, nullptr, &yBuf))) { errmsg = "yBuf failed"; return false; }
    bd.ByteWidth = (UINT)(uvN * 4);
    if (FAILED(dev->CreateBuffer(&bd, nullptr, &uvBuf))) { errmsg = "uvBuf failed"; return false; }
    D3D11_UNORDERED_ACCESS_VIEW_DESC ud; memset(&ud, 0, sizeof(ud));
    ud.Format = DXGI_FORMAT_UNKNOWN; ud.ViewDimension = D3D11_UAV_DIMENSION_BUFFER;
    ud.Buffer.FirstElement = 0; ud.Buffer.NumElements = (UINT)yN;
    if (FAILED(dev->CreateUnorderedAccessView(yBuf, &ud, &yUAV))) { errmsg = "yUAV failed"; return false; }
    ud.Buffer.NumElements = (UINT)uvN;
    if (FAILED(dev->CreateUnorderedAccessView(uvBuf, &ud, &uvUAV))) { errmsg = "uvUAV failed"; return false; }
    D3D11_BUFFER_DESC sd; memset(&sd, 0, sizeof(sd));
    sd.ByteWidth = (UINT)(yN * 4); sd.Usage = D3D11_USAGE_STAGING;
    sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    sd.MiscFlags = D3D11_RESOURCE_MISC_BUFFER_STRUCTURED; sd.StructureByteStride = 4;
    if (FAILED(dev->CreateBuffer(&sd, nullptr, &yStag))) { errmsg = "yStag failed"; return false; }
    sd.ByteWidth = (UINT)(uvN * 4);
    if (FAILED(dev->CreateBuffer(&sd, nullptr, &uvStag))) { errmsg = "uvStag failed"; return false; }
    D3D11_BUFFER_DESC cd; memset(&cd, 0, sizeof(cd));
    cd.ByteWidth = 48; cd.Usage = D3D11_USAGE_DEFAULT; cd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    if (FAILED(dev->CreateBuffer(&cd, nullptr, &cbuf))) { errmsg = "cbuf failed"; return false; }
    errmsg = "ok"; return true;
  }
  // 源纹理（B8G8R8A8_UNORM）上建SRV + 更新裁剪常量；纹理切换时重建
  bool ensure_src(ID3D11Texture2D* tex, int w, int h) {
    VCTransform t;
    if (!tex || !t.init(w,h,stream)) { errmsg = "bad src texture"; return false; }
    if (tex != srcTex) {
      if (srv) { srv->Release(); srv = nullptr; }
      D3D11_SHADER_RESOURCE_VIEW_DESC vd; memset(&vd, 0, sizeof(vd));
      vd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
      vd.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
      vd.Texture2D.MostDetailedMip = 0; vd.Texture2D.MipLevels = 1;
      if (FAILED(dev->CreateShaderResourceView(tex, &vd, &srv))) { errmsg = "SRV failed"; return false; }
      srcTex = tex;
    }
    uint32_t cc[12]={t.ox,t.oy,t.rw,t.rh,t.dx,t.dy,t.dw,t.dh,t.width,t.height,0,0};
    ctx->UpdateSubresource(cbuf, 0, nullptr, cc, 0, 0);
    return true;
  }
  // GPU工作：dispatch + 拷到staging（timestamp包住这段即真GPU时间）
  bool run(ID3D11Texture2D* tex, int w, int h) {
    if (!csY || !csU || !yUAV || !uvUAV || !cbuf) { errmsg = "not initialized"; return false; }
    if (!ensure_src(tex, w, h)) return false;
    ID3D11ShaderResourceView* srvs[1] = { srv };
    ID3D11UnorderedAccessView* uavs[2] = { yUAV, uvUAV };
    ID3D11Buffer* cbs[1] = { cbuf };
    const size_t yN = (stream.y_bytes()+3)/4, uvN = (stream.uv_bytes()+3)/4;
    ctx->CSSetConstantBuffers(0, 1, cbs);
    ctx->CSSetShaderResources(0, 1, srvs);
    ctx->CSSetUnorderedAccessViews(0, 2, uavs, nullptr);
    ctx->CSSetShader(csY, nullptr, 0);
    size_t gy=(yN+63)/64;
    ctx->Dispatch((UINT)std::min(gy,(size_t)65535),(UINT)((gy+65534)/65535),1);
    ctx->CSSetShader(csU, nullptr, 0);
    size_t gu=(uvN+63)/64;
    ctx->Dispatch((UINT)std::min(gu,(size_t)65535),(UINT)((gu+65534)/65535),1);
    ID3D11ShaderResourceView* nsrv[1] = { nullptr };
    ID3D11UnorderedAccessView* nuav[2] = { nullptr, nullptr };
    ID3D11Buffer* ncb[1] = { nullptr };
    ctx->CSSetUnorderedAccessViews(0, 2, nuav, nullptr);
    ctx->CSSetShaderResources(0, 1, nsrv);
    ctx->CSSetConstantBuffers(0, 1, ncb);
    ctx->CSSetShader(nullptr, nullptr, 0);
    ctx->CopyResource(yStag, yBuf);
    ctx->CopyResource(uvStag, uvBuf);
    return true;
  }
  bool readback(uint8_t* Y, uint8_t* UV) {
    if (!Y || !UV) { errmsg = "null output"; return false; }
    D3D11_MAPPED_SUBRESOURCE my, muv;
    if (FAILED(ctx->Map(yStag, 0, D3D11_MAP_READ, 0, &my))) { errmsg = "yStag map failed"; return false; }
    memcpy(Y, my.pData, stream.y_bytes());
    ctx->Unmap(yStag, 0);
    if (FAILED(ctx->Map(uvStag, 0, D3D11_MAP_READ, 0, &muv))) { errmsg = "uvStag map failed"; return false; }
    memcpy(UV, muv.pData, stream.uv_bytes());
    ctx->Unmap(uvStag, 0);
    return true;
  }
};
#else
struct DDAGpu { // 非Windows桩
  const char* err() const { return "non-Windows build"; }
};
#endif
