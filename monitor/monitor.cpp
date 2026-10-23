// monitor/monitor.cpp — vc-monitor.exe
// D3D11 NV12 preview + GDI OSD overlay on video surface.
// OSD: right-click to toggle Full / Mini / Off.  ESC=quit.  O=cycle OSD.
// Text via TextOutW (Unicode).  Semi-transparent bg via AlphaBlend.
// No right panel — full 640x700 video window.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <windowsx.h>
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
#include <deque>
#pragma comment(lib,"d3d11.lib")
#pragma comment(lib,"dxgi.lib")
#pragma comment(lib,"d3dcompiler.lib")
#pragma comment(lib,"gdi32.lib")
#pragma comment(lib,"msimg32.lib")   // AlphaBlend

// ── constants ─────────────────────────────────────────────────────────────────
static const int    VCAM_W      = 640;
static const int    VCAM_H      = 640;
static const int    WIN_W       = 640;
static const int    WIN_H       = 640;   // pure video, no status bar
static const UINT   FRAME_BYTES = 614400;
static const UINT   HDR_SIZE    = 24;
static const UINT32 SHM_MAGIC   = 0x5643414Du;
static const char*  SHM_NAME    = "Local\\VirtualCaptureVcamFrame";
static const int    HIST_N      = 120;

// Context-menu IDs
enum { IDM_OSD_FULL=1001, IDM_OSD_MINI, IDM_OSD_OFF, IDM_QUIT };

static wchar_t g_run_dir[MAX_PATH];

// ── SHM header ────────────────────────────────────────────────────────────────
#pragma pack(push,1)
struct ShmHdr { uint32_t magic; int32_t seq; uint32_t frame_id; uint64_t hash; uint32_t size; };
#pragma pack(pop)

// ── HLSL ──────────────────────────────────────────────────────────────────────
static const char* kVS = R"HLSL(
void VS(uint id:SV_VertexID, out float4 pos:SV_Position, out float2 uv:TEXCOORD) {
    uv  = float2((id&1)?1.0f:0.0f,(id>>1)?1.0f:0.0f);
    pos = float4(uv.x*2.0f-1.0f, 1.0f-uv.y*2.0f, 0.0f, 1.0f);
}
)HLSL";

// NV12 → RGB.  Video fills entire window.
static const char* kPS = R"HLSL(
Texture2D<float>  TexY  : register(t0);
Texture2D<float2> TexUV : register(t1);
SamplerState      Smp   : register(s0);
cbuffer CB : register(b0) {
    float4 sig_color;
    float  vid_h, win_h, pad0, pad1;
};
float4 PS(float4 pos:SV_Position, float2 uv:TEXCOORD):SV_Target {
    // Status dot top-left 18x18
    if (pos.x>=4&&pos.x<22&&pos.y>=4&&pos.y<22) return sig_color;
    // NV12 → RGB (BT.601 limited range)
    float2 vuv = float2(pos.x/640.0f, pos.y/640.0f);
    float Y   = TexY .Sample(Smp, vuv).r;
    float2 UV = TexUV.Sample(Smp, vuv).rg;
    float Cb=UV.r-0.5f, Cr=UV.g-0.5f;
    float y=(Y-16.0f/255.0f)*(255.0f/219.0f);
    return float4(saturate(y+1.402f*Cr),
                  saturate(y-0.344136f*Cb-0.714136f*Cr),
                  saturate(y+1.772f*Cb), 1.0f);
}
)HLSL";

// ── D3D globals ───────────────────────────────────────────────────────────────
static ID3D11Device*             g_dev  = nullptr;
static ID3D11DeviceContext*      g_ctx  = nullptr;
static IDXGISwapChain*           g_sc   = nullptr;
static ID3D11RenderTargetView*   g_rtv  = nullptr;
static ID3D11VertexShader*       g_vs   = nullptr;
static ID3D11PixelShader*        g_ps   = nullptr;
static ID3D11SamplerState*       g_samp = nullptr;
static ID3D11Buffer*             g_cb   = nullptr;
static ID3D11Texture2D*          g_texY = nullptr, *g_texUV = nullptr;
static ID3D11ShaderResourceView* g_srvY = nullptr, *g_srvUV = nullptr;

struct CBData { float sig_color[4]; float vid_h, win_h, pad0, pad1; };

// ── SHM ───────────────────────────────────────────────────────────────────────
static HANDLE  g_hMap  = nullptr;
static void*   g_pView = nullptr;
static uint8_t g_frame_buf[FRAME_BYTES];

struct FrameInfo { uint32_t fid; uint64_t hash; bool valid; };
static FrameInfo ReadSHMFrame() {
    if (!g_pView) return {0,0,false};
    ShmHdr h1; memcpy(&h1,g_pView,HDR_SIZE);
    if (h1.magic!=SHM_MAGIC||(h1.seq&1)||h1.size!=FRAME_BYTES) return {h1.frame_id,h1.hash,false};
    memcpy(g_frame_buf,(uint8_t*)g_pView+HDR_SIZE,FRAME_BYTES);
    ShmHdr h2; memcpy(&h2,g_pView,HDR_SIZE);
    return {h1.frame_id,h1.hash,h1.seq==h2.seq};
}

