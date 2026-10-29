// tools/testwin/testwin.cpp



//



#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <dxgi1_5.h>
#include <d3dcompiler.h>
#include <stdint.h>
#include <stdio.h>
#include <math.h>
#pragma comment(lib, "d3d11.lib")
#pragma comment(lib, "dxgi.lib")
#pragma comment(lib, "d3dcompiler.lib")

static const int WIN_W = 960, WIN_H = 720;
static const wchar_t* WCLASS = L"VCTestWin";

// ------ HLSL ------

static const char* kCS = R"HLSL(
cbuffer CB : register(b0) {
    uint frame_id;
    uint pad0, pad1, pad2;
}
RWTexture2D<unorm float4> OutTex : register(u0);
[numthreads(8,8,1)]
void CS(uint3 tid : SV_DispatchThreadID) {
    uint x = tid.x, y = tid.y;
    
    uint s = (frame_id * 1664525u + x * 22695477u + y * 1013904223u) ^ (frame_id << 13);
    s ^= s >> 7; s ^= s << 17; s ^= s >> 5;
    float r = float(s & 0xFF) / 255.0f;
    float g = float((s >> 8) & 0xFF) / 255.0f;
    float b_v = float((s >> 16) & 0xFF) / 255.0f;
    
    float wave = 0.3f * sin(float(x + frame_id) * 0.05f) * sin(float(y + frame_id) * 0.07f);
    OutTex[uint2(x,y)] = float4(saturate(r + wave), saturate(g + wave), saturate(b_v + wave), 1.0f);
}
)HLSL";


static const char* kVS = R"HLSL(
void VS(uint id : SV_VertexID, out float4 pos : SV_Position, out float2 uv : TEXCOORD) {
    uv  = float2((id & 1) ? 1.0f : 0.0f, (id >> 1) ? 1.0f : 0.0f);
    pos = float4(uv.x * 2.0f - 1.0f, 1.0f - uv.y * 2.0f, 0.0f, 1.0f);
}
)HLSL";
static const char* kPS = R"HLSL(
Texture2D<float4> Tex : register(t0);
SamplerState Smp : register(s0);
float4 PS(float4 pos : SV_Position, float2 uv : TEXCOORD) : SV_Target {
    return Tex.Sample(Smp, uv);
}
)HLSL";

// ------ globals ------
static ID3D11Device*           g_dev     = nullptr;
static ID3D11DeviceContext*    g_ctx     = nullptr;
static IDXGISwapChain1*        g_sc      = nullptr;
static ID3D11RenderTargetView* g_rtv     = nullptr;
static ID3D11ComputeShader*    g_cs      = nullptr;
static ID3D11VertexShader*     g_vs      = nullptr;
static ID3D11PixelShader*      g_ps      = nullptr;
static ID3D11Texture2D*        g_tex     = nullptr;
static ID3D11UnorderedAccessView* g_uav  = nullptr;
static ID3D11ShaderResourceView*  g_srv  = nullptr;
static ID3D11Buffer*           g_cb      = nullptr;
static ID3D11SamplerState*     g_smp     = nullptr;
static bool                    g_tearing = false;
static uint32_t                g_fid     = 0;


static LARGE_INTEGER g_qpf, g_qp0;
static int64_t       g_fps_last = 0;
static int           g_fps_count = 0, g_fps_disp = 0;
static wchar_t       g_title[128];
static HWND          g_hwnd = nullptr;

static bool CompileShader(const char* src, const char* ep, const char* tgt, ID3DBlob** out) {
    ID3DBlob* err = nullptr;
    HRESULT hr = D3DCompile(src, strlen(src), nullptr, nullptr, nullptr,
                            ep, tgt, 0, 0, out, &err);
    if (FAILED(hr)) {
        if (err) { OutputDebugStringA((char*)err->GetBufferPointer()); err->Release(); }
        return false;
    }
    if (err) err->Release();
    return true;
}

