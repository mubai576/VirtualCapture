// monitor/monitor.cpp — vc-monitor.exe
// D3D11 NV12 preview window.  Reads SHM Local\VirtualCaptureVcamFrame.
// OSD: title bar shows key stats every 500ms.
// Shader: NV12->RGB + status-color corner indicator.
// No extra dependencies beyond Windows SDK + D3D11.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <d3dcompiler.h>
#include <stdint.h>
#include <stdio.h>
#include <math.h>
#include <string>
#include <atomic>
#include <thread>
#include <mutex>
#pragma comment(lib,"d3d11.lib")
#pragma comment(lib,"dxgi.lib")
#pragma comment(lib,"d3dcompiler.lib")

// ── constants ─────────────────────────────────────────────────────────────────
static const int    VCAM_W      = 640;
static const int    VCAM_H      = 640;
static const int    WIN_W       = 700;
static const int    WIN_H       = 720;  // extra 20px for status bar
static const UINT   FRAME_BYTES = 614400;
static const UINT   HDR_SIZE    = 24;
static const UINT32 SHM_MAGIC   = 0x5643414Du;
static const char*  SHM_NAME    = "Local\\VirtualCaptureVcamFrame";

static wchar_t g_run_dir[MAX_PATH];

// ── SHM header ────────────────────────────────────────────────────────────────
#pragma pack(push,1)
struct ShmHdr { uint32_t magic; int32_t seq; uint32_t frame_id; uint64_t hash; uint32_t size; };
#pragma pack(pop)

// ── shaders ───────────────────────────────────────────────────────────────────
static const char* kVS = R"HLSL(
void VS(uint id:SV_VertexID, out float4 pos:SV_Position, out float2 uv:TEXCOORD) {
    uv  = float2((id&1) ? 1.0f : 0.0f, (id>>1) ? 1.0f : 0.0f);
    pos = float4(uv.x*2.0f-1.0f, 1.0f-uv.y*2.0f, 0.0f, 1.0f);
}
)HLSL";

// NV12 → RGB (BT.601 limited range) + status indicator corners
static const char* kPS = R"HLSL(
Texture2D<float>  TexY  : register(t0);
Texture2D<float2> TexUV : register(t1);
SamplerState      Smp   : register(s0);

cbuffer CB : register(b0) {
    float4 status_color;   // signal status color
    float  render_fps;
    float  pad0, pad1, pad2;
};

float4 PS(float4 pos:SV_Position, float2 uv:TEXCOORD) : SV_Target {
    // --- status indicator: top-left 20x20 px corner ---
    float2 pxy = float2(pos.x / 700.0f, pos.y / 720.0f);
    if (pos.x < 22.0f && pos.y < 22.0f && pos.x > 2.0f && pos.y > 2.0f)
        return status_color;

    // --- black status bar at bottom (y > 700) ---
    if (pos.y >= 700.0f)
        return float4(0.08f, 0.08f, 0.08f, 1.0f);

    // --- NV12 -> RGB for main area (map uv to 640x640 centered in 700x700) ---
    float2 video_uv;
    video_uv.x = (pos.x - 30.0f) / 640.0f;   // 30px left margin
    video_uv.y =  pos.y           / 700.0f;

    // out-of-range: dark border
    if (video_uv.x < 0.0f || video_uv.x > 1.0f)
        return float4(0.05f, 0.05f, 0.05f, 1.0f);

    float Y  = TexY .Sample(Smp, video_uv).r;
    float2 UV= TexUV.Sample(Smp, video_uv).rg;
    float Cb = UV.r - 0.5f;
    float Cr = UV.g - 0.5f;
    float y  = (Y  - 16.0f/255.0f) * (255.0f/219.0f);
    float r  = saturate(y + 1.402f * Cr);
    float g  = saturate(y - 0.344136f*Cb - 0.714136f*Cr);
    float b  = saturate(y + 1.772f * Cb);
    return float4(r, g, b, 1.0f);
}
)HLSL";

// ── D3D globals ───────────────────────────────────────────────────────────────
static ID3D11Device*           g_dev   = nullptr;
static ID3D11DeviceContext*    g_ctx   = nullptr;
static IDXGISwapChain*         g_sc    = nullptr;
static ID3D11RenderTargetView* g_rtv   = nullptr;
static ID3D11VertexShader*     g_vs    = nullptr;
static ID3D11PixelShader*      g_ps    = nullptr;
static ID3D11SamplerState*     g_samp  = nullptr;
static ID3D11Buffer*           g_cb    = nullptr;
static ID3D11Texture2D*          g_texY  = nullptr;
static ID3D11Texture2D*          g_texUV = nullptr;
static ID3D11ShaderResourceView* g_srvY  = nullptr;
static ID3D11ShaderResourceView* g_srvUV = nullptr;

