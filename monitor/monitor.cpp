// monitor/monitor.cpp — vc-monitor.exe
// D3D11 NV12 preview + GDI OSD overlay (text + fps/latency sparklines).
// OSD toggle: right-click context menu.  ESC=quit.
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
#include <deque>
#pragma comment(lib,"d3d11.lib")
#pragma comment(lib,"dxgi.lib")
#pragma comment(lib,"d3dcompiler.lib")
#pragma comment(lib,"gdi32.lib")

// ── constants ─────────────────────────────────────────────────────────────────
static const int    VCAM_W      = 640;
static const int    VCAM_H      = 640;
static const int    WIN_W       = 900;   // wider to hold graph panel
static const int    WIN_H       = 700;
static const UINT   FRAME_BYTES = 614400;
static const UINT   HDR_SIZE    = 24;
static const UINT32 SHM_MAGIC   = 0x5643414Du;
static const char*  SHM_NAME    = "Local\\VirtualCaptureVcamFrame";
static const int    HIST_N      = 120;   // sparkline history points

// Context-menu IDs
enum { IDM_OSD_TOGGLE=1001, IDM_OSD_FULL, IDM_OSD_MINI, IDM_SEP, IDM_QUIT };

static wchar_t g_run_dir[MAX_PATH];

// ── SHM header ────────────────────────────────────────────────────────────────
#pragma pack(push,1)
struct ShmHdr { uint32_t magic; int32_t seq; uint32_t frame_id; uint64_t hash; uint32_t size; };
#pragma pack(pop)

// ── HLSL ──────────────────────────────────────────────────────────────────────
static const char* kVS = R"HLSL(
void VS(uint id:SV_VertexID, out float4 pos:SV_Position, out float2 uv:TEXCOORD) {
    uv  = float2((id&1)?1.0f:0.0f, (id>>1)?1.0f:0.0f);
    pos = float4(uv.x*2.0f-1.0f, 1.0f-uv.y*2.0f, 0.0f, 1.0f);
}
)HLSL";