// ── Stats ─────────────────────────────────────────────────────────────────────
struct Stats {
    float fps=0, rfps=0;
    float rseg_avg=0, rseg_p50=0, rseg_p99=0, rseg_max=0;
    float pc1_avg=0,  pc1_p99=0,  pc1_max=0;
    float gpu_ms=0, gpu_dispatch=0;
    float bps=0, pps=0, ia_avg=0, ia_p99=0;
    int   complete=0, dropped=0, mismatch=0, shm_pub=0;
    int   gaps=0, pkt_lost=0, send_err=0;
    bool  rx_run=false, tx_run=false;
    float uptime_rx=0, uptime_tx=0;
    int   cpu_pct=0;
    bool  gpu_convert=false;
    char  rx_sid[32]={}, tx_sid[32]={};
};
static std::mutex  g_smtx;
static Stats       g_stats;
static std::deque<float> g_hist_fps, g_hist_rseg, g_hist_pc1, g_hist_full;
static std::atomic<float>    g_rfps{0.f};
static std::atomic<uint32_t> g_last_fid{0};
static std::atomic<bool>     g_has_signal{false};
static std::atomic<int>      g_osd_mode{2};   // 0=off 1=mini 2=full

// tiny JSON helpers
static float jf(const char* b,const char* k){
    const char* p=strstr(b,k); if(!p) return 0.f;
    p=strchr(p,':'); if(!p) return 0.f;
    while(*p==' '||*p==':') p++; return (float)atof(p);
}
static int  ji(const char* b,const char* k){ return (int)jf(b,k); }
static bool jb(const char* b,const char* k){
    const char* p=strstr(b,k); if(!p) return false;
    p=strchr(p,':'); if(!p) return false;
    while(*p==' '||*p==':') p++; return strncmp(p,"true",4)==0;
}
static void jstr(const char* b,const char* k,char* out,int n){
    const char* p=strstr(b,k); if(!p){out[0]=0;return;}
    p=strchr(p,':'); if(!p){out[0]=0;return;}
    while(*p==' '||*p==':'||*p=='"') p++;
    int i=0; while(i<n-1&&*p&&*p!='"'&&*p!=',') out[i++]=*p++;
    out[i]=0;
}

static void ReadLatestStats(const wchar_t* pat, Stats& s, bool is_rx){
    char buf[8192]="";
    WIN32_FIND_DATAW fd; HANDLE hf=FindFirstFileW(pat,&fd);
    if(hf==INVALID_HANDLE_VALUE) return;
    WIN32_FIND_DATAW best=fd;
    while(FindNextFileW(hf,&fd))
        if(CompareFileTime(&fd.ftLastWriteTime,&best.ftLastWriteTime)>0) best=fd;
    FindClose(hf);
    wchar_t path[MAX_PATH]; swprintf_s(path,L"%s\\%s",g_run_dir,best.cFileName);
    HANDLE h=CreateFileW(path,GENERIC_READ,FILE_SHARE_READ|FILE_SHARE_WRITE,nullptr,OPEN_EXISTING,0,nullptr);
    if(h==INVALID_HANDLE_VALUE) return;
    DWORD n=0; ReadFile(h,buf,sizeof(buf)-1,&n,nullptr); CloseHandle(h); buf[n]=0;
    if(is_rx){
        s.rseg_avg=jf(buf,"rseg_avg_ms"); s.rseg_p50=jf(buf,"rseg_p50_ms");
        s.rseg_p99=jf(buf,"rseg_p99_ms"); s.rseg_max=jf(buf,"rseg_max_ms");
        s.ia_avg   =jf(buf,"inter_arrival_avg_ms");
        s.ia_p99   =jf(buf,"inter_arrival_p99_ms");
        s.bps=jf(buf,"bps"); s.pps=jf(buf,"pps");
        s.complete=ji(buf,"complete"); s.dropped=ji(buf,"dropped");
        s.mismatch=ji(buf,"hash_mismatch"); s.shm_pub=ji(buf,"shm_published");
        s.gaps=ji(buf,"gaps"); s.pkt_lost=ji(buf,"packets_lost");
        s.rx_run=jb(buf,"running"); s.uptime_rx=jf(buf,"uptime_s");
        jstr(buf,"session_id",s.rx_sid,sizeof(s.rx_sid));
    } else {
        s.fps=jf(buf,"fps_actual");
        s.pc1_avg=jf(buf,"pc1_seg_avg_ms"); s.pc1_p99=jf(buf,"pc1_seg_p99_ms");
        s.pc1_max=jf(buf,"pc1_seg_max_ms");
        s.gpu_ms=jf(buf,"gpu_ms"); s.gpu_dispatch=jf(buf,"gpu_dispatch_ms");
        s.cpu_pct=ji(buf,"cpu_pct"); s.send_err=ji(buf,"send_errors");
        s.tx_run=jb(buf,"running"); s.uptime_tx=jf(buf,"uptime_s");
        s.gpu_convert=jb(buf,"gpu_convert");
        jstr(buf,"session_id",s.tx_sid,sizeof(s.tx_sid));
    }
}
static void StatsThread(){
    wchar_t rp[MAX_PATH],tp[MAX_PATH];
    swprintf_s(rp,L"%s\\*.receiver.stats.json",g_run_dir);
    swprintf_s(tp,L"%s\\*.sender.stats.json",  g_run_dir);
    while(true){
        Stats s; ReadLatestStats(rp,s,true); ReadLatestStats(tp,s,false);
        s.rfps=g_rfps.load();
        { std::lock_guard<std::mutex> lk(g_smtx);
          g_stats=s;
          auto push=[](std::deque<float>& d,float v){ d.push_back(v); if((int)d.size()>HIST_N) d.pop_front(); };
          if(s.fps>0)      push(g_hist_fps,  s.fps);
          if(s.rseg_p99>0) push(g_hist_rseg, s.rseg_p99);
          if(s.pc1_p99>0)  push(g_hist_pc1,  s.pc1_p99);
          if(s.fps>0&&s.rseg_p99>0) push(g_hist_full, s.pc1_p99+s.rseg_p99);
        }
        Sleep(400);
    }
}