struct CBData { float status_color[4]; float render_fps; float pad[3]; };

// ── SHM ───────────────────────────────────────────────────────────────────────
static HANDLE   g_hMap  = nullptr;
static void*    g_pView = nullptr;
static uint8_t  g_frame_buf[FRAME_BYTES];

struct FrameInfo { uint32_t fid; uint64_t hash; bool valid; };
static FrameInfo ReadSHMFrame() {
    if (!g_pView) return {0, 0, false};
    ShmHdr h1; memcpy(&h1, g_pView, HDR_SIZE);
    if (h1.magic != SHM_MAGIC || (h1.seq & 1) || h1.size != FRAME_BYTES) return {h1.frame_id, h1.hash, false};
    memcpy(g_frame_buf, (uint8_t*)g_pView + HDR_SIZE, FRAME_BYTES);
    ShmHdr h2; memcpy(&h2, g_pView, HDR_SIZE);
    return {h1.frame_id, h1.hash, h1.seq == h2.seq};
}

// ── Stats ─────────────────────────────────────────────────────────────────────
struct Stats {
    float fps=0, rseg_p99=0, rseg_max=0, pc1_p99=0, gpu_ms=0;
    int   complete=0, dropped=0, mismatch=0, shm_pub=0;
    bool  rx_running=false;
};
static std::mutex  g_smtx;
static Stats       g_stats;
static std::atomic<float>    g_rfps{0.f};
static std::atomic<uint32_t> g_last_fid{0};
static std::atomic<bool>     g_has_signal{false};

static float jf(const char* b, const char* k) {
    const char* p=strstr(b,k); if (!p) return 0.f;
    p=strchr(p,':'); if (!p) return 0.f;
    while (*p==' '||*p==':') p++; return (float)atof(p);
}
static int ji(const char* b, const char* k) { return (int)jf(b,k); }
static bool jb(const char* b, const char* k) {
    const char* p=strstr(b,k); if (!p) return false;
    p=strchr(p,':'); if (!p) return false;
    while (*p==' '||*p==':') p++; return strncmp(p,"true",4)==0;
}

static void ReadLatestStats(const wchar_t* pattern, Stats& s, bool is_rx) {
    char buf[8192]="";
    WIN32_FIND_DATAW fd; HANDLE hf=FindFirstFileW(pattern,&fd);
    if (hf==INVALID_HANDLE_VALUE) return;
    WIN32_FIND_DATAW best=fd;
    while (FindNextFileW(hf,&fd))
        if (CompareFileTime(&fd.ftLastWriteTime,&best.ftLastWriteTime)>0) best=fd;
    FindClose(hf);
    wchar_t path[MAX_PATH]; swprintf_s(path,L"%s\\%s",g_run_dir,best.cFileName);
    HANDLE h=CreateFileW(path,GENERIC_READ,FILE_SHARE_READ|FILE_SHARE_WRITE,
                         nullptr,OPEN_EXISTING,0,nullptr);
    if (h==INVALID_HANDLE_VALUE) return;
    DWORD n=0; ReadFile(h,buf,sizeof(buf)-1,&n,nullptr); CloseHandle(h); buf[n]=0;
    if (is_rx) {
        s.rseg_p99   = jf(buf,"rseg_p99_ms");
        s.rseg_max   = jf(buf,"rseg_max_ms");
        s.complete   = ji(buf,"complete");
        s.dropped    = ji(buf,"dropped");
        s.mismatch   = ji(buf,"hash_mismatch");
        s.shm_pub    = ji(buf,"shm_published");
        s.rx_running = jb(buf,"running");
    } else {
        s.fps    = jf(buf,"fps_actual");
        s.pc1_p99= jf(buf,"pc1_seg_p99_ms");
        s.gpu_ms = jf(buf,"gpu_dispatch_ms");
    }
}

static void StatsThread() {
    wchar_t rx_pat[MAX_PATH], tx_pat[MAX_PATH];
    swprintf_s(rx_pat,L"%s\\*.receiver.stats.json",g_run_dir);
    swprintf_s(tx_pat,L"%s\\*.sender.stats.json",  g_run_dir);
    while (true) {
        Stats s;
        ReadLatestStats(rx_pat,s,true);
        ReadLatestStats(tx_pat,s,false);
        { std::lock_guard<std::mutex> lk(g_smtx); g_stats=s; }
        Sleep(500);
    }
}

