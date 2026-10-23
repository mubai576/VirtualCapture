#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <d3dcompiler.h>
#include <cstdio>
#pragma comment(lib,"d3d11.lib")
#pragma comment(lib,"d3dcompiler.lib")
#pragma comment(lib,"user32.lib")

static const char* kPS = R"(
float4 PS(float4 p:SV_Position,float2 uv:TEXCOORD):SV_Target{return float4(uv,0,1);}
void VS(uint id:SV_VertexID,out float4 p:SV_Position,out float2 uv:TEXCOORD){
    uv=float2((id&1)?1:0,(id>>1)?1:0);p=float4(uv.x*2-1,1-uv.y*2,0,1);}
)";

static LRESULT CALLBACK WP(HWND h,UINT m,WPARAM w,LPARAM l){
    if(m==WM_DESTROY){PostQuitMessage(0);return 0;}
    return DefWindowProcW(h,m,w,l);
}
int main(){
    printf("=== vc-monitor diag ===\n");
    HINSTANCE hi=GetModuleHandleW(nullptr);
    WNDCLASSEXW wc={sizeof(wc)};
    wc.lpfnWndProc=WP; wc.hInstance=hi;
    wc.hCursor=LoadCursorW(nullptr,MAKEINTRESOURCEW(32512));
    wc.lpszClassName=L"VCDiag";
    ATOM a=RegisterClassExW(&wc);
    printf("RegisterClassEx: atom=%d err=%lu\n",a,a?0:GetLastError());
    if(!a) return 1;

    HWND hw=CreateWindowExW(0,L"VCDiag",L"Diag",WS_OVERLAPPEDWINDOW,
        200,200,700,700,nullptr,nullptr,hi,nullptr);
    printf("CreateWindow: hwnd=%p err=%lu\n",(void*)hw,hw?0:GetLastError());
    if(!hw) return 1;
    ShowWindow(hw,SW_SHOW); UpdateWindow(hw);

    DXGI_SWAP_CHAIN_DESC sd={};
    sd.BufferCount=1; sd.BufferDesc.Width=700; sd.BufferDesc.Height=700;
    sd.BufferDesc.Format=DXGI_FORMAT_R8G8B8A8_UNORM;
    sd.BufferDesc.RefreshRate={0,1};
    sd.BufferUsage=DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.OutputWindow=hw; sd.SampleDesc.Count=1; sd.Windowed=TRUE;
    ID3D11Device* dev=nullptr; ID3D11DeviceContext* ctx=nullptr;
    IDXGISwapChain* sc=nullptr; D3D_FEATURE_LEVEL fl;
    HRESULT hr=D3D11CreateDeviceAndSwapChain(nullptr,D3D_DRIVER_TYPE_HARDWARE,
        nullptr,0,nullptr,0,D3D11_SDK_VERSION,&sd,&sc,&dev,&fl,&ctx);
    printf("D3D11 hr=0x%08X fl=%d\n",(unsigned)hr,(int)fl);
    if(FAILED(hr)) { printf("FAIL D3D\n"); Sleep(4000); return 1; }

    ID3DBlob *blob=nullptr,*err2=nullptr;
    hr=D3DCompile(kPS,strlen(kPS),nullptr,nullptr,nullptr,"PS","ps_5_0",0,0,&blob,&err2);
    printf("D3DCompile PS hr=0x%08X\n",(unsigned)hr);
    if(err2) printf("  err: %s\n",(char*)err2->GetBufferPointer());
    printf("all ok, window visible for 5s\n");

    MSG msg; DWORD t0=GetTickCount();
    while(GetTickCount()-t0 < 5000){
        while(PeekMessageW(&msg,nullptr,0,0,PM_REMOVE)){
            TranslateMessage(&msg); DispatchMessageW(&msg);
            if(msg.message==WM_QUIT) goto done;
        }
        if(sc) sc->Present(0,0);
        Sleep(16);
    }
done:
    printf("done\n");
    return 0;
}