// NV12 → RGB, video occupies left VCAM_W px of WIN_W.  Right panel = dark bg.
static const char* kPS = R"HLSL(
Texture2D<float>  TexY  : register(t0);
Texture2D<float2> TexUV : register(t1);
SamplerState      Smp   : register(s0);
cbuffer CB : register(b0) {
    float4 sig_color;   // status dot color
    float  win_w;       // total window width
    float  vid_w;       // video region width (VCAM_W)
    float  win_h;
    float  pad;
};
float4 PS(float4 pos:SV_Position, float2 uv:TEXCOORD):SV_Target {
    float px=pos.x, py=pos.y;
    // right panel (graph area): dark
    if (px >= vid_w) return float4(0.07f,0.07f,0.09f,1.0f);
    // status dot top-left
    if (px>=4&&px<20&&py>=4&&py<20) return sig_color;
    // NV12 -> RGB
    float2 vuv = float2(px/vid_w, py/win_h);
    float Y  = TexY .Sample(Smp,vuv).r;
    float2 UV= TexUV.Sample(Smp,vuv).rg;
    float Cb=UV.r-0.5f, Cr=UV.g-0.5f;
    float y=(Y-16.0f/255.0f)*(255.0f/219.0f);
    float r=saturate(y+1.402f*Cr);
    float g=saturate(y-0.344136f*Cb-0.714136f*Cr);
    float b=saturate(y+1.772f*Cb);
    return float4(r,g,b,1.0f);
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

struct CBData { float sig_color[4]; float win_w, vid_w, win_h, pad; };

// ── SHM ───────────────────────────────────────────────────────────────────────
static HANDLE  g_hMap  = nullptr;
static void*   g_pView = nullptr;
static uint8_t g_frame_buf[FRAME_BYTES];

struct FrameInfo { uint32_t fid; uint64_t hash; bool valid; };
static FrameInfo ReadSHMFrame() {
    if (!g_pView) return {0,0,false};
    ShmHdr h1; memcpy(&h1, g_pView, HDR_SIZE);
    if (h1.magic!=SHM_MAGIC||(h1.seq&1)||h1.size!=FRAME_BYTES) return {h1.frame_id,h1.hash,false};
    memcpy(g_frame_buf,(uint8_t*)g_pView+HDR_SIZE,FRAME_BYTES);
    ShmHdr h2; memcpy(&h2,g_pView,HDR_SIZE);
    return {h1.frame_id,h1.hash,h1.seq==h2.seq};
}

// ── Stats + history ───────────────────────────────────────────────────────────
struct Stats {
    float fps=0,rseg_avg=0,rseg_p50=0,rseg_p99=0,rseg_max=0;
    float pc1_avg=0,pc1_p99=0,pc1_max=0,gpu_ms=0,gpu_dispatch=0;
    float bps=0,pps=0;
    int   complete=0,dropped=0,mismatch=0,shm_pub=0,gaps=0,packets_lost=0;
    bool  rx_run=false,tx_run=false;
    float uptime_rx=0,uptime_tx=0;
};
static std::mutex  g_smtx;
static Stats       g_stats;
static std::deque<float> g_hist_fps, g_hist_rseg, g_hist_rfps;
static std::atomic<float>    g_rfps{0.f};
static std::atomic<uint32_t> g_last_fid{0};
static std::atomic<bool>     g_has_signal{false};

// OSD state: 0=off, 1=mini (top-left stats only), 2=full (stats+graphs)
static std::atomic<int>  g_osd_mode{2};  // default full OSD

static float jf(const char* b, const char* k) {
    const char* p=strstr(b,k); if(!p) return 0.f;
    p=strchr(p,':'); if(!p) return 0.f;
    while(*p==' '||*p==':') p++; return (float)atof(p);
}
static int  ji(const char* b, const char* k){ return (int)jf(b,k); }
static bool jb(const char* b, const char* k){
    const char* p=strstr(b,k); if(!p) return false;
    p=strchr(p,':'); if(!p) return false;
    while(*p==' '||*p==':') p++; return strncmp(p,"true",4)==0;
}
static void ReadLatestStats(const wchar_t* pat, Stats& s, bool is_rx) {
    char buf[8192]="";
    WIN32_FIND_DATAW fd; HANDLE hf=FindFirstFileW(pat,&fd);
    if (hf==INVALID_HANDLE_VALUE) return;
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
        s.complete=ji(buf,"complete"); s.dropped=ji(buf,"dropped");
        s.mismatch=ji(buf,"hash_mismatch"); s.shm_pub=ji(buf,"shm_published");
        s.gaps=ji(buf,"gaps"); s.packets_lost=ji(buf,"packets_lost");
        s.bps=jf(buf,"bps"); s.pps=jf(buf,"pps");
        s.rx_run=jb(buf,"running"); s.uptime_rx=jf(buf,"uptime_s");
    } else {
        s.fps=jf(buf,"fps_actual");
        s.pc1_avg=jf(buf,"pc1_seg_avg_ms"); s.pc1_p99=jf(buf,"pc1_seg_p99_ms");
        s.pc1_max=jf(buf,"pc1_seg_max_ms");
        s.gpu_ms=jf(buf,"gpu_ms"); s.gpu_dispatch=jf(buf,"gpu_dispatch_ms");
        s.tx_run=jb(buf,"running"); s.uptime_tx=jf(buf,"uptime_s");
    }
}
static void StatsThread() {
    wchar_t rp[MAX_PATH],tp[MAX_PATH];
    swprintf_s(rp,L"%s\\*.receiver.stats.json",g_run_dir);
    swprintf_s(tp,L"%s\\*.sender.stats.json",  g_run_dir);
    while(true){
        Stats s; ReadLatestStats(rp,s,true); ReadLatestStats(tp,s,false);
        { std::lock_guard<std::mutex> lk(g_smtx);
          g_stats=s;
          if(s.fps>0){
              g_hist_fps.push_back(s.fps);   if((int)g_hist_fps.size()>HIST_N) g_hist_fps.pop_front();
          }
          if(s.rseg_p99>0){
              g_hist_rseg.push_back(s.rseg_p99); if((int)g_hist_rseg.size()>HIST_N) g_hist_rseg.pop_front();
          }
          float rf=g_rfps.load();
          if(rf>0){
              g_hist_rfps.push_back(rf); if((int)g_hist_rfps.size()>HIST_N) g_hist_rfps.pop_front();
          }
        }
        Sleep(400);
    }
}

