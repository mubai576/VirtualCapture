// monitor/monitor.cpp — vc-monitor.exe
// D3D11 NV12 preview + GDI OSD overlay.
// OSD style: YouTube-inspired — large bold numbers, drop-shadow text, minimal bg.
// Right-click: Full / Mini / Off.  O=cycle.  ESC=quit.
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
#include <atomic>
#include <thread>
#include <mutex>
#include <deque>
#pragma comment(lib,"d3d11.lib")
#pragma comment(lib,"dxgi.lib")
#pragma comment(lib,"d3dcompiler.lib")
#pragma comment(lib,"gdi32.lib")
#pragma comment(lib,"msimg32.lib")

// ── constants ─────────────────────────────────────────────────────────────────
static const int    VCAM_W      = 640;
static const int    VCAM_H      = 640;
static const int    WIN_W       = 640;
static const int    WIN_H       = 640;
static const UINT   FRAME_BYTES = 614400;
static const UINT   HDR_SIZE    = 24;
static const UINT32 SHM_MAGIC   = 0x5643414Du;
static const char*  SHM_NAME    = "Local\\VirtualCaptureVcamFrame";
static const int    HIST_N      = 120;

enum { IDM_OSD_FULL=1001, IDM_OSD_MINI, IDM_OSD_OFF, IDM_QUIT };
static wchar_t g_run_dir[MAX_PATH];

// ── SHM ───────────────────────────────────────────────────────────────────────
#pragma pack(push,1)
struct ShmHdr { uint32_t magic; int32_t seq; uint32_t frame_id; uint64_t hash; uint32_t size; };
#pragma pack(pop)
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

// ── HLSL ──────────────────────────────────────────────────────────────────────
static const char* kVS = R"HLSL(
void VS(uint id:SV_VertexID,out float4 pos:SV_Position,out float2 uv:TEXCOORD){
    uv=float2((id&1)?1:0,(id>>1)?1:0);
    pos=float4(uv.x*2-1,1-uv.y*2,0,1);
})HLSL";

static const char* kPS = R"HLSL(
Texture2D<float>  TexY :register(t0);
Texture2D<float2> TexUV:register(t1);
SamplerState      Smp  :register(s0);
cbuffer CB:register(b0){ float4 sig; float pad[4]; };
float4 PS(float4 pos:SV_Position,float2 uv:TEXCOORD):SV_Target{
    if(pos.x>=4&&pos.x<20&&pos.y>=4&&pos.y<20) return sig;
    float2 v=float2(pos.x/640,pos.y/640);
    float  Y =TexY .Sample(Smp,v).r;
    float2 UV=TexUV.Sample(Smp,v).rg;
    float Cb=UV.r-.5f,Cr=UV.g-.5f;
    float y=(Y-16.f/255.f)*(255.f/219.f);
    return float4(saturate(y+1.402f*Cr),
                  saturate(y-.344136f*Cb-.714136f*Cr),
                  saturate(y+1.772f*Cb),1);
})HLSL";

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

struct CBData { float sig[4]; float pad[4]; };

// ── Stats ─────────────────────────────────────────────────────────────────────
struct Stats {
    float fps=0, rfps=0;
    float rseg_avg=0, rseg_p50=0, rseg_p99=0, rseg_max=0;
    float pc1_avg=0, pc1_p99=0, pc1_max=0;
    float gpu_ms=0, gpu_dispatch=0;
    float bps=0, pps=0, ia_avg=0, ia_p99=0;
    int   complete=0, dropped=0, mismatch=0, shm_pub=0, gaps=0, pkt_lost=0, send_err=0;
    bool  rx_run=false, tx_run=false;
    float uptime_rx=0, uptime_tx=0;
    int   cpu_pct=0; bool gpu_convert=false;
    char  rx_sid[32]={}, tx_sid[32]={};
};
static std::mutex g_smtx;
static Stats      g_stats;
static std::deque<float> g_h_fps, g_h_rseg, g_h_pc1, g_h_full;
static std::atomic<float>    g_rfps{0.f};
static std::atomic<uint32_t> g_last_fid{0};
static std::atomic<bool>     g_has_signal{false};
static std::atomic<int>      g_osd_mode{2};