// ── D3D init ──────────────────────────────────────────────────────────────────
static bool CompileShader(const char* src,const char* ep,const char* model,ID3DBlob** out){
    ID3DBlob* err=nullptr;
    HRESULT hr=D3DCompile(src,strlen(src),nullptr,nullptr,nullptr,ep,model,0,0,out,&err);
    if(FAILED(hr)&&err){ char m[512]; snprintf(m,512,"Shader '%s':\n%s",ep,(char*)err->GetBufferPointer()); MessageBoxA(nullptr,m,"Shader",MB_OK); err->Release(); }
    return SUCCEEDED(hr);
}
static bool InitD3D(HWND hwnd){
    DXGI_SWAP_CHAIN_DESC sd={};
    sd.BufferCount=2; sd.BufferDesc.Width=WIN_W; sd.BufferDesc.Height=WIN_H;
    sd.BufferDesc.Format=DXGI_FORMAT_R8G8B8A8_UNORM; sd.BufferDesc.RefreshRate={0,1};
    sd.BufferUsage=DXGI_USAGE_RENDER_TARGET_OUTPUT; sd.OutputWindow=hwnd;
    sd.SampleDesc.Count=1; sd.Windowed=TRUE; sd.SwapEffect=DXGI_SWAP_EFFECT_DISCARD;
    D3D_FEATURE_LEVEL fl;
    if(FAILED(D3D11CreateDeviceAndSwapChain(nullptr,D3D_DRIVER_TYPE_HARDWARE,nullptr,0,
        nullptr,0,D3D11_SDK_VERSION,&sd,&g_sc,&g_dev,&fl,&g_ctx))) return false;
    ID3D11Texture2D* bb=nullptr; g_sc->GetBuffer(0,IID_PPV_ARGS(&bb));
    if(FAILED(g_dev->CreateRenderTargetView(bb,nullptr,&g_rtv))){ bb->Release(); return false; }
    bb->Release();
    ID3DBlob *vb=nullptr,*pb=nullptr;
    if(!CompileShader(kVS,"VS","vs_5_0",&vb)) return false;
    if(!CompileShader(kPS,"PS","ps_5_0",&pb)) return false;
    g_dev->CreateVertexShader(vb->GetBufferPointer(),vb->GetBufferSize(),nullptr,&g_vs);
    g_dev->CreatePixelShader (pb->GetBufferPointer(),pb->GetBufferSize(),nullptr,&g_ps);
    vb->Release(); pb->Release();
    D3D11_SAMPLER_DESC smpd={}; smpd.Filter=D3D11_FILTER_MIN_MAG_MIP_LINEAR;
    smpd.AddressU=smpd.AddressV=smpd.AddressW=D3D11_TEXTURE_ADDRESS_CLAMP;
    g_dev->CreateSamplerState(&smpd,&g_samp);
    D3D11_BUFFER_DESC cbd={sizeof(CBData),D3D11_USAGE_DYNAMIC,D3D11_BIND_CONSTANT_BUFFER,D3D11_CPU_ACCESS_WRITE,0,0};
    g_dev->CreateBuffer(&cbd,nullptr,&g_cb);
    D3D11_TEXTURE2D_DESC td={}; td.MipLevels=1; td.ArraySize=1; td.SampleDesc.Count=1;
    td.Usage=D3D11_USAGE_DYNAMIC; td.BindFlags=D3D11_BIND_SHADER_RESOURCE; td.CPUAccessFlags=D3D11_CPU_ACCESS_WRITE;
    td.Width=VCAM_W; td.Height=VCAM_H; td.Format=DXGI_FORMAT_R8_UNORM;
    g_dev->CreateTexture2D(&td,nullptr,&g_texY);
    D3D11_SHADER_RESOURCE_VIEW_DESC sv={DXGI_FORMAT_R8_UNORM,D3D11_SRV_DIMENSION_TEXTURE2D,{}}; sv.Texture2D.MipLevels=1;
    g_dev->CreateShaderResourceView(g_texY,&sv,&g_srvY);
    td.Width=VCAM_W/2; td.Height=VCAM_H/2; td.Format=DXGI_FORMAT_R8G8_UNORM;
    g_dev->CreateTexture2D(&td,nullptr,&g_texUV); sv.Format=DXGI_FORMAT_R8G8_UNORM;
    g_dev->CreateShaderResourceView(g_texUV,&sv,&g_srvUV);
    return true;
}
static void UploadFrame(const uint8_t* nv12){
    D3D11_MAPPED_SUBRESOURCE ms;
    if(SUCCEEDED(g_ctx->Map(g_texY,0,D3D11_MAP_WRITE_DISCARD,0,&ms))){
        const uint8_t* s=nv12; uint8_t* d=(uint8_t*)ms.pData;
        for(int y=0;y<VCAM_H;y++,d+=ms.RowPitch,s+=VCAM_W) memcpy(d,s,VCAM_W);
        g_ctx->Unmap(g_texY,0);
    }
    if(SUCCEEDED(g_ctx->Map(g_texUV,0,D3D11_MAP_WRITE_DISCARD,0,&ms))){
        const uint8_t* s=nv12+VCAM_W*VCAM_H; uint8_t* d=(uint8_t*)ms.pData;
        for(int y=0;y<VCAM_H/2;y++,d+=ms.RowPitch,s+=VCAM_W) memcpy(d,s,VCAM_W);
        g_ctx->Unmap(g_texUV,0);
    }
}