// ── D3D ───────────────────────────────────────────────────────────────────────
static bool CompileShader(const char* src, const char* entry, const char* model, ID3DBlob** out) {
    ID3DBlob* err=nullptr;
    HRESULT hr=D3DCompile(src,strlen(src),nullptr,nullptr,nullptr,entry,model,0,0,out,&err);
    if (FAILED(hr) && err) {
        char msg[1024]; snprintf(msg,sizeof(msg),"Shader '%s' error:\n%s",entry,(char*)err->GetBufferPointer());
        MessageBoxA(nullptr,msg,"Shader Error",MB_OK|MB_ICONERROR);
        err->Release();
    }
    return SUCCEEDED(hr);
}

static bool InitD3D(HWND hwnd) {
    DXGI_SWAP_CHAIN_DESC sd={};
    sd.BufferCount=2; sd.BufferDesc.Width=WIN_W; sd.BufferDesc.Height=WIN_H;
    sd.BufferDesc.Format=DXGI_FORMAT_R8G8B8A8_UNORM;
    sd.BufferDesc.RefreshRate={0,1};
    sd.BufferUsage=DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.OutputWindow=hwnd; sd.SampleDesc.Count=1; sd.Windowed=TRUE;
    sd.SwapEffect=DXGI_SWAP_EFFECT_DISCARD;

    D3D_FEATURE_LEVEL fl;
    HRESULT hr=D3D11CreateDeviceAndSwapChain(nullptr,D3D_DRIVER_TYPE_HARDWARE,nullptr,0,
        nullptr,0,D3D11_SDK_VERSION,&sd,&g_sc,&g_dev,&fl,&g_ctx);
    if (FAILED(hr)) return false;

    ID3D11Texture2D* bb=nullptr;
    g_sc->GetBuffer(0,IID_PPV_ARGS(&bb));
    hr=g_dev->CreateRenderTargetView(bb,nullptr,&g_rtv); bb->Release();
    if (FAILED(hr)) return false;

    ID3DBlob *vsBlob=nullptr, *psBlob=nullptr;
    if (!CompileShader(kVS,"VS","vs_5_0",&vsBlob)) return false;
    if (!CompileShader(kPS,"PS","ps_5_0",&psBlob)) return false;
    g_dev->CreateVertexShader(vsBlob->GetBufferPointer(),vsBlob->GetBufferSize(),nullptr,&g_vs);
    g_dev->CreatePixelShader (psBlob->GetBufferPointer(),psBlob->GetBufferSize(),nullptr,&g_ps);
    vsBlob->Release(); psBlob->Release();

    D3D11_SAMPLER_DESC smpd={};
    smpd.Filter=D3D11_FILTER_MIN_MAG_MIP_LINEAR;
    smpd.AddressU=smpd.AddressV=smpd.AddressW=D3D11_TEXTURE_ADDRESS_CLAMP;
    g_dev->CreateSamplerState(&smpd,&g_samp);

    D3D11_BUFFER_DESC cbd={sizeof(CBData),D3D11_USAGE_DYNAMIC,D3D11_BIND_CONSTANT_BUFFER,
                           D3D11_CPU_ACCESS_WRITE,0,0};
    g_dev->CreateBuffer(&cbd,nullptr,&g_cb);

    // Y plane R8
    D3D11_TEXTURE2D_DESC td={};
    td.Width=VCAM_W; td.Height=VCAM_H; td.MipLevels=1; td.ArraySize=1;
    td.Format=DXGI_FORMAT_R8_UNORM; td.SampleDesc.Count=1;
    td.Usage=D3D11_USAGE_DYNAMIC; td.BindFlags=D3D11_BIND_SHADER_RESOURCE;
    td.CPUAccessFlags=D3D11_CPU_ACCESS_WRITE;
    g_dev->CreateTexture2D(&td,nullptr,&g_texY);
    D3D11_SHADER_RESOURCE_VIEW_DESC svd={DXGI_FORMAT_R8_UNORM,D3D11_SRV_DIMENSION_TEXTURE2D,{}};
    svd.Texture2D.MipLevels=1;
    g_dev->CreateShaderResourceView(g_texY,&svd,&g_srvY);

    // UV plane R8G8
    td.Width=VCAM_W/2; td.Height=VCAM_H/2; td.Format=DXGI_FORMAT_R8G8_UNORM;
    g_dev->CreateTexture2D(&td,nullptr,&g_texUV);
    svd.Format=DXGI_FORMAT_R8G8_UNORM;
    g_dev->CreateShaderResourceView(g_texUV,&svd,&g_srvUV);

    return true;
}