// JSON helpers
static float jf(const char* b,const char* k){ const char* p=strstr(b,k); if(!p)return 0.f; p=strchr(p,':'); if(!p)return 0.f; while(*p==' '||*p==':')p++; return(float)atof(p); }
static int   ji(const char* b,const char* k){ return(int)jf(b,k); }
static bool  jb(const char* b,const char* k){ const char* p=strstr(b,k); if(!p)return false; p=strchr(p,':'); if(!p)return false; while(*p==' '||*p==':')p++; return strncmp(p,"true",4)==0; }
static void  jstr(const char* b,const char* k,char* out,int n){ const char* p=strstr(b,k); if(!p){out[0]=0;return;} p=strchr(p,':'); if(!p){out[0]=0;return;} while(*p==' '||*p==':'||*p=='"')p++; int i=0; while(i<n-1&&*p&&*p!='"'&&*p!=',')out[i++]=*p++; out[i]=0; }

static void ReadStats(const wchar_t* pat, Stats& s, bool rx) {
    char buf[8192]="";
    WIN32_FIND_DATAW fd; HANDLE hf=FindFirstFileW(pat,&fd);
    if(hf==INVALID_HANDLE_VALUE)return;
    WIN32_FIND_DATAW best=fd;
    while(FindNextFileW(hf,&fd)) if(CompareFileTime(&fd.ftLastWriteTime,&best.ftLastWriteTime)>0) best=fd;
    FindClose(hf);
    wchar_t path[MAX_PATH]; swprintf_s(path,L"%s\\%s",g_run_dir,best.cFileName);
    HANDLE h=CreateFileW(path,GENERIC_READ,FILE_SHARE_READ|FILE_SHARE_WRITE,nullptr,OPEN_EXISTING,0,nullptr);
    if(h==INVALID_HANDLE_VALUE)return;
    DWORD n=0; ReadFile(h,buf,sizeof(buf)-1,&n,nullptr); CloseHandle(h); buf[n]=0;
    if(rx){
        s.rseg_avg=jf(buf,"rseg_avg_ms"); s.rseg_p50=jf(buf,"rseg_p50_ms");
        s.rseg_p99=jf(buf,"rseg_p99_ms"); s.rseg_max=jf(buf,"rseg_max_ms");
        s.ia_avg=jf(buf,"inter_arrival_avg_ms"); s.ia_p99=jf(buf,"inter_arrival_p99_ms");
        s.bps=jf(buf,"bps"); s.pps=jf(buf,"pps");
        s.complete=ji(buf,"complete"); s.dropped=ji(buf,"dropped");
        s.mismatch=ji(buf,"hash_mismatch"); s.shm_pub=ji(buf,"shm_published");
        s.gaps=ji(buf,"gaps"); s.pkt_lost=ji(buf,"packets_lost");
        s.rx_run=jb(buf,"running"); s.uptime_rx=jf(buf,"uptime_s");
        jstr(buf,"session_id",s.rx_sid,sizeof(s.rx_sid));
    } else {
        s.fps=jf(buf,"fps_actual");
        // fallback: derive from sent_frames / uptime
        if(s.fps<=0){
            float frames=jf(buf,"sent_frames"); float up=jf(buf,"uptime_s");
            if(frames>0&&up>0) s.fps=frames/up;
        }
        s.pc1_avg=jf(buf,"pc1_seg_avg_ms"); s.pc1_p99=jf(buf,"pc1_seg_p99_ms"); s.pc1_max=jf(buf,"pc1_seg_max_ms");
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
        Stats s; ReadStats(rp,s,true); ReadStats(tp,s,false); s.rfps=g_rfps.load();
        { std::lock_guard<std::mutex> lk(g_smtx);
          g_stats=s;
          auto push=[](std::deque<float>&d,float v){ d.push_back(v); if((int)d.size()>HIST_N)d.pop_front(); };
          if(s.fps>0)      push(g_h_fps, s.fps);
          if(s.rseg_p99>0) push(g_h_rseg,s.rseg_p99);
          if(s.pc1_p99>0)  push(g_h_pc1, s.pc1_p99);
          if(s.fps>0&&s.rseg_p99>0) push(g_h_full, s.pc1_p99+s.rseg_p99);
        }
        Sleep(400);
    }
}

