// Direct DLL lifecycle tests: no registration, resident agent or device changes.
#include <windows.h>
#include <dshow.h>
#include <psapi.h>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
static const CLSID Camera={0xb84c7e21,0x4a5f,0x4e2b,{0x9c,0x11,0x5a,0x3d,0x7f,0x01,0x22,0x9a}};
// Null Renderer is installed by DirectShow; its old qedit.h declaration is no longer in this SDK.
static const CLSID NullRenderer={0xc1f400a4,0x3f08,0x11d3,{0x9f,0x0b,0x00,0x60,0x08,0x03,0x9e,0x37}};
#define CHECK(x) do {if(!(x)){fprintf(stderr,"FAIL line %d: %s\n",__LINE__,#x);return 1;}}while(0)
static SIZE_T private_bytes(){
  PROCESS_MEMORY_COUNTERS_EX p={};p.cb=sizeof(p);
  GetProcessMemoryInfo(GetCurrentProcess(),(PROCESS_MEMORY_COUNTERS*)&p,sizeof(p));return p.PrivateUsage;
}
int main(int argc,char** argv){
  if(argc!=2){fprintf(stderr,"usage: vc-vcam-lifecycle <candidate DLL>\n");return 2;}
  CHECK(SUCCEEDED(CoInitializeEx(nullptr,COINIT_MULTITHREADED)));
  HMODULE module=LoadLibraryW(std::filesystem::absolute(argv[1]).wstring().c_str()); CHECK(module);
  auto get=(HRESULT (STDAPICALLTYPE*)(REFCLSID,REFIID,void**))GetProcAddress(module,"DllGetClassObject");
  auto unload=(HRESULT (STDAPICALLTYPE*)())GetProcAddress(module,"DllCanUnloadNow");
  CHECK(get && unload && unload()==S_OK);
  IClassFactory* factory=nullptr;CHECK(get(Camera,IID_IClassFactory,(void**)&factory)==S_OK);
  CHECK(unload()==S_FALSE); // An outstanding factory also keeps the DLL loaded.
  auto create=[&](){IBaseFilter* f=nullptr;factory->CreateInstance(nullptr,IID_IBaseFilter,(void**)&f);return f;};
  for(int i=0;i<32;i++){auto* f=create();CHECK(f);CHECK(f->Release()==0);}
  SIZE_T before=private_bytes();
  for(int i=0;i<1000;i++){auto* f=create();CHECK(f);CHECK(f->Release()==0);}
  SIZE_T after=private_bytes();
  CHECK(after<=before+8*1024*1024);
  printf("PASS 1000 create/release: private_delta=%lld bytes\n",(long long)after-(long long)before);
  IBaseFilter* f=create();CHECK(f);IPin* pin=nullptr;CHECK(f->FindPin(L"Capture",&pin)==S_OK);
  IEnumPins* pins=nullptr;CHECK(f->EnumPins(&pins)==S_OK);
  CHECK(f->Release()>0); // Only external pins/enumerator retain the filter.
  PIN_INFO info={};CHECK(pin->QueryPinInfo(&info)==S_OK && info.pFilter);info.pFilter->Release();
  IEnumPins* clone=nullptr;CHECK(pins->Clone(&clone)==S_OK);pins->Release();clone->Release();
  IEnumMediaTypes* types=nullptr;CHECK(pin->EnumMediaTypes(&types)==S_OK);
  CHECK(pin->Release()==0);
  CHECK(factory->Release()==0);factory=nullptr;CHECK(unload()==S_FALSE); // Detached media enumerator.
  AM_MEDIA_TYPE* mt=nullptr;CHECK(types->Next(1,&mt,nullptr)==S_OK && mt);
  if(mt->pbFormat)CoTaskMemFree(mt->pbFormat);if(mt->pUnk)mt->pUnk->Release();CoTaskMemFree(mt);
  types->Release();CHECK(unload()==S_OK);
  puts("PASS external pin, pin enumerator, media enumerator, factory lifetime");
  CHECK(get(Camera,IID_IClassFactory,(void**)&factory)==S_OK);
  for(int i=0;i<50;i++){
    f=create();CHECK(f);IGraphBuilder* graph=nullptr;
    CHECK(SUCCEEDED(CoCreateInstance(CLSID_FilterGraph,nullptr,CLSCTX_INPROC_SERVER,IID_IGraphBuilder,(void**)&graph)));
    CHECK(SUCCEEDED(graph->AddFilter(f,L"Lifecycle test")));
    IBaseFilter* sink=nullptr;
    CHECK(SUCCEEDED(CoCreateInstance(NullRenderer,nullptr,CLSCTX_INPROC_SERVER,IID_IBaseFilter,(void**)&sink)));
    CHECK(SUCCEEDED(graph->AddFilter(sink,L"System null renderer")));
    IEnumPins* input_pins=nullptr;IPin* input=nullptr;IPin* output=nullptr;
    CHECK(sink->EnumPins(&input_pins)==S_OK && input_pins->Next(1,&input,nullptr)==S_OK);
    input_pins->Release();CHECK(f->FindPin(L"Capture",&output)==S_OK);
    CHECK(SUCCEEDED(graph->ConnectDirect(output,input,nullptr)));
    input->Release();output->Release();sink->Release();
    FILTER_INFO fi={};CHECK(f->QueryFilterInfo(&fi)==S_OK && fi.pGraph);fi.pGraph->Release();
    graph->Release(); // Graph destruction must detach its still externally-held filter.
    CHECK(f->QueryFilterInfo(&fi)==S_OK && !fi.pGraph);CHECK(f->Release()==0);
  }
  factory->Release();CHECK(unload()==S_OK);puts("PASS 50 connected graph teardown cycles; DllCanUnloadNow=S_OK");
  FreeLibrary(module);CoUninitialize();return 0;
}