// ── GDI OSD ───────────────────────────────────────────────────────────────────
static HFONT g_fnt_sm=nullptr, g_fnt_md=nullptr, g_fnt_bd=nullptr;
static void InitFonts(){
    g_fnt_sm = CreateFontW(-12,0,0,0,FW_NORMAL,0,0,0,DEFAULT_CHARSET,0,0,CLEARTYPE_QUALITY,FIXED_PITCH,L"Consolas");
    g_fnt_md = CreateFontW(-14,0,0,0,FW_BOLD,  0,0,0,DEFAULT_CHARSET,0,0,CLEARTYPE_QUALITY,FIXED_PITCH,L"Consolas");
    g_fnt_bd = CreateFontW(-18,0,0,0,FW_BOLD,  0,0,0,DEFAULT_CHARSET,0,0,CLEARTYPE_QUALITY,FIXED_PITCH,L"Consolas");
}

// Draw semi-transparent filled rectangle (alpha 0-255)
static void AlphaRect(HDC dst, int x, int y, int w, int h, COLORREF col, BYTE alpha){
    HDC mem=CreateCompatibleDC(dst);
    HBITMAP bmp=CreateCompatibleBitmap(dst,w,h);
    HBITMAP old=(HBITMAP)SelectObject(mem,bmp);
    HBRUSH br=CreateSolidBrush(col);
    RECT r={0,0,w,h}; FillRect(mem,&r,br);
    DeleteObject(br);
    BLENDFUNCTION bf={AC_SRC_OVER,0,alpha,0};
    AlphaBlend(dst,x,y,w,h,mem,0,0,w,h,bf);
    SelectObject(mem,old); DeleteObject(bmp); DeleteDC(mem);
}

// Draw sparkline: data in dq, rect [x,y,w,h], scale 0..max_val
// warn_thresh draws a dashed line + turns points red above it
static void DrawSparkline(HDC hdc, int x, int y, int w, int h,
                           const std::deque<float>& dq, float max_val,
                           COLORREF lc, float warn){
    if(max_val<=0) max_val=1.f;
    // warn line
    if(warn>0&&warn<max_val){
        int wy=y+h-(int)(warn/max_val*h);
        HPEN wp=CreatePen(PS_DOT,1,RGB(150,140,60));
        HPEN op=(HPEN)SelectObject(hdc,wp);
        MoveToEx(hdc,x,wy,nullptr); LineTo(hdc,x+w,wy);
        SelectObject(hdc,op); DeleteObject(wp);
    }
    int n=(int)dq.size(); if(n<2) return;
    int start=(n>w)?n-w:0;
    HPEN lpen=CreatePen(PS_SOLID,2,lc);
    HPEN op=(HPEN)SelectObject(hdc,lpen);
    bool first=true;
    for(int i=start;i<n;i++){
        float v=dq[i]; if(v<0)v=0; if(v>max_val)v=max_val;
        int px=x+(i-start), py=y+h-(int)(v/max_val*h);
        if(py<y)py=y; if(py>=y+h)py=y+h-1;
        COLORREF c=(warn>0&&dq[i]>warn)?RGB(255,80,60):lc;
        // recreate pen when color changes
        SelectObject(hdc,op); DeleteObject(lpen);
        lpen=CreatePen(PS_SOLID,2,c); op=(HPEN)SelectObject(hdc,lpen);
        if(first){ MoveToEx(hdc,px,py,nullptr); first=false; }
        else LineTo(hdc,px,py);
    }
    SelectObject(hdc,op); DeleteObject(lpen);
}