// ── D3D init ──────────────────────────────────────────────────────────────────
static bool CompileShader(const char* src,const char* ep,const char* model,ID3DBlob** out){
    ID3DBlob* err=nullptr;
    HRESULT hr=D3DCompile(src,strlen(src),nullptr,nullptr,nullptr,ep,model,0,0,out,&err);
    if(FAILED(hr)&&err){char m[512];snprintf(m,512,"Shader '%s':\n%s",ep,(char*)err->GetBufferPointer());MessageBoxA(nullptr,m,"Shader",MB_OK);err->Release();}
    return SUCCEEDED(hr);
}
static bool InitD3D(HWND hw){
    DXGI_SWAP_CHAIN_DESC sd={};
    sd.BufferCount=2;sd.BufferDesc.Width=WIN_W;sd.BufferDesc.Height=WIN_H;
    sd.BufferDesc.Format=DXGI_FORMAT_B8G8R8A8_UNORM;sd.BufferDesc.RefreshRate={0,1};
    sd.BufferUsage=DXGI_USAGE_RENDER_TARGET_OUTPUT;sd.OutputWindow=hw;
    sd.SampleDesc.Count=1;sd.Windowed=TRUE;sd.SwapEffect=DXGI_SWAP_EFFECT_DISCARD;
    sd.Flags=DXGI_SWAP_CHAIN_FLAG_GDI_COMPATIBLE;
    D3D_FEATURE_LEVEL fl;
    if(FAILED(D3D11CreateDeviceAndSwapChain(nullptr,D3D_DRIVER_TYPE_HARDWARE,nullptr,0,nullptr,0,D3D11_SDK_VERSION,&sd,&g_sc,&g_dev,&fl,&g_ctx)))return false;
    ID3D11Texture2D* bb=nullptr; g_sc->GetBuffer(0,IID_PPV_ARGS(&bb));
    if(FAILED(g_dev->CreateRenderTargetView(bb,nullptr,&g_rtv))){bb->Release();return false;} bb->Release();
    ID3DBlob *vb=nullptr,*pb=nullptr;
    if(!CompileShader(kVS,"VS","vs_5_0",&vb))return false;
    if(!CompileShader(kPS,"PS","ps_5_0",&pb))return false;
    g_dev->CreateVertexShader(vb->GetBufferPointer(),vb->GetBufferSize(),nullptr,&g_vs);
    g_dev->CreatePixelShader (pb->GetBufferPointer(),pb->GetBufferSize(),nullptr,&g_ps);
    vb->Release();pb->Release();
    D3D11_SAMPLER_DESC sd2={};sd2.Filter=D3D11_FILTER_MIN_MAG_MIP_LINEAR;sd2.AddressU=sd2.AddressV=sd2.AddressW=D3D11_TEXTURE_ADDRESS_CLAMP;
    g_dev->CreateSamplerState(&sd2,&g_samp);
    D3D11_BUFFER_DESC cbd={sizeof(CBData),D3D11_USAGE_DYNAMIC,D3D11_BIND_CONSTANT_BUFFER,D3D11_CPU_ACCESS_WRITE,0,0};
    g_dev->CreateBuffer(&cbd,nullptr,&g_cb);
    D3D11_TEXTURE2D_DESC td={};td.MipLevels=1;td.ArraySize=1;td.SampleDesc.Count=1;
    td.Usage=D3D11_USAGE_DYNAMIC;td.BindFlags=D3D11_BIND_SHADER_RESOURCE;td.CPUAccessFlags=D3D11_CPU_ACCESS_WRITE;
    td.Width=VCAM_W;td.Height=VCAM_H;td.Format=DXGI_FORMAT_R8_UNORM;
    g_dev->CreateTexture2D(&td,nullptr,&g_texY);
    D3D11_SHADER_RESOURCE_VIEW_DESC sv={DXGI_FORMAT_R8_UNORM,D3D11_SRV_DIMENSION_TEXTURE2D,{}};sv.Texture2D.MipLevels=1;
    g_dev->CreateShaderResourceView(g_texY,&sv,&g_srvY);
    td.Width=VCAM_W/2;td.Height=VCAM_H/2;td.Format=DXGI_FORMAT_R8G8_UNORM;
    g_dev->CreateTexture2D(&td,nullptr,&g_texUV);sv.Format=DXGI_FORMAT_R8G8_UNORM;
    g_dev->CreateShaderResourceView(g_texUV,&sv,&g_srvUV);
    return true;
}
static void UploadFrame(const uint8_t* nv12){
    D3D11_MAPPED_SUBRESOURCE ms;
    if(SUCCEEDED(g_ctx->Map(g_texY,0,D3D11_MAP_WRITE_DISCARD,0,&ms))){
        const uint8_t* s=nv12;uint8_t* d=(uint8_t*)ms.pData;
        for(int y=0;y<VCAM_H;y++,d+=ms.RowPitch,s+=VCAM_W)memcpy(d,s,VCAM_W);
        g_ctx->Unmap(g_texY,0);}
    if(SUCCEEDED(g_ctx->Map(g_texUV,0,D3D11_MAP_WRITE_DISCARD,0,&ms))){
        const uint8_t* s=nv12+VCAM_W*VCAM_H;uint8_t* d=(uint8_t*)ms.pData;
        for(int y=0;y<VCAM_H/2;y++,d+=ms.RowPitch,s+=VCAM_W)memcpy(d,s,VCAM_W);
        g_ctx->Unmap(g_texUV,0);}
}