static bool Init(HWND hwnd) {
    // Tearing support
    IDXGIFactory2* fac = nullptr;
    CreateDXGIFactory1(__uuidof(IDXGIFactory2), (void**)&fac);
    IDXGIFactory5* fac5 = nullptr;
    if (fac) fac->QueryInterface(__uuidof(IDXGIFactory5), (void**)&fac5);
    BOOL tearing = FALSE;
    if (fac5) { fac5->CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &tearing, sizeof(tearing)); fac5->Release(); }
    g_tearing = (tearing == TRUE);

    UINT flags = 0;
    HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, flags,
                                   nullptr, 0, D3D11_SDK_VERSION, &g_dev, nullptr, &g_ctx);
    if (FAILED(hr)) return false;

    // SwapChain
    DXGI_SWAP_CHAIN_DESC1 scd = {};
    scd.Width = WIN_W; scd.Height = WIN_H;
    scd.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    scd.BufferCount = 2;
    scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    scd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    scd.SampleDesc.Count = 1;
    if (g_tearing) scd.Flags = DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;

    IDXGIDevice1* dxgi = nullptr; g_dev->QueryInterface(&dxgi);
    IDXGIAdapter* adp = nullptr;  if (dxgi) { dxgi->GetAdapter(&adp); dxgi->Release(); }
    IDXGIFactory2* fac2 = nullptr;
    if (adp) { adp->GetParent(__uuidof(IDXGIFactory2),(void**)&fac2); adp->Release(); }
    if (fac2) { fac2->CreateSwapChainForHwnd(g_dev, hwnd, &scd, nullptr, nullptr, &g_sc); fac2->Release(); }
    if (fac) fac->Release();
    if (!g_sc) return false;

    // RTV
    ID3D11Texture2D* bb = nullptr;
    g_sc->GetBuffer(0, __uuidof(ID3D11Texture2D), (void**)&bb);
    if (bb) { g_dev->CreateRenderTargetView(bb, nullptr, &g_rtv); bb->Release(); }

    // Compute texture (R8G8B8A8_UNORM, UAV+SRV)
    D3D11_TEXTURE2D_DESC td = {};
    td.Width = WIN_W; td.Height = WIN_H; td.MipLevels = 1; td.ArraySize = 1;
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.BindFlags = D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE;
    g_dev->CreateTexture2D(&td, nullptr, &g_tex);
    if (g_tex) {
        g_dev->CreateUnorderedAccessView(g_tex, nullptr, &g_uav);
        g_dev->CreateShaderResourceView(g_tex, nullptr, &g_srv);
    }

    // Constant buffer
    D3D11_BUFFER_DESC bd = {}; bd.ByteWidth = 16; bd.Usage = D3D11_USAGE_DEFAULT; bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    g_dev->CreateBuffer(&bd, nullptr, &g_cb);

    // Sampler
    D3D11_SAMPLER_DESC sd = {}; sd.Filter = D3D11_FILTER_MIN_MAG_MIP_LINEAR;
    sd.AddressU = sd.AddressV = sd.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
    g_dev->CreateSamplerState(&sd, &g_smp);

    // Shaders
    ID3DBlob* blob = nullptr;
    if (CompileShader(kCS, "CS", "cs_5_0", &blob)) {
        g_dev->CreateComputeShader(blob->GetBufferPointer(), blob->GetBufferSize(), nullptr, &g_cs);
        blob->Release();
    }
    if (CompileShader(kVS, "VS", "vs_5_0", &blob)) {
        g_dev->CreateVertexShader(blob->GetBufferPointer(), blob->GetBufferSize(), nullptr, &g_vs);
        blob->Release();
    }
    if (CompileShader(kPS, "PS", "ps_5_0", &blob)) {
        g_dev->CreatePixelShader(blob->GetBufferPointer(), blob->GetBufferSize(), nullptr, &g_ps);
        blob->Release();
    }

    return g_cs && g_vs && g_ps && g_tex && g_uav && g_srv && g_cb && g_rtv;
}