static void UploadFrame(const uint8_t* nv12) {
    D3D11_MAPPED_SUBRESOURCE ms;
    if (SUCCEEDED(g_ctx->Map(g_texY,0,D3D11_MAP_WRITE_DISCARD,0,&ms))) {
        const uint8_t* s=nv12; uint8_t* d=(uint8_t*)ms.pData;
        for (int y=0;y<VCAM_H;y++,d+=ms.RowPitch,s+=VCAM_W) memcpy(d,s,VCAM_W);
        g_ctx->Unmap(g_texY,0);
    }
    if (SUCCEEDED(g_ctx->Map(g_texUV,0,D3D11_MAP_WRITE_DISCARD,0,&ms))) {
        const uint8_t* s=nv12+VCAM_W*VCAM_H; uint8_t* d=(uint8_t*)ms.pData;
        for (int y=0;y<VCAM_H/2;y++,d+=ms.RowPitch,s+=VCAM_W) memcpy(d,s,VCAM_W);
        g_ctx->Unmap(g_texUV,0);
    }
}

// ── Render ────────────────────────────────────────────────────────────────────
static void Render(bool valid) {
    static LARGE_INTEGER freq={},prev={};
    static int cnt=0; static float acc=0.f;
    if (!freq.QuadPart) QueryPerformanceFrequency(&freq);
    LARGE_INTEGER now; QueryPerformanceCounter(&now);
    if (prev.QuadPart) {
        float dt=(float)(now.QuadPart-prev.QuadPart)/(float)freq.QuadPart;
        acc+=dt; cnt++;
        if (acc>=0.5f) { g_rfps.store(cnt/acc); acc=0; cnt=0; }
    }
    prev=now;

    float clear[4]={0,0,0,1};
    D3D11_VIEWPORT vp={0,0,(float)WIN_W,(float)WIN_H,0,1};
    g_ctx->RSSetViewports(1,&vp);
    g_ctx->OMSetRenderTargets(1,&g_rtv,nullptr);
    g_ctx->ClearRenderTargetView(g_rtv,clear);

    // update constant buffer: status color
    Stats s; { std::lock_guard<std::mutex> lk(g_smtx); s=g_stats; }
    bool live = s.rx_running && valid;
    bool err  = s.mismatch > 0 || s.dropped > 0;
    D3D11_MAPPED_SUBRESOURCE ms;
    if (SUCCEEDED(g_ctx->Map(g_cb,0,D3D11_MAP_WRITE_DISCARD,0,&ms))) {
        CBData* cb=(CBData*)ms.pData;
        if (!g_pView) { cb->status_color[0]=0.4f; cb->status_color[1]=0.4f; cb->status_color[2]=0.4f; cb->status_color[3]=1; }
        else if (err) { cb->status_color[0]=1.f;  cb->status_color[1]=0.2f; cb->status_color[2]=0.f;  cb->status_color[3]=1; }
        else if (live) { cb->status_color[0]=0.1f; cb->status_color[1]=1.f;  cb->status_color[2]=0.1f; cb->status_color[3]=1; }
        else           { cb->status_color[0]=1.f;  cb->status_color[1]=0.8f; cb->status_color[2]=0.f;  cb->status_color[3]=1; }
        cb->render_fps=g_rfps.load();
        g_ctx->Unmap(g_cb,0);
    }

    g_ctx->VSSetShader(g_vs,nullptr,0);
    g_ctx->PSSetShader(g_ps,nullptr,0);
    g_ctx->PSSetSamplers(0,1,&g_samp);
    ID3D11ShaderResourceView* srvs[2]={g_srvY,g_srvUV};
    g_ctx->PSSetShaderResources(0,2,srvs);
    g_ctx->PSSetConstantBuffers(0,1,&g_cb);
    g_ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    g_ctx->Draw(4,0);
    g_sc->Present(0,0);
}

// ── Window ────────────────────────────────────────────────────────────────────
static HWND g_hwnd = nullptr;
static LRESULT CALLBACK WndProc(HWND hw, UINT msg, WPARAM wp, LPARAM lp) {
    if (msg==WM_DESTROY) { PostQuitMessage(0); return 0; }
    if (msg==WM_KEYDOWN && wp==VK_ESCAPE) { PostQuitMessage(0); return 0; }
    if (msg==WM_TIMER) {
        // Update title bar with stats
        Stats s; { std::lock_guard<std::mutex> lk(g_smtx); s=g_stats; }
        wchar_t title[256];
        swprintf_s(title,
            L"vc-monitor  |  fps:%.0f  rseg_p99:%.1fms  rseg_max:%.0fms  "
            L"pc1_p99:%.1fms  gpu:%.2fms  "
            L"ok:%d drop:%d mismatch:%d  rfps:%.0f  [ESC=quit]",
            s.fps, s.rseg_p99, s.rseg_max,
            s.pc1_p99, s.gpu_ms,
            s.complete, s.dropped, s.mismatch,
            g_rfps.load());
        SetWindowTextW(hw, title);
        return 0;
    }
    return DefWindowProcW(hw,msg,wp,lp);
}