// wprintf to a line: helper
static void OsdLine(HDC hdc, int x, int& y, int dy, COLORREF col,
                    HFONT fnt, const wchar_t* fmt, ...){
    wchar_t buf[256]; va_list va; va_start(va,fmt); vswprintf_s(buf,fmt,va); va_end(va);
    SelectObject(hdc,fnt);
    SetTextColor(hdc,col);
    TextOutW(hdc,x,y,buf,(int)wcslen(buf));
    y+=dy;
}

static COLORREF SpeedColor(float v, float good, float warn){
    if(v<=0)   return RGB(120,120,120);
    if(v<good) return RGB(100,230,100);
    if(v<warn) return RGB(255,200,60);
    return RGB(255,80,60);
}

static void DrawOSD(HWND hwnd, const Stats& s, int mode){
    if(mode==0) return;
    HDC hdc=GetDC(hwnd);
    if(!hdc) return;
    SetBkMode(hdc,TRANSPARENT);

    bool live=s.rx_run&&g_has_signal.load();
    bool has_err=(s.mismatch>0||s.dropped>0);
    float full_p99=s.pc1_p99+s.rseg_p99;

    // ── Mini OSD: compact 3-line overlay (top-left) ──────────────────────────
    if(mode==1){
        AlphaRect(hdc, 2, 24, 380, 56, RGB(0,0,0), 204);
        int cy=27;
        COLORREF fps_c=(s.fps>=230)?RGB(100,230,100):(s.fps>=180)?RGB(255,200,60):RGB(255,80,60);
        fps_c=(s.fps>=230)?RGB(100,230,100):(s.fps>=180)?RGB(255,200,60):RGB(255,80,60);
        OsdLine(hdc,6,cy,18,fps_c,g_fnt_md,
            L" FPS %.0f  rfps %.0f  p99 %.1fms (pc1:%.1f rseg:%.1f)",
            s.fps, s.rfps, full_p99, s.pc1_p99, s.rseg_p99);
        COLORREF sq_c=live?RGB(80,230,80):(has_err?RGB(255,80,60):RGB(255,200,60));
        OsdLine(hdc,6,cy,18,sq_c,g_fnt_md,
            L" %s  ok:%-6d drop:%-3d miss:%-3d shm:%-6d",
            live?L"\u25cf LIVE":L"\u25cb IDLE",
            s.complete, s.dropped, s.mismatch, s.shm_pub);
        ReleaseDC(hwnd,hdc); return;
    }

    // ── Full OSD ──────────────────────────────────────────────────────────────
    // Left column: receiver stats
    // Right column: sender stats
    // Bottom: graphs across full width
    // Each column ~300px wide

    const int COL1=6, COL2=326, COL_W=306;
    const int ROW_H=16, HDR_H=19;
    int L=COL1, R=COL2;

    // Background panels
    AlphaRect(hdc, 2, 24, 636, 194, RGB(0,0,0), 204);  // stats area
    AlphaRect(hdc, 2, 226, 636, 172, RGB(0,0,0), 204); // graphs area

    // ── Left: Receiver ────────────────────────────────────────────────────────
    int cy=27;
    COLORREF hdr_c=RGB(120,180,255);
    OsdLine(hdc,L,cy,HDR_H,hdr_c,g_fnt_md, L" \u2501\u2501 Receiver \u2501\u2501");
    // running status
    COLORREF st_c=live?RGB(80,230,80):(has_err?RGB(255,80,60):RGB(255,200,60));
    OsdLine(hdc,L,cy,ROW_H,st_c,g_fnt_sm,
        L" %s  uptime %.0fs  sid: %.10hs",
        live?L"\u25cf LIVE":L"\u25cb IDLE", s.uptime_rx, s.rx_sid);

    // rseg timing
    OsdLine(hdc,L,cy,ROW_H,SpeedColor(s.rseg_avg,5,8),g_fnt_sm,
        L" rseg avg/p50/p99/max: %.1f / %.1f / %.1f / %.1f ms",
        s.rseg_avg, s.rseg_p50, s.rseg_p99, s.rseg_max);
    // inter-arrival
    OsdLine(hdc,L,cy,ROW_H,SpeedColor(s.ia_p99,6,10),g_fnt_sm,
        L" inter-arr avg/p99: %.2f / %.2f ms",
        s.ia_avg, s.ia_p99);
    // frame counts
    COLORREF ok_c=(s.dropped==0&&s.mismatch==0)?RGB(100,230,100):RGB(255,80,60);
    OsdLine(hdc,L,cy,ROW_H,ok_c,g_fnt_sm,
        L" ok %-7d  drop %-4d  mismatch %-4d  gaps %-3d",
        s.complete, s.dropped, s.mismatch, s.gaps);
    // shm + pkt
    OsdLine(hdc,L,cy,ROW_H,RGB(180,180,220),g_fnt_sm,
        L" shm_pub %-7d  pkt_lost %-4d",
        s.shm_pub, s.pkt_lost);
    // bandwidth
    COLORREF bw_c=(s.bps/1e9f>1.0f)?RGB(255,200,60):RGB(160,200,160);
    OsdLine(hdc,L,cy,ROW_H,bw_c,g_fnt_sm,
        L" bw %.3f Gbps  pps %.0f",
        s.bps/1e9f, s.pps);

    // ── Right: Sender ─────────────────────────────────────────────────────────
    cy=27;
    OsdLine(hdc,R,cy,HDR_H,hdr_c,g_fnt_md, L" \u2501\u2501 Sender \u2501\u2501");
    // fps
    COLORREF fps_c=(s.fps>=230)?RGB(100,230,100):(s.fps>=180)?RGB(255,200,60):RGB(255,80,60);
    OsdLine(hdc,R,cy,ROW_H,fps_c,g_fnt_sm,
        L" fps %.1f / %.0f  cpu %d%%  %s",
        s.fps, 240.f, s.cpu_pct, s.gpu_convert?L"GPU":L"CPU");
    // pc1_seg
    OsdLine(hdc,R,cy,ROW_H,SpeedColor(s.pc1_p99,4,6),g_fnt_sm,
        L" pc1 avg/p99/max: %.2f / %.2f / %.2f ms",
        s.pc1_avg, s.pc1_p99, s.pc1_max);
    // gpu
    OsdLine(hdc,R,cy,ROW_H,SpeedColor(s.gpu_dispatch,1,2),g_fnt_sm,
        L" gpu_disp %.3f ms  gpu_ms %.2f ms",
        s.gpu_dispatch, s.gpu_ms);
    // send errors
    COLORREF se_c=(s.send_err==0)?RGB(100,230,100):RGB(255,80,60);
    OsdLine(hdc,R,cy,ROW_H,se_c,g_fnt_sm,
        L" send_err %-4d  uptime %.0fs  sid: %.10hs",
        s.send_err, s.uptime_tx, s.tx_sid);

    // full path summary (big)
    cy=175;
    COLORREF fp_c=(full_p99<10)?RGB(100,240,100):(full_p99<15)?RGB(255,200,60):RGB(255,80,60);
    AlphaRect(hdc, 2, cy-2, 636, 23, RGB(0,0,0), 204);
    OsdLine(hdc,6,cy,22,fp_c,g_fnt_bd,
        L" Full-path p99: %.2f ms  (pc1: %.2f + rseg: %.2f)   rfps: %.0f",
        full_p99, s.pc1_p99, s.rseg_p99, s.rfps);

    // ── Graphs ────────────────────────────────────────────────────────────────
    const int GY=228, GH=55, GW=140, GPAD=12;
    // graph 1: FPS
    {
        std::deque<float> h; { std::lock_guard<std::mutex> lk(g_smtx); h=g_hist_fps; }
        OsdLine(hdc,6,*(int*)&GY,0,RGB(160,200,255),g_fnt_sm, L" FPS (0-280)"); // label above
        // actual y
        int ly=GY+13; // title height
        RECT gr={6,ly,6+GW,ly+GH}; FillRect(hdc,&gr,(HBRUSH)GetStockObject(BLACK_BRUSH));
        DrawSparkline(hdc,6,ly,GW,GH,h,280.f,RGB(80,200,80),200.f);
        wchar_t vl[24]; swprintf_s(vl,L"%.0f",h.empty()?0.f:h.back());
        SetTextColor(hdc,RGB(80,200,80)); SelectObject(hdc,g_fnt_sm);
        TextOutW(hdc,6+GW+2,ly+GH/2-6,vl,(int)wcslen(vl));
    }
    // graph 2: rseg_p99
    {
        std::deque<float> h; { std::lock_guard<std::mutex> lk(g_smtx); h=g_hist_rseg; }
        int gx=6+GW+GPAD;
        int ly=GY+13;
        RECT gr={gx,ly,gx+GW,ly+GH}; FillRect(hdc,&gr,(HBRUSH)GetStockObject(BLACK_BRUSH));
        SetTextColor(hdc,RGB(255,200,80)); SelectObject(hdc,g_fnt_sm);
        TextOutW(hdc,gx,GY,L" rseg p99 (0-30ms)",18);
        DrawSparkline(hdc,gx,ly,GW,GH,h,30.f,RGB(255,200,60),10.f);
        wchar_t vl[24]; swprintf_s(vl,L"%.1fms",h.empty()?0.f:h.back());
        SetTextColor(hdc,RGB(255,200,60)); TextOutW(hdc,gx+GW+2,ly+GH/2-6,vl,(int)wcslen(vl));
    }
    // graph 3: pc1_p99
    {
        std::deque<float> h; { std::lock_guard<std::mutex> lk(g_smtx); h=g_hist_pc1; }
        int gx=6+2*(GW+GPAD);
        int ly=GY+13;
        RECT gr={gx,ly,gx+GW,ly+GH}; FillRect(hdc,&gr,(HBRUSH)GetStockObject(BLACK_BRUSH));
        SetTextColor(hdc,RGB(200,150,255)); SelectObject(hdc,g_fnt_sm);
        TextOutW(hdc,gx,GY,L" pc1 p99 (0-10ms)",17);
        DrawSparkline(hdc,gx,ly,GW,GH,h,10.f,RGB(180,120,255),5.f);
        wchar_t vl[24]; swprintf_s(vl,L"%.2fms",h.empty()?0.f:h.back());
        SetTextColor(hdc,RGB(180,120,255)); TextOutW(hdc,gx+GW+2,ly+GH/2-6,vl,(int)wcslen(vl));
    }
    // graph 4: full-path p99
    {
        std::deque<float> h; { std::lock_guard<std::mutex> lk(g_smtx); h=g_hist_full; }
        int gx=6+3*(GW+GPAD);
        int ly=GY+13;
        RECT gr={gx,ly,gx+GW,ly+GH}; FillRect(hdc,&gr,(HBRUSH)GetStockObject(BLACK_BRUSH));
        SetTextColor(hdc,RGB(255,140,100)); SelectObject(hdc,g_fnt_sm);
        TextOutW(hdc,gx,GY,L" full p99 (0-20ms)",18);
        DrawSparkline(hdc,gx,ly,GW,GH,h,20.f,RGB(255,140,80),10.f);
        wchar_t vl[24]; swprintf_s(vl,L"%.2fms",h.empty()?0.f:h.back());
        SetTextColor(hdc,RGB(255,140,80)); TextOutW(hdc,gx+GW+2,ly+GH/2-6,vl,(int)wcslen(vl));
    }

    ReleaseDC(hwnd,hdc);
}