static void Render() {
    // Update CB with frame_id
    uint32_t cb[4] = { g_fid++, 0, 0, 0 };
    g_ctx->UpdateSubresource(g_cb, 0, nullptr, cb, 0, 0);

    // CS: fill texture
    g_ctx->CSSetShader(g_cs, nullptr, 0);
    g_ctx->CSSetConstantBuffers(0, 1, &g_cb);
    g_ctx->CSSetUnorderedAccessViews(0, 1, &g_uav, nullptr);
    g_ctx->Dispatch((WIN_W + 7) / 8, (WIN_H + 7) / 8, 1);
    ID3D11UnorderedAccessView* nulluav = nullptr;
    g_ctx->CSSetUnorderedAccessViews(0, 1, &nulluav, nullptr);

    // VS+PS: fullscreen quad
    float clear[4] = {};
    g_ctx->OMSetRenderTargets(1, &g_rtv, nullptr);
    g_ctx->ClearRenderTargetView(g_rtv, clear);
    D3D11_VIEWPORT vp = { 0,0,(float)WIN_W,(float)WIN_H,0,1 };
    g_ctx->RSSetViewports(1, &vp);
    g_ctx->VSSetShader(g_vs, nullptr, 0);
    g_ctx->PSSetShader(g_ps, nullptr, 0);
    g_ctx->PSSetShaderResources(0, 1, &g_srv);
    g_ctx->PSSetSamplers(0, 1, &g_smp);
    g_ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    g_ctx->Draw(4, 0);
    ID3D11ShaderResourceView* nullsrv = nullptr;
    g_ctx->PSSetShaderResources(0, 1, &nullsrv);

    // Present no-vsync
    UINT flags = g_tearing ? DXGI_PRESENT_ALLOW_TEARING : 0;
    g_sc->Present(0, flags);

    // FPS counter
    LARGE_INTEGER qp; QueryPerformanceCounter(&qp);
    g_fps_count++;
    int64_t elapsed = qp.QuadPart - g_fps_last;
    if (elapsed >= g_qpf.QuadPart) {
        g_fps_disp = g_fps_count;
        g_fps_count = 0;
        g_fps_last = qp.QuadPart;
        double uptime = (double)(qp.QuadPart - g_qp0.QuadPart) / g_qpf.QuadPart;
        swprintf_s(g_title, 128, L"VirtualCapture DDA Test — %d fps  t=%.1fs  (Esc=quit)", g_fps_disp, uptime);
        SetWindowTextW(g_hwnd, g_title);
    }
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp) {
    if (msg == WM_DESTROY) { PostQuitMessage(0); return 0; }
    if (msg == WM_KEYDOWN && wp == VK_ESCAPE) { DestroyWindow(hwnd); return 0; }
    return DefWindowProcW(hwnd, msg, wp, lp);
}

int WINAPI WinMain(HINSTANCE hInst, HINSTANCE, LPSTR, int) {
    QueryPerformanceFrequency(&g_qpf);
    QueryPerformanceCounter(&g_qp0);
    g_fps_last = g_qp0.QuadPart;

    WNDCLASSEXW wc = { sizeof(wc) };
    wc.lpfnWndProc = WndProc; wc.hInstance = hInst;
    wc.lpszClassName = WCLASS; wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
    RegisterClassExW(&wc);

    RECT r = { 0, 0, WIN_W, WIN_H };
    AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
    HWND hwnd = CreateWindowExW(0, WCLASS, L"VirtualCapture DDA Test (Esc=quit)",
        WS_OVERLAPPEDWINDOW, 100, 100,
        r.right - r.left, r.bottom - r.top,
        nullptr, nullptr, hInst, nullptr);
    if (!hwnd) return 1;
    g_hwnd = hwnd;

    if (!Init(hwnd)) {
        MessageBoxW(hwnd, L"D3D11 init failed", L"Error", MB_OK);
        return 1;
    }

    ShowWindow(hwnd, SW_SHOW);
    UpdateWindow(hwnd);

    MSG msg = {};
    while (true) {
        while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&msg); DispatchMessageW(&msg);
            if (msg.message == WM_QUIT) goto done;
        }
        Render();
    }
done:
    return 0;
}