// ── OSD fonts ─────────────────────────────────────────────────────────────────
static HFONT g_f_hero  = nullptr;  // 32pt bold — headline numbers
static HFONT g_f_label = nullptr;  // 13pt bold — field labels
static HFONT g_f_val   = nullptr;  // 16pt bold — secondary values
static HFONT g_f_small = nullptr;  // 11pt — graph labels / sid
static void InitFonts(){
    g_f_hero  = CreateFontW(-32,0,0,0,FW_BOLD,0,0,0,DEFAULT_CHARSET,0,0,CLEARTYPE_QUALITY,FIXED_PITCH,L"Consolas");
    g_f_label = CreateFontW(-13,0,0,0,FW_BOLD,0,0,0,DEFAULT_CHARSET,0,0,CLEARTYPE_QUALITY,FIXED_PITCH,L"Consolas");
    g_f_val   = CreateFontW(-16,0,0,0,FW_BOLD,0,0,0,DEFAULT_CHARSET,0,0,CLEARTYPE_QUALITY,FIXED_PITCH,L"Consolas");
    g_f_small = CreateFontW(-11,0,0,0,FW_NORMAL,0,0,0,DEFAULT_CHARSET,0,0,CLEARTYPE_QUALITY,FIXED_PITCH,L"Consolas");
}

// ── OSD drawing primitives ────────────────────────────────────────────────────

// YouTube-style panel: RGB(50,59,67) solid fill via AlphaBlend
static void Panel(HDC hdc,int x,int y,int w,int h,BYTE alpha=230){
    HDC mem=CreateCompatibleDC(hdc);
    HBITMAP bmp=CreateCompatibleBitmap(hdc,w,h);
    HBITMAP old=(HBITMAP)SelectObject(mem,bmp);
    HBRUSH br=CreateSolidBrush(RGB(50,59,67));
    RECT r={0,0,w,h}; FillRect(mem,&r,br); DeleteObject(br);
    BLENDFUNCTION bf={AC_SRC_OVER,0,alpha,0};
    AlphaBlend(hdc,x,y,w,h,mem,0,0,w,h,bf);
    SelectObject(mem,old);DeleteObject(bmp);DeleteDC(mem);
}

// White text, no shadow needed on dark panel
static void Txt(HDC hdc,int x,int y,const wchar_t* text,COLORREF fg,HFONT fnt){
    SelectObject(hdc,fnt); SetBkMode(hdc,TRANSPARENT);
    SetTextColor(hdc,fg); TextOutW(hdc,x,y,text,(int)wcslen(text));
}

// Colored value helper
static COLORREF RampColor(float v,float lo,float hi){
    // lo=green hi=red
    if(v<=lo) return RGB(80,230,80);
    if(v>=hi) return RGB(255,70,50);
    float t=(v-lo)/(hi-lo);
    return RGB((BYTE)(80+t*175),(BYTE)(230-t*160),50);
}
static COLORREF FpsColor(float fps){
    if(fps>=230) return RGB(80,230,80);
    if(fps>=180) return RGB(255,200,50);
    return RGB(255,70,50);
}