// ── main ──────────────────────────────────────────────────────────────────────
int WINAPI WinMain(HINSTANCE hi, HINSTANCE, LPSTR, int) {
    // compute run dir: go up from exe dir until we find a "run" subfolder
    wchar_t exe[MAX_PATH]; GetModuleFileNameW(nullptr,exe,MAX_PATH);
    // strip filename
    wchar_t* last=wcsrchr(exe,L'\\'); if (last) *last=0;
    // try exe/../run, exe/../../run
    bool found=false;
    for (int up=0; up<4 && !found; up++) {
        swprintf_s(g_run_dir,L"%s\\run",exe);
        if (GetFileAttributesW(g_run_dir)!=INVALID_FILE_ATTRIBUTES) { found=true; break; }
        wchar_t* p=wcsrchr(exe,L'\\'); if (p) *p=0; else break;
    }
    if (!found) swprintf_s(g_run_dir,L"%s\\run",exe);  // fallback

    // Open SHM
    g_hMap = OpenFileMappingA(FILE_MAP_READ, FALSE, SHM_NAME);
    if (g_hMap) g_pView = MapViewOfFile(g_hMap, FILE_MAP_READ, 0, 0, HDR_SIZE+FRAME_BYTES);

    // Register & create window
    WNDCLASSEXW wc={sizeof(wc)};
    wc.style=CS_HREDRAW|CS_VREDRAW;
    wc.lpfnWndProc=WndProc;
    wc.hInstance=hi;
    wc.hCursor=LoadCursorW(nullptr,(LPCWSTR)IDC_ARROW);
    wc.lpszClassName=L"VCMonitor";
    if (!RegisterClassExW(&wc)) {
        wchar_t msg[128]; swprintf_s(msg,L"RegisterClassExW failed: %lu",GetLastError());
        MessageBoxW(nullptr,msg,L"Error",MB_OK|MB_ICONERROR); return 1;
    }

    // Fix window size to account for border/title bar
    RECT r={0,0,WIN_W,WIN_H};
    AdjustWindowRect(&r,WS_OVERLAPPEDWINDOW,FALSE);
    g_hwnd = CreateWindowExW(0,L"VCMonitor",L"vc-monitor  [starting...]",
        WS_OVERLAPPEDWINDOW,CW_USEDEFAULT,CW_USEDEFAULT,
        r.right-r.left, r.bottom-r.top,
        nullptr,nullptr,hi,nullptr);
    if (!g_hwnd) {
        wchar_t msg[128]; swprintf_s(msg,L"CreateWindowExW failed: %lu",GetLastError());
        MessageBoxW(nullptr,msg,L"Error",MB_OK|MB_ICONERROR); return 1;
    }

    if (!InitD3D(g_hwnd)) {
        MessageBoxW(g_hwnd,L"D3D11 init failed.\nCheck that DirectX 11 is available.",
                    L"Error",MB_OK|MB_ICONERROR);
        return 1;
    }

    // Load initial grey frame (no-signal)
    { static uint8_t grey[FRAME_BYTES]; memset(grey,128,FRAME_BYTES); UploadFrame(grey); }

    ShowWindow(g_hwnd,SW_SHOW); UpdateWindow(g_hwnd);
    SetTimer(g_hwnd,1,500,nullptr);  // stats update timer

    // Stats background thread
    std::thread st(StatsThread); st.detach();

    MSG msg={};
    while (true) {
        while (PeekMessageW(&msg,nullptr,0,0,PM_REMOVE)) {
            TranslateMessage(&msg); DispatchMessageW(&msg);
            if (msg.message==WM_QUIT) goto done;
        }
        auto fi=ReadSHMFrame();
        if (fi.valid && fi.fid != g_last_fid.load()) {
            g_last_fid.store(fi.fid);
            g_has_signal.store(true);
            UploadFrame(g_frame_buf);
        }
        Render(fi.valid && g_has_signal.load());
        Sleep(1);
    }
done:
    if (g_pView) UnmapViewOfFile(g_pView);
    if (g_hMap)  CloseHandle(g_hMap);
    return 0;
}