// ── D3D init ──────────────────────────────────────────────────────────────────
static bool CompileShader(const char* src,const char* ep,const char* model,ID3DBlob** out){
    ID3DBlob* err=nullptr;
    HRESULT hr=D3DCompile(src,strlen(src),nullptr,nullptr,nullptr,ep,model,0,0,out,&err);
    if(FAILED(hr)&&err){ char m[512]; snprintf(m,512,"Shader '%s':\n%s",ep,(char*)err->GetBufferPointer()); MessageBoxA(nullptr,m,"Shader Error",MB_OK|MB_ICONERROR); err->Release(); }
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
    D3D11_TEXTURE2D_DESC td={};
    td.Width=VCAM_W; td.Height=VCAM_H; td.MipLevels=1; td.ArraySize=1;
    td.Format=DXGI_FORMAT_R8_UNORM; td.SampleDesc.Count=1;
    td.Usage=D3D11_USAGE_DYNAMIC; td.BindFlags=D3D11_BIND_SHADER_RESOURCE; td.CPUAccessFlags=D3D11_CPU_ACCESS_WRITE;
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
// Draws OSD panel on right side (x >= VCAM_W) and text overlay on video.
// Called after D3D Present via GetDC on the HWND (DXGI_SWAP_EFFECT_DISCARD allows this).

static HFONT g_font_sm  = nullptr;  // 12pt
static HFONT g_font_hdr = nullptr;  // 14pt bold
static HFONT g_font_val = nullptr;  // 20pt bold (big numbers)

static void InitFonts(){
    g_font_sm  = CreateFontW(-12,0,0,0,FW_NORMAL,0,0,0,DEFAULT_CHARSET,0,0,CLEARTYPE_QUALITY,0,L"Consolas");
    g_font_hdr = CreateFontW(-13,0,0,0,FW_BOLD,  0,0,0,DEFAULT_CHARSET,0,0,CLEARTYPE_QUALITY,0,L"Consolas");
    g_font_val = CreateFontW(-20,0,0,0,FW_BOLD,  0,0,0,DEFAULT_CHARSET,0,0,CLEARTYPE_QUALITY,0,L"Consolas");
}

// draw a mini sparkline in rect [x,y,x+w,y+h], values in dq, max_val scale
static void DrawSparkline(HDC hdc, int x, int y, int w, int h,
                           const std::deque<float>& dq, float max_val,
                           COLORREF line_col, COLORREF warn_col, float warn_thresh){
    if(dq.empty()) return;
    // background already dark; draw grid line at warn_thresh
    if(warn_thresh>0 && max_val>0){
        int gy=y+h-(int)(warn_thresh/max_val*h);
        if(gy>y && gy<y+h){
            HPEN gpen=CreatePen(PS_DOT,1,RGB(100,100,60));
            HPEN old=(HPEN)SelectObject(hdc,gpen);
            MoveToEx(hdc,x,gy,nullptr); LineTo(hdc,x+w,gy);
            SelectObject(hdc,old); DeleteObject(gpen);
        }
    }
    int n=(int)dq.size(); if(n<2) return;
    HPEN lpen=CreatePen(PS_SOLID,2,line_col);
    HPEN old=(HPEN)SelectObject(hdc,lpen);
    int start=n>w?n-w:0;
    for(int i=start;i<n;i++){
        float v=dq[i]; if(max_val>0 && v>max_val) v=max_val;
        int px=x+(i-start);
        int py=y+h-(max_val>0?(int)(v/max_val*h):0);
        if(py<y) py=y; if(py>=y+h) py=y+h-1;
        COLORREF c=(warn_thresh>0&&dq[i]>warn_thresh)?warn_col:line_col;
        // change pen color per segment if needed
        if(i==start){ MoveToEx(hdc,px,py,nullptr); }
        else { LineTo(hdc,px,py); }
    }
    SelectObject(hdc,old); DeleteObject(lpen);
    // draw last value label
    if(!dq.empty()){
        char lbl[24]; snprintf(lbl,sizeof(lbl),"%.1f",dq.back());
        SetTextColor(hdc,line_col); SetBkMode(hdc,TRANSPARENT);
        TextOutA(hdc,x+w+2,y+h/2-6,lbl,(int)strlen(lbl));
    }
}

static void DrawOSD(HWND hwnd, const Stats& s, int osd_mode){
    if(osd_mode==0) return;

    HDC hdc=GetDC(hwnd);
    if(!hdc) return;

    const int VW=VCAM_W, PX=VCAM_W+4;  // panel x start
    const int PW=WIN_W-VCAM_W-8;        // panel width
    int cy=8;                            // current y in panel

    // ── semi-transparent video overlay (top-left stats block) ──────────────
    // Draw on video area: semi-dark background rectangle
    RECT vr={4,24,350,osd_mode==1?80:116};
    HBRUSH bg=CreateSolidBrush(RGB(0,0,0));
    // alpha blend manually: just a dark filled rect with 60% coverage
    SetDCBrushColor(hdc,RGB(0,0,0));
    FillRect(hdc,&vr,(HBRUSH)GetStockObject(BLACK_BRUSH));

    // video overlay text
    SelectObject(hdc,g_font_hdr);
    SetBkMode(hdc,TRANSPARENT);
    char buf[128];
    // line 1: fps + rfps
    COLORREF fps_col=(s.fps>=230)?RGB(100,255,100):(s.fps>=180)?RGB(255,220,60):RGB(255,80,80);
    SetTextColor(hdc,fps_col);
    snprintf(buf,sizeof(buf)," FPS: %.0f / rfps %.0f",s.fps,g_rfps.load());
    TextOutA(hdc,6,26,buf,(int)strlen(buf));
    // line 2: full-path p99
    float full_p99=s.pc1_p99+s.rseg_p99;
    COLORREF lat_col=(full_p99<10)?RGB(100,255,100):(full_p99<15)?RGB(255,220,60):RGB(255,80,80);
    SetTextColor(hdc,lat_col);
    snprintf(buf,sizeof(buf)," p99: %.1fms (pc1:%.1f r:%.1f)",full_p99,s.pc1_p99,s.rseg_p99);
    TextOutA(hdc,6,42,buf,(int)strlen(buf));
    if(osd_mode==2){
        // line 3: mismatch / dropped
        COLORREF err_col=(s.mismatch==0&&s.dropped==0)?RGB(100,255,100):RGB(255,80,80);
        SetTextColor(hdc,err_col);
        snprintf(buf,sizeof(buf)," ok:%-7d drop:%d mismatch:%d",s.complete,s.dropped,s.mismatch);
        TextOutA(hdc,6,58,buf,(int)strlen(buf));
        // line 4: rseg_max + gpu
        SetTextColor(hdc,RGB(180,180,200));
        snprintf(buf,sizeof(buf)," rmax:%.0fms gpu:%.2fms bps:%.2fG",s.rseg_max,s.gpu_dispatch,s.bps/1e9f);
        TextOutA(hdc,6,74,buf,(int)strlen(buf));
        // line 5: signal status
        bool live=s.rx_run&&g_has_signal.load();
        SetTextColor(hdc,live?RGB(80,255,80):RGB(255,200,60));
        snprintf(buf,sizeof(buf)," %s  uptime:%.0fs",live?"● LIVE":"○ IDLE",s.uptime_rx);
        TextOutA(hdc,6,90,buf,(int)strlen(buf));
    }
    DeleteObject(bg);

    if(osd_mode==1){ ReleaseDC(hwnd,hdc); return; }

    // ── right panel: detailed stats + graphs ──────────────────────────────
    // Panel background (already rendered by shader as dark, just draw text/graphs)

    // header
    SelectObject(hdc,g_font_hdr);
    SetTextColor(hdc,RGB(180,200,255));
    auto txt=[&](const char* s){ TextOutA(hdc,PX,cy,s,(int)strlen(s)); cy+=15; };
    auto row=[&](const char* label, const char* val, COLORREF vc){
        SetTextColor(hdc,RGB(140,140,160));
        TextOutA(hdc,PX,cy,label,(int)strlen(label));
        SetTextColor(hdc,vc);
        TextOutA(hdc,PX+120,cy,val,(int)strlen(val));
        cy+=14;
    };

    txt("── Receiver ──");
    snprintf(buf,sizeof(buf),"%.1fms",s.rseg_avg);
    row("rseg avg",buf,(s.rseg_avg<5)?RGB(100,220,100):RGB(255,180,60));
    snprintf(buf,sizeof(buf),"%.1fms",s.rseg_p50);
    row("rseg p50",buf,(s.rseg_p50<5)?RGB(100,220,100):RGB(255,180,60));
    snprintf(buf,sizeof(buf),"%.1fms",s.rseg_p99);
    row("rseg p99",buf,(s.rseg_p99<10)?RGB(100,220,100):(s.rseg_p99<15)?RGB(255,180,60):RGB(255,80,80));
    snprintf(buf,sizeof(buf),"%.1fms",s.rseg_max);
    row("rseg max",buf,(s.rseg_max<100)?RGB(100,220,100):RGB(255,80,80));
    snprintf(buf,sizeof(buf),"%d",s.complete);   row("frames ok", buf,RGB(180,255,180));
    snprintf(buf,sizeof(buf),"%d",s.dropped);    row("dropped",   buf,s.dropped?RGB(255,80,80):RGB(100,220,100));
    snprintf(buf,sizeof(buf),"%d",s.mismatch);   row("mismatch",  buf,s.mismatch?RGB(255,80,80):RGB(100,220,100));
    snprintf(buf,sizeof(buf),"%d",s.gaps);       row("gaps",      buf,s.gaps?RGB(255,150,60):RGB(100,220,100));
    snprintf(buf,sizeof(buf),"%d",s.shm_pub);    row("shm_pub",   buf,RGB(180,180,255));
    snprintf(buf,sizeof(buf),"%.2fG",s.bps/1e9f);row("bps",       buf,RGB(200,200,200));
    snprintf(buf,sizeof(buf),"%.0f",s.pps);      row("pps",       buf,RGB(200,200,200));

    cy+=4;
    txt("── Sender ──");
    snprintf(buf,sizeof(buf),"%.1f",s.fps);
    row("fps",buf,(s.fps>=230)?RGB(100,220,100):(s.fps>=180)?RGB(255,180,60):RGB(255,80,80));
    snprintf(buf,sizeof(buf),"%.2fms",s.pc1_avg); row("pc1 avg",buf,(s.pc1_avg<4)?RGB(100,220,100):RGB(255,180,60));
    snprintf(buf,sizeof(buf),"%.2fms",s.pc1_p99); row("pc1 p99",buf,(s.pc1_p99<5)?RGB(100,220,100):RGB(255,180,60));
    snprintf(buf,sizeof(buf),"%.2fms",s.pc1_max); row("pc1 max",buf,(s.pc1_max<20)?RGB(100,220,100):RGB(255,80,80));
    snprintf(buf,sizeof(buf),"%.3fms",s.gpu_dispatch); row("gpu disp",buf,(s.gpu_dispatch<1)?RGB(100,220,100):RGB(255,80,80));
    snprintf(buf,sizeof(buf),"%.2fms",s.gpu_ms); row("gpu ms",buf,RGB(200,200,200));

    cy+=4;
    float full99=s.pc1_p99+s.rseg_p99;
    txt("── Full path ──");
    snprintf(buf,sizeof(buf),"%.2fms",full99);
    row("p99 total",buf,(full99<10)?RGB(100,255,100):(full99<15)?RGB(255,180,60):RGB(255,80,80));
    snprintf(buf,sizeof(buf),"%.0f",g_rfps.load()); row("render fps",buf,RGB(180,200,255));

    cy+=8;
    // ── Graphs ────────────────────────────────────────────────────────────
    SelectObject(hdc,g_font_sm);
    const int GW=PW-36, GH=50;  // graph width/height
    const int GX=PX;

    // fps graph
    SetTextColor(hdc,RGB(180,200,255));
    TextOutA(hdc,GX,cy,"FPS",3); cy+=13;
    {
        RECT gr={GX,cy,GX+GW,cy+GH}; FillRect(hdc,&gr,(HBRUSH)GetStockObject(BLACK_BRUSH));
        std::deque<float> hf,hr; { std::lock_guard<std::mutex> lk(g_smtx); hf=g_hist_fps; hr=g_hist_rfps; }
        // rfps in dim blue
        DrawSparkline(hdc,GX,cy,GW,GH,hr,320.f,RGB(60,80,180),RGB(60,80,180),0);
        // fps in bright
        DrawSparkline(hdc,GX,cy,GW,GH,hf,280.f,RGB(80,220,80),RGB(255,150,60),200.f);
        // axis labels
        SetTextColor(hdc,RGB(100,100,120)); SetBkMode(hdc,TRANSPARENT);
        TextOutA(hdc,GX,cy,"280",3); TextOutA(hdc,GX,cy+GH-12,"0",1);
    }
    cy+=GH+4;

    // rseg_p99 graph
    SetTextColor(hdc,RGB(255,200,100));
    TextOutA(hdc,GX,cy,"rseg p99 (ms)",13); cy+=13;
    {
        RECT gr={GX,cy,GX+GW,cy+GH}; FillRect(hdc,&gr,(HBRUSH)GetStockObject(BLACK_BRUSH));
        std::deque<float> hr; { std::lock_guard<std::mutex> lk(g_smtx); hr=g_hist_rseg; }
        DrawSparkline(hdc,GX,cy,GW,GH,hr,30.f,RGB(255,200,60),RGB(255,80,80),10.f);
        SetTextColor(hdc,RGB(100,100,120));
        TextOutA(hdc,GX,cy,"30ms",4); TextOutA(hdc,GX,cy+GH-12,"0",1);
        // 10ms warning line label
        int wy=cy+GH-(int)(10.f/30.f*GH);
        SetTextColor(hdc,RGB(120,120,60));
        TextOutA(hdc,GX+GW+2,wy,"10ms",4);
    }
    cy+=GH+4;

    ReleaseDC(hwnd,hdc);
}

// ── Render ────────────────────────────────────────────────────────────────────
static HWND g_hwnd=nullptr;

static void Render(bool valid){
    // measure render fps
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

    float clear[4]={0.04f,0.04f,0.05f,1};
    D3D11_VIEWPORT vp={0,0,(float)WIN_W,(float)WIN_H,0,1};
    g_ctx->RSSetViewports(1,&vp);
    g_ctx->OMSetRenderTargets(1,&g_rtv,nullptr);
    g_ctx->ClearRenderTargetView(g_rtv,clear);

    Stats s; { std::lock_guard<std::mutex> lk(g_smtx); s=g_stats; }
    bool live=s.rx_run&&valid;
    bool err =s.mismatch>0||s.dropped>0;
    D3D11_MAPPED_SUBRESOURCE ms;
    if(SUCCEEDED(g_ctx->Map(g_cb,0,D3D11_MAP_WRITE_DISCARD,0,&ms))){
        CBData* cb=(CBData*)ms.pData;
        if(!g_pView){ cb->sig_color[0]=0.4f; cb->sig_color[1]=0.4f; cb->sig_color[2]=0.4f; cb->sig_color[3]=1; }
        else if(err){ cb->sig_color[0]=1.f;  cb->sig_color[1]=0.2f; cb->sig_color[2]=0.f;  cb->sig_color[3]=1; }
        else if(live){ cb->sig_color[0]=0.1f; cb->sig_color[1]=1.f;  cb->sig_color[2]=0.1f; cb->sig_color[3]=1; }
        else          { cb->sig_color[0]=1.f;  cb->sig_color[1]=0.8f; cb->sig_color[2]=0.f;  cb->sig_color[3]=1; }
        cb->win_w=(float)WIN_W; cb->vid_w=(float)VCAM_W; cb->win_h=(float)WIN_H;
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

    // GDI OSD pass (after Present, DISCARD mode allows GetDC)
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
        if(wp=='O'||wp=='o'){  // keyboard shortcut too
            g_osd_mode.store((g_osd_mode.load()+1)%3); return 0;
        }
        break;
    case WM_RBUTTONDOWN:{
        POINT pt={(short)LOWORD(lp),(short)HIWORD(lp)};
        ClientToScreen(hw,&pt);
        HMENU hm=CreatePopupMenu();
        int mode=g_osd_mode.load();
        AppendMenuW(hm,MF_STRING|(mode==2?MF_CHECKED:0),IDM_OSD_FULL, L"Full OSD (stats + graphs)");
        AppendMenuW(hm,MF_STRING|(mode==1?MF_CHECKED:0),IDM_OSD_MINI, L"Mini OSD (top-left only)");
        AppendMenuW(hm,MF_STRING|(mode==0?MF_CHECKED:0),IDM_OSD_TOGGLE,L"OSD Off");
        AppendMenuW(hm,MF_SEPARATOR,0,nullptr);
        AppendMenuW(hm,MF_STRING,IDM_QUIT,L"Quit  [ESC]");
        TrackPopupMenu(hm,TPM_RIGHTBUTTON,pt.x,pt.y,0,hw,nullptr);
        DestroyMenu(hm);
        return 0;
    }
    case WM_COMMAND:
        switch(LOWORD(wp)){
        case IDM_OSD_FULL:   g_osd_mode.store(2); return 0;
        case IDM_OSD_MINI:   g_osd_mode.store(1); return 0;
        case IDM_OSD_TOGGLE: g_osd_mode.store(0); return 0;
        case IDM_QUIT:       PostQuitMessage(0);   return 0;
        }
        break;
    case WM_TIMER:{
        // update title bar with compact summary
        Stats s; { std::lock_guard<std::mutex> lk(g_smtx); s=g_stats; }
        wchar_t title[200];
        swprintf_s(title,L"vc-monitor  fps:%.0f  p99:%.1fms  drop:%d  mismatch:%d  rfps:%.0f  [RClick=OSD] [ESC=quit]",
            s.fps, s.pc1_p99+s.rseg_p99, s.dropped, s.mismatch, g_rfps.load());
        SetWindowTextW(hw,title);
        return 0;
    }
    }
    return DefWindowProcW(hw,msg,wp,lp);
}

// ── main ──────────────────────────────────────────────────────────────────────
int WINAPI WinMain(HINSTANCE hi,HINSTANCE,LPSTR,int){
    // find run dir
    wchar_t exe[MAX_PATH]; GetModuleFileNameW(nullptr,exe,MAX_PATH);
    wchar_t* last=wcsrchr(exe,L'\\'); if(last) *last=0;
    bool found=false;
    for(int up=0;up<4&&!found;up++){
        swprintf_s(g_run_dir,L"%s\\run",exe);
        if(GetFileAttributesW(g_run_dir)!=INVALID_FILE_ATTRIBUTES){ found=true; break; }
        wchar_t* p=wcsrchr(exe,L'\\'); if(p) *p=0; else break;
    }
    if(!found) swprintf_s(g_run_dir,L"%s\\run",exe);

    // SHM
    g_hMap=OpenFileMappingA(FILE_MAP_READ,FALSE,SHM_NAME);
    if(g_hMap) g_pView=MapViewOfFile(g_hMap,FILE_MAP_READ,0,0,HDR_SIZE+FRAME_BYTES);

    // Window
    WNDCLASSEXW wc={sizeof(wc)};
    wc.style=CS_HREDRAW|CS_VREDRAW; wc.lpfnWndProc=WndProc; wc.hInstance=hi;
    wc.hCursor=LoadCursorW(nullptr,(LPCWSTR)IDC_ARROW); wc.lpszClassName=L"VCMonitor";
    if(!RegisterClassExW(&wc)){
        wchar_t e[128]; swprintf_s(e,L"RegisterClassExW: %lu",GetLastError());
        MessageBoxW(nullptr,e,L"Error",MB_OK|MB_ICONERROR); return 1;
    }
    RECT r={0,0,WIN_W,WIN_H}; AdjustWindowRect(&r,WS_OVERLAPPEDWINDOW,FALSE);
    g_hwnd=CreateWindowExW(0,L"VCMonitor",L"vc-monitor [starting...]",
        WS_OVERLAPPEDWINDOW,CW_USEDEFAULT,CW_USEDEFAULT,r.right-r.left,r.bottom-r.top,
        nullptr,nullptr,hi,nullptr);
    if(!g_hwnd){
        wchar_t e[128]; swprintf_s(e,L"CreateWindowExW: %lu",GetLastError());
        MessageBoxW(nullptr,e,L"Error",MB_OK|MB_ICONERROR); return 1;
    }
    if(!InitD3D(g_hwnd)){
        MessageBoxW(g_hwnd,L"D3D11 init failed.",L"Error",MB_OK|MB_ICONERROR); return 1;
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
        if(fi.valid && fi.fid!=g_last_fid.load()){
            g_last_fid.store(fi.fid); g_has_signal.store(true); UploadFrame(g_frame_buf);
        }
        Render(fi.valid&&g_has_signal.load());
        Sleep(1);
    }
done:
    if(g_pView) UnmapViewOfFile(g_pView);
    if(g_hMap)  CloseHandle(g_hMap);
    if(g_font_sm)  DeleteObject(g_font_sm);
    if(g_font_hdr) DeleteObject(g_font_hdr);
    if(g_font_val) DeleteObject(g_font_val);
    return 0;
}