// Draw sparkline (no background — caller provides bg rect separately)
static void Spark(HDC hdc,int x,int y,int w,int h,
                  const std::deque<float>& dq,float maxv,COLORREF lc,float warn=0){
    if(maxv<=0)maxv=1;
    if(warn>0&&warn<maxv){
        int wy=y+h-(int)(warn/maxv*h);
        HPEN wp=CreatePen(PS_DOT,1,RGB(220,180,40));
        HPEN op=(HPEN)SelectObject(hdc,wp);
        MoveToEx(hdc,x,wy,nullptr);LineTo(hdc,x+w,wy);
        SelectObject(hdc,op);DeleteObject(wp);
    }
    int n=(int)dq.size();if(n<2)return;
    int start=n>w?n-w:0;
    bool first=true;
    HPEN lp=CreatePen(PS_SOLID,2,lc),op=(HPEN)SelectObject(hdc,lp);
    for(int i=start;i<n;i++){
        float v=dq[i];if(v<0)v=0;if(v>maxv)v=maxv;
        int px=x+(i-start),py=y+h-(int)(v/maxv*h);
        if(py<y)py=y;if(py>=y+h)py=y+h-1;
        COLORREF c=(warn>0&&dq[i]>warn)?RGB(255,80,50):lc;
        SelectObject(hdc,op);DeleteObject(lp);
        lp=CreatePen(PS_SOLID,2,c);op=(HPEN)SelectObject(hdc,lp);
        if(first){MoveToEx(hdc,px,py,nullptr);first=false;}else LineTo(hdc,px,py);
    }
    SelectObject(hdc,op);DeleteObject(lp);
}