// ── Render ────────────────────────────────────────────────────────────────────
static HWND g_hwnd=nullptr;
static void Render(bool valid){
    static LARGE_INTEGER freq={},prev={};
    static int cnt=0; static float acc=0.f;
    if(!freq.QuadPart) QueryPerformanceFrequency(&freq);
    LARGE_INTEGER now; QueryPerformanceCounter(&now);
    if(prev.QuadPart){
        float dt=(float)(now.QuadPart-prev.QuadPart)/(float)freq.QuadPart;
        acc+=dt; cnt++;
        if(acc>=0.5f){ g_rfps.store(cnt/acc); acc=0; cnt=0; }
    }
    prev=now;

    float clear[4]={0.05f,0.05f,0.06f,1};
    D3D11_VIEWPORT vp={0,0,(float)WIN_W,(float)WIN_H,0,1};
    g_ctx->RSSetViewports(1,&vp);
    g_ctx->OMSetRenderTargets(1,&g_rtv,nullptr);
    g_ctx->ClearRenderTargetView(g_rtv,clear);

    Stats s; { std::lock_guard<std::mutex> lk(g_smtx); s=g_stats; }
    bool live=s.rx_run&&valid, err=s.mismatch>0||s.dropped>0;
    D3D11_MAPPED_SUBRESOURCE ms;
    if(SUCCEEDED(g_ctx->Map(g_cb,0,D3D11_MAP_WRITE_DISCARD,0,&ms))){
        CBData* cb=(CBData*)ms.pData;
        if(!g_pView){ cb->sig_color[0]=0.4f; cb->sig_color[1]=0.4f; cb->sig_color[2]=0.4f; cb->sig_color[3]=1; }
        else if(err){ cb->sig_color[0]=1.f;  cb->sig_color[1]=0.1f; cb->sig_color[2]=0.f;  cb->sig_color[3]=1; }
        else if(live){ cb->sig_color[0]=0.0f; cb->sig_color[1]=1.f;  cb->sig_color[2]=0.1f; cb->sig_color[3]=1; }
        else          { cb->sig_color[0]=1.f;  cb->sig_color[1]=0.8f; cb->sig_color[2]=0.f;  cb->sig_color[3]=1; }
        cb->vid_h=(float)VCAM_H; cb->win_h=(float)WIN_H;
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

    int mode=g_osd_mode.load();
    if(mode>0 && g_hwnd){
        Stats s2; { std::lock_guard<std::mutex> lk(g_smtx); s2=g_stats; }
        DrawOSD(g_hwnd,s2,mode);
    }
}

// ── WndProc ───────────────────────────────────────────────────────────────────
static LRESULT CALLBACK WndProc(HWND hw,UINT msg,WPARAM wp,LPARAM lp){
    switch(msg){
    case WM_DESTROY: PostQuitMessage(0); return 0;
    case WM_KEYDOWN:
        if(wp==VK_ESCAPE){ PostQuitMessage(0); return 0; }
        if(wp=='O'){ g_osd_mode.store((g_osd_mode.load()+1)%3); return 0; }
        break;
    case WM_RBUTTONDOWN:{
        POINT pt={GET_X_LPARAM(lp),GET_Y_LPARAM(lp)};
        ClientToScreen(hw,&pt);
        int mode=g_osd_mode.load();
        HMENU hm=CreatePopupMenu();
        AppendMenuW(hm,MF_STRING|(mode==2?MF_CHECKED:0),IDM_OSD_FULL, L"Full OSD (stats + graphs)");
        AppendMenuW(hm,MF_STRING|(mode==1?MF_CHECKED:0),IDM_OSD_MINI, L"Mini OSD");
        AppendMenuW(hm,MF_STRING|(mode==0?MF_CHECKED:0),IDM_OSD_OFF,  L"OSD Off");
        AppendMenuW(hm,MF_SEPARATOR,IDM_QUIT-1,nullptr);
        AppendMenuW(hm,MF_STRING,IDM_QUIT,L"Quit  [ESC]");
        TrackPopupMenu(hm,TPM_RIGHTBUTTON,pt.x,pt.y,0,hw,nullptr);
        DestroyMenu(hm);
        return 0;
    }
    case WM_COMMAND:
        switch(LOWORD(wp)){
        case IDM_OSD_FULL: g_osd_mode.store(2); return 0;
        case IDM_OSD_MINI: g_osd_mode.store(1); return 0;
        case IDM_OSD_OFF:  g_osd_mode.store(0); return 0;
        case IDM_QUIT:     PostQuitMessage(0);   return 0;
        }
        break;
    case WM_TIMER:
        return 0;
    }
    return DefWindowProcW(hw,msg,wp,lp);
}

// ── main ──────────────────────────────────────────────────────────────────────
int WINAPI WinMain(HINSTANCE hi,HINSTANCE,LPSTR,int){
    wchar_t exe[MAX_PATH]; GetModuleFileNameW(nullptr,exe,MAX_PATH);
    { wchar_t* p=wcsrchr(exe,L'\\'); if(p) *p=0; }
    bool found=false;
    for(int up=0;up<4&&!found;up++){
        swprintf_s(g_run_dir,L"%s\\run",exe);
        if(GetFileAttributesW(g_run_dir)!=INVALID_FILE_ATTRIBUTES){ found=true; break; }
        wchar_t* p=wcsrchr(exe,L'\\'); if(p) *p=0; else break;
    }
    if(!found) swprintf_s(g_run_dir,L"%s\\run",exe);

    g_hMap=OpenFileMappingA(FILE_MAP_READ,FALSE,SHM_NAME);
    if(g_hMap) g_pView=MapViewOfFile(g_hMap,FILE_MAP_READ,0,0,HDR_SIZE+FRAME_BYTES);

    WNDCLASSEXW wc={sizeof(wc)};
    wc.style=CS_HREDRAW|CS_VREDRAW; wc.lpfnWndProc=WndProc; wc.hInstance=hi;
    wc.hCursor=LoadCursorW(nullptr,MAKEINTRESOURCEW(32512));
    wc.lpszClassName=L"VCMonitor";
    if(!RegisterClassExW(&wc)){
        wchar_t e[128]; swprintf_s(e,L"RegisterClassExW: %lu",GetLastError());
        MessageBoxW(nullptr,e,L"Error",MB_OK); return 1;
    }
    RECT r={0,0,WIN_W,WIN_H}; AdjustWindowRect(&r,WS_OVERLAPPEDWINDOW,FALSE);
    g_hwnd=CreateWindowExW(0,L"VCMonitor",L"vc-monitor",
        WS_OVERLAPPEDWINDOW,CW_USEDEFAULT,CW_USEDEFAULT,r.right-r.left,r.bottom-r.top,
        nullptr,nullptr,hi,nullptr);
    if(!g_hwnd){
        wchar_t e[128]; swprintf_s(e,L"CreateWindowExW: %lu",GetLastError());
        MessageBoxW(nullptr,e,L"Error",MB_OK); return 1;
    }
    if(!InitD3D(g_hwnd)){
        MessageBoxW(g_hwnd,L"D3D11 init failed.",L"Error",MB_OK); return 1;
    }
    InitFonts();
    { static uint8_t grey[FRAME_BYTES]; memset(grey,128,FRAME_BYTES); UploadFrame(grey); }

    ShowWindow(g_hwnd,SW_SHOW); UpdateWindow(g_hwnd);
    SetTimer(g_hwnd,1,500,nullptr);
    std::thread st(StatsThread); st.detach();

    MSG msg={};
    while(true){
        while(PeekMessageW(&msg,nullptr,0,0,PM_REMOVE)){
            TranslateMessage(&msg); DispatchMessageW(&msg);
            if(msg.message==WM_QUIT) goto done;
        }
        auto fi=ReadSHMFrame();
        if(fi.valid&&fi.fid!=g_last_fid.load()){
            g_last_fid.store(fi.fid); g_has_signal.store(true); UploadFrame(g_frame_buf);
        }
        Render(fi.valid&&g_has_signal.load());
        Sleep(1);
    }
done:
    if(g_pView) UnmapViewOfFile(g_pView);
    if(g_hMap)  CloseHandle(g_hMap);
    if(g_fnt_sm) DeleteObject(g_fnt_sm);
    if(g_fnt_md) DeleteObject(g_fnt_md);
    if(g_fnt_bd) DeleteObject(g_fnt_bd);
    return 0;
}