// ── DrawOSD ───────────────────────────────────────────────────────────────────
// DrawOSD — takes pre-acquired HDC (DXGI surface DC)
static void DrawOSD(HDC hdc, const Stats& s, int mode){

    bool live=s.rx_run&&g_has_signal.load();
    bool err =s.mismatch>0||s.dropped>0;
    float fp =s.pc1_p99+s.rseg_p99;

    // ── MINI: 2-line top bar ─────────────────────────────────────────────────
    if(mode==1){
        Panel(hdc,0,0,WIN_W,52,190);
        wchar_t buf[128];
        // Line 1: FPS  p99  status
        swprintf_s(buf,L"%.0f fps   p99 %.1f ms   %s",
                   s.fps, fp, live?L"\u25cf LIVE":L"\u25cb IDLE");
        Txt(hdc,10,6,buf,FpsColor(s.fps),g_f_val);
        // Line 2: detail
        swprintf_s(buf,L"rseg %.1f ms   pc1 %.1f ms   ok %d   drop %d   miss %d",
                   s.rseg_p99,s.pc1_p99,s.complete,s.dropped,s.mismatch);
        Txt(hdc,10,28,buf,RGB(200,210,230),g_f_label);
        return;
    }

    // ── FULL OSD ─────────────────────────────────────────────────────────────
    // Layout (640×640):
    //   0..55   hero bar: FPS | p99 | status dot
    //  58..240  detail grid: Receiver (left) / Sender (right)
    // 245..390  4 sparkline graphs side-by-side
    // (video shows through everywhere via alpha)

    // ── HERO BAR (top) ───────────────────────────────────────────────────────
    // Single panel covering all three sections: hero + detail + graphs
    // hero: 0-58, detail: 58-240, graphs: 240-408  => total 408px
    Panel(hdc,0,0,WIN_W,408,220);

    // FPS (left, big)
    wchar_t buf[128];
    swprintf_s(buf,L"%.0f fps",s.fps);
    Txt(hdc,8,4,buf,FpsColor(s.fps),g_f_hero);

    // full-path p99 (center)
    COLORREF fp_c=RampColor(fp,8,15);
    swprintf_s(buf,L"p99 %.1f ms",fp);
    Txt(hdc,240,4,buf,fp_c,g_f_hero);

    // status dot + rfps (right)
    COLORREF st_c=err?RGB(255,70,50):(live?RGB(80,230,80):RGB(255,200,50));
    swprintf_s(buf,L"%s  rfps %.0f",live?L"\u25cf LIVE":L"\u25cb",s.rfps);
    Txt(hdc,490,4,buf,st_c,g_f_val);
    swprintf_s(buf,L"uptime %.0fs",s.uptime_rx);
    Txt(hdc,490,28,buf,RGB(160,170,190),g_f_label);

    // ── DETAIL GRID ──────────────────────────────────────────────────────────

    const int LX=8, RX=326, DY=17;
    int ly=64, ry=64;

    // helper lambdas
    auto row=[&](int& cy,int col,const wchar_t* label,const wchar_t* val,COLORREF vc){
        Txt(hdc,col,     cy,label,RGB(160,170,200),g_f_label);
        Txt(hdc,col+150, cy,val,  vc,               g_f_val);
        cy+=DY;
    };
    auto fmt=[](wchar_t* b,const wchar_t* fmt,...){
        va_list v; va_start(v,fmt); vswprintf_s(b,64,fmt,v); va_end(v);
    };
    wchar_t vb[64];

    // section header
    Txt(hdc,LX,ly,L"Receiver",RGB(120,170,255),g_f_val); ly+=DY+2;
    fmt(vb,L"%.1f / %.1f / %.1f ms",s.rseg_avg,s.rseg_p50,s.rseg_p99);
    row(ly,LX,L"rseg avg/p50/p99",vb,RampColor(s.rseg_p99,6,12));
    fmt(vb,L"%.1f ms",s.rseg_max);
    row(ly,LX,L"rseg max",vb,RampColor(s.rseg_max,30,200));
    fmt(vb,L"%.2f / %.2f ms",s.ia_avg,s.ia_p99);
    row(ly,LX,L"inter-arr avg/p99",vb,RampColor(s.ia_p99,6,12));
    fmt(vb,L"%d",s.complete);
    row(ly,LX,L"frames ok",vb,RGB(180,240,180));
    COLORREF dc=(s.dropped||s.mismatch)?RGB(255,70,50):RGB(80,230,80);
    fmt(vb,L"%d / %d",s.dropped,s.mismatch);
    row(ly,LX,L"drop / mismatch",vb,dc);
    fmt(vb,L"%d / %d",s.gaps,s.pkt_lost);
    row(ly,LX,L"gaps / pkt_lost",vb,(s.gaps||s.pkt_lost)?RGB(255,160,50):RGB(80,230,80));
    fmt(vb,L"%.3f Gbps  %.0f pps",s.bps/1e9f,s.pps);
    row(ly,LX,L"bandwidth",vb,RGB(180,200,230));
    fmt(vb,L"%.8hs",s.rx_sid);
    row(ly,LX,L"session",vb,RGB(120,130,150));

    Txt(hdc,RX,ry,L"Sender",RGB(120,170,255),g_f_val); ry+=DY+2;
    fmt(vb,L"%.1f fps   cpu %d%%",s.fps,s.cpu_pct);
    row(ry,RX,L"fps / cpu",vb,FpsColor(s.fps));
    fmt(vb,L"%.2f / %.2f / %.2f ms",s.pc1_avg,s.pc1_p99,s.pc1_max);
    row(ry,RX,L"pc1 avg/p99/max",vb,RampColor(s.pc1_p99,4,8));
    fmt(vb,L"%.3f ms",s.gpu_dispatch);
    row(ry,RX,L"gpu_dispatch",vb,RampColor(s.gpu_dispatch,0.5f,2));
    fmt(vb,L"%.2f ms",s.gpu_ms);
    row(ry,RX,L"gpu_ms",vb,RGB(200,200,200));
    fmt(vb,L"%s",s.gpu_convert?L"GPU":L"CPU");
    row(ry,RX,L"convert",vb,s.gpu_convert?RGB(80,230,80):RGB(200,200,200));
    fmt(vb,L"%d  uptime %.0fs",s.send_err,s.uptime_tx);
    row(ry,RX,L"send_err / up",vb,s.send_err?RGB(255,70,50):RGB(80,230,80));
    fmt(vb,L"%.8hs",s.tx_sid);
    row(ry,RX,L"session",vb,RGB(120,130,150));

    // ── GRAPHS: 2 rows x 2 cols, white lines, shared bg ─────────────────────
    // divider
    {HPEN p=CreatePen(PS_SOLID,1,RGB(90,100,110));HPEN op=(HPEN)SelectObject(hdc,p);
     MoveToEx(hdc,0,240,nullptr);LineTo(hdc,WIN_W,240);SelectObject(hdc,op);DeleteObject(p);}

    std::deque<float> h_fps,h_rseg,h_pc1,h_full;
    { std::lock_guard<std::mutex> lk(g_smtx); h_fps=g_h_fps;h_rseg=g_h_rseg;h_pc1=g_h_pc1;h_full=g_h_full; }

    struct GDef { const wchar_t* label; std::deque<float>* h; float maxv; float warn; };
    GDef graphs[4]={
        {L"FPS (0-280)",        &h_fps,  280.f, 200.f},
        {L"rseg p99 ms (0-30)", &h_rseg,  30.f,  10.f},
        {L"pc1 p99 ms (0-10)",  &h_pc1,   10.f,   5.f},
        {L"full p99 ms (0-20)", &h_full,  20.f,  10.f},
    };
    const int GW2=(WIN_W/2)-8, GH2=72, GCOL=WIN_W/2;
    const int GY0=244, GROW=GH2+20;
    for(int i=0;i<4;i++){
        int col=i%2, row2=i/2;
        int gx=4+col*GCOL, gy=GY0+row2*GROW;
        Txt(hdc,gx,gy,graphs[i].label,RGB(210,215,225),g_f_small);
        Spark(hdc,gx,gy+12,GW2,GH2,*graphs[i].h,graphs[i].maxv,RGB(255,255,255),graphs[i].warn);
        if(!graphs[i].h->empty()){
            float v=graphs[i].h->back();
            wchar_t lbl[24];
            if(i==0) swprintf_s(lbl,L"%.0f",v);
            else     swprintf_s(lbl,L"%.2f",v);
            COLORREF vc=(graphs[i].warn>0&&v>graphs[i].warn)?RGB(255,130,60):RGB(255,255,255);
            Txt(hdc,gx+2,gy+12+GH2-14,lbl,vc,g_f_small);
        }
    }

    // (no ReleaseDC here — caller owns hdc)
}

// ── Render ────────────────────────────────────────────────────────────────────
static HWND g_hwnd=nullptr;
static void Render(bool valid){
    static LARGE_INTEGER freq={},prev={};
    static int cnt=0;static float acc=0.f;
    if(!freq.QuadPart)QueryPerformanceFrequency(&freq);
    LARGE_INTEGER now;QueryPerformanceCounter(&now);
    if(prev.QuadPart){
        float dt=(float)(now.QuadPart-prev.QuadPart)/(float)freq.QuadPart;
        acc+=dt;cnt++;if(acc>=0.5f){g_rfps.store(cnt/acc);acc=0;cnt=0;}
    }
    prev=now;
    float clear[4]={0.04f,0.04f,0.05f,1};
    D3D11_VIEWPORT vp={0,0,(float)WIN_W,(float)WIN_H,0,1};
    g_ctx->RSSetViewports(1,&vp);
    g_ctx->OMSetRenderTargets(1,&g_rtv,nullptr);
    g_ctx->ClearRenderTargetView(g_rtv,clear);
    Stats s;{std::lock_guard<std::mutex> lk(g_smtx);s=g_stats;}
    bool live=s.rx_run&&valid,err=s.mismatch>0||s.dropped>0;
    D3D11_MAPPED_SUBRESOURCE ms;
    if(SUCCEEDED(g_ctx->Map(g_cb,0,D3D11_MAP_WRITE_DISCARD,0,&ms))){
        CBData* cb=(CBData*)ms.pData;
        if(!g_pView){cb->sig[0]=0.4f;cb->sig[1]=0.4f;cb->sig[2]=0.4f;cb->sig[3]=1;}
        else if(err){cb->sig[0]=1.f;cb->sig[1]=0.1f;cb->sig[2]=0.f;cb->sig[3]=1;}
        else if(live){cb->sig[0]=0.f;cb->sig[1]=1.f;cb->sig[2]=0.1f;cb->sig[3]=1;}
        else{cb->sig[0]=1.f;cb->sig[1]=0.8f;cb->sig[2]=0.f;cb->sig[3]=1;}
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

    // OSD: draw on DXGI back-buffer surface DC BEFORE Present
    int mode=g_osd_mode.load();
    if(mode>0){
        IDXGISurface1* pSurf=nullptr;
        if(SUCCEEDED(g_sc->GetBuffer(0,__uuidof(IDXGISurface1),(void**)&pSurf))){
            HDC hdc=nullptr;
            if(SUCCEEDED(pSurf->GetDC(FALSE,&hdc))){
                Stats s2;{std::lock_guard<std::mutex> lk(g_smtx);s2=g_stats;}
                DrawOSD(hdc,s2,mode);
                pSurf->ReleaseDC(nullptr);
            }
            pSurf->Release();
        }
    }

    g_sc->Present(0,0);
    // (no post-Present GDI needed)
}

// ── WndProc ───────────────────────────────────────────────────────────────────
static LRESULT CALLBACK WndProc(HWND hw,UINT msg,WPARAM wp,LPARAM lp){
    switch(msg){
    case WM_DESTROY: PostQuitMessage(0); return 0;
    case WM_KEYDOWN:
        if(wp==VK_ESCAPE){PostQuitMessage(0);return 0;}
        if(wp=='O'){g_osd_mode.store((g_osd_mode.load()+1)%3);return 0;}
        break;
    case WM_RBUTTONDOWN:{
        POINT pt={GET_X_LPARAM(lp),GET_Y_LPARAM(lp)};
        ClientToScreen(hw,&pt);
        int mode=g_osd_mode.load();
        HMENU hm=CreatePopupMenu();
        AppendMenuW(hm,MF_STRING|(mode==2?MF_CHECKED:0),IDM_OSD_FULL,L"Full OSD");
        AppendMenuW(hm,MF_STRING|(mode==1?MF_CHECKED:0),IDM_OSD_MINI,L"Mini OSD");
        AppendMenuW(hm,MF_STRING|(mode==0?MF_CHECKED:0),IDM_OSD_OFF, L"OSD Off");
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
    case WM_TIMER: return 0;
    }
    return DefWindowProcW(hw,msg,wp,lp);
}

// ── main ──────────────────────────────────────────────────────────────────────
int WINAPI WinMain(HINSTANCE hi,HINSTANCE,LPSTR,int){
    wchar_t exe[MAX_PATH];GetModuleFileNameW(nullptr,exe,MAX_PATH);
    {wchar_t* p=wcsrchr(exe,L'\\');if(p)*p=0;}
    bool found=false;
    for(int up=0;up<4&&!found;up++){
        swprintf_s(g_run_dir,L"%s\\run",exe);
        if(GetFileAttributesW(g_run_dir)!=INVALID_FILE_ATTRIBUTES){found=true;break;}
        wchar_t* p=wcsrchr(exe,L'\\');if(p)*p=0;else break;
    }
    if(!found)swprintf_s(g_run_dir,L"%s\\run",exe);
    g_hMap=OpenFileMappingA(FILE_MAP_READ,FALSE,SHM_NAME);
    if(g_hMap)g_pView=MapViewOfFile(g_hMap,FILE_MAP_READ,0,0,HDR_SIZE+FRAME_BYTES);
    WNDCLASSEXW wc={sizeof(wc)};
    wc.style=CS_HREDRAW|CS_VREDRAW;wc.lpfnWndProc=WndProc;wc.hInstance=hi;
    wc.hCursor=LoadCursorW(nullptr,MAKEINTRESOURCEW(32512));wc.lpszClassName=L"VCMonitor";
    if(!RegisterClassExW(&wc)){wchar_t e[128];swprintf_s(e,L"RegisterClassExW: %lu",GetLastError());MessageBoxW(nullptr,e,L"Error",MB_OK);return 1;}
    RECT r={0,0,WIN_W,WIN_H};AdjustWindowRect(&r,WS_OVERLAPPEDWINDOW,FALSE);
    g_hwnd=CreateWindowExW(0,L"VCMonitor",L"vc-monitor",WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,CW_USEDEFAULT,r.right-r.left,r.bottom-r.top,nullptr,nullptr,hi,nullptr);
    if(!g_hwnd){wchar_t e[128];swprintf_s(e,L"CreateWindowExW: %lu",GetLastError());MessageBoxW(nullptr,e,L"Error",MB_OK);return 1;}
    if(!InitD3D(g_hwnd)){MessageBoxW(g_hwnd,L"D3D11 init failed.",L"Error",MB_OK);return 1;}
    InitFonts();
    {static uint8_t grey[FRAME_BYTES];memset(grey,128,FRAME_BYTES);UploadFrame(grey);}
    ShowWindow(g_hwnd,SW_SHOW);UpdateWindow(g_hwnd);
    SetTimer(g_hwnd,1,500,nullptr);
    std::thread st(StatsThread);st.detach();
    MSG msg={};
    while(true){
        while(PeekMessageW(&msg,nullptr,0,0,PM_REMOVE)){
            TranslateMessage(&msg);DispatchMessageW(&msg);
            if(msg.message==WM_QUIT)goto done;
        }
        auto fi=ReadSHMFrame();
        if(fi.valid&&fi.fid!=g_last_fid.load()){
            g_last_fid.store(fi.fid);g_has_signal.store(true);UploadFrame(g_frame_buf);
        }
        Render(fi.valid&&g_has_signal.load());
        Sleep(1);
    }
done:
    if(g_pView)UnmapViewOfFile(g_pView);
    if(g_hMap)CloseHandle(g_hMap);
    if(g_f_hero) DeleteObject(g_f_hero);
    if(g_f_label)DeleteObject(g_f_label);
    if(g_f_val)  DeleteObject(g_f_val);
    if(g_f_small)DeleteObject(g_f_small);
    return 0;
}
