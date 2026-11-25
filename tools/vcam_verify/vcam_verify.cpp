// vcam_verify.cpp — P4-4 J1+J2 DShow 验收：枚举 + IAMStreamConfig 格式检查
// 编译：cmake -S . -B build && cmake --build build --config Release
// 用法：vc-vcam-verify.exe
// 退出码：0=PASS  1=FAIL
#include <windows.h>
#include <dshow.h>
#include <strmif.h>
#include <ks.h>
#include <ksmedia.h>
#include <string>
#include <cstdio>
#include <atomic>
#include <fstream>
#include <filesystem>
#include <vector>
#include <mutex>
#include <chrono>
#include "../../common/fnv1a.h"
#include "../../common/stream.h"
#include "../../common/mjpeg_codec.h"
#pragma comment(lib,"strmiids.lib")
#pragma comment(lib,"ole32.lib")
#pragma comment(lib,"oleaut32.lib")

// NV12 subtype GUID: {3231564E-0000-0010-8000-00AA00389B71}
static const GUID MEDIASUBTYPE_NV12_EXPECTED =
    {0x3231564E,0x0000,0x0010,{0x80,0x00,0x00,0xAA,0x00,0x38,0x9B,0x71}};

static void FreeMediaType(AM_MEDIA_TYPE* pmt) {
    if (!pmt) return;
    if (pmt->cbFormat && pmt->pbFormat) CoTaskMemFree(pmt->pbFormat);
    if (pmt->pUnk) pmt->pUnk->Release();
    CoTaskMemFree(pmt);
}

static int enumerate(int width,int height,int fps) {
    printf("P4-4 J1+J2: DShow vcam enumerate + format check\n");
    printf("================================================\n");

    CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);

    ICreateDevEnum* pSDE = nullptr;
    HRESULT hr = CoCreateInstance(CLSID_SystemDeviceEnum, nullptr, CLSCTX_INPROC_SERVER,
                                   IID_ICreateDevEnum, (void**)&pSDE);
    if (FAILED(hr)) { printf("FAIL: CoCreate SystemDevEnum hr=0x%08X\n", (unsigned)hr); return 1; }

    IEnumMoniker* pEnum = nullptr;
    hr = pSDE->CreateClassEnumerator(CLSID_VideoInputDeviceCategory, &pEnum, 0);
    pSDE->Release();
    if (FAILED(hr) || !pEnum) { printf("FAIL: CreateClassEnumerator hr=0x%08X\n", (unsigned)hr); return 1; }

    bool j1_pass = false, j2_pass = false;
    IMoniker* pMon = nullptr;
    while (pEnum->Next(1, &pMon, nullptr) == S_OK) {
        // Get FriendlyName via IPropertyBag
        IPropertyBag* pBag = nullptr;
        hr = pMon->BindToStorage(nullptr, nullptr, IID_IPropertyBag, (void**)&pBag);
        std::wstring name;
        if (SUCCEEDED(hr) && pBag) {
            VARIANT var; VariantInit(&var);
            if (SUCCEEDED(pBag->Read(L"FriendlyName", &var, nullptr)) && var.vt == VT_BSTR && var.bstrVal)
                name = var.bstrVal;
            VariantClear(&var);
            pBag->Release();
        }
        // narrow for printf
        std::string narrow(name.begin(), name.end());
        printf("  device: %s\n", narrow.c_str());

        if (name.find(L"VirtualCapture") != std::wstring::npos) {
            printf("  J1 PASS: found 'VirtualCapture'\n");
            j1_pass = true;

            // Bind to filter
            IBaseFilter* pFilter = nullptr;
            hr = pMon->BindToObject(nullptr, nullptr, IID_IBaseFilter, (void**)&pFilter);
            if (FAILED(hr) || !pFilter) {
                printf("  FAIL BindToObject hr=0x%08X\n", (unsigned)hr);
                pMon->Release(); continue;
            }

            // Enumerate pins, find output pin with IAMStreamConfig
            IEnumPins* pEnumPins = nullptr;
            pFilter->EnumPins(&pEnumPins);
            if (pEnumPins) {
                IPin* pPin = nullptr;
                while (pEnumPins->Next(1, &pPin, nullptr) == S_OK) {
                    PIN_DIRECTION dir;
                    if (SUCCEEDED(pPin->QueryDirection(&dir)) && dir == PINDIR_OUTPUT) {
                        IAMStreamConfig* pSC = nullptr;
                        if (SUCCEEDED(pPin->QueryInterface(IID_IAMStreamConfig, (void**)&pSC)) && pSC) {
                            AM_MEDIA_TYPE* pmt = nullptr;
                            hr = pSC->GetFormat(&pmt);
                            if (SUCCEEDED(hr) && pmt && pmt->pbFormat) {
                                // Parse VIDEOINFOHEADER
                                VIDEOINFOHEADER* vih = (VIDEOINFOHEADER*)pmt->pbFormat;
                                BITMAPINFOHEADER& bih = vih->bmiHeader;
                                REFERENCE_TIME avg = vih->AvgTimePerFrame;
                                DWORD w = (DWORD)abs(bih.biWidth);
                                DWORD h2 = (DWORD)abs(bih.biHeight);
                                DWORD fourcc = bih.biCompression;
                                char fcc[5] = {(char)(fourcc),(char)(fourcc>>8),(char)(fourcc>>16),(char)(fourcc>>24),0};
                                bool subtype_nv12 = (pmt->subtype.Data1 == MEDIASUBTYPE_NV12_EXPECTED.Data1);
                                printf("  IAMStreamConfig GetFormat:\n");
                                printf("    biWidth=%u biHeight=%u fourcc=%s subtype.Data1=0x%08X\n",
                                       w, h2, fcc, (unsigned)pmt->subtype.Data1);
                                printf("    AvgTimePerFrame=%lld (%.1f fps)\n",
                                       (long long)avg, avg ? 1e7/(double)avg : 0.0);
                                bool ok = (w == (DWORD)width && h2 == (DWORD)height &&
                                           (fourcc == 0x3231564EU || subtype_nv12) &&
                                           avg == 10000000LL/fps);
                                if (ok) { printf("  J2 PASS: NV12 %dx%d AvgTimePerFrame=%lld\n",width,height,10000000LL/fps); j2_pass = true; }
                                else      printf("  J2 FAIL: format mismatch\n");
                                FreeMediaType(pmt);
                            } else {
                                printf("  FAIL GetFormat hr=0x%08X\n", (unsigned)hr);
                            }
                            pSC->Release();
                        }
                    }
                    pPin->Release();
                    if (j2_pass) break;
                }
                pEnumPins->Release();
            }
            pFilter->Release();
        }
        pMon->Release();
        if (j1_pass) break;
    }
    pEnum->Release();
    CoUninitialize();

    printf("\n================================================\n");
    printf("J1 Enumerate: %s\n", j1_pass ? "PASS" : "FAIL");
    printf("J2 Format:    %s\n", j2_pass ? "PASS" : "FAIL");
    bool all_pass = j1_pass && j2_pass;
    printf("\nResult: %s\n", all_pass ? "PASS" : "FAIL");
    return all_pass ? 0 : 1;
}


// A real allocator/IMediaSample sink, loaded in-process without COM registration or a graph UI.
class VerifySink : public IPin, public IMemInputPin {
  LONG refs=1;
  VCStream stream;
  std::ofstream hashes;
  HANDLE flushed=CreateEvent(nullptr,TRUE,FALSE,nullptr);
  bool block_receive=false;
  bool hold_samples=false;
  bool jpeg=false;
  bool checkFlat=false;
  vcjpeg::Codec decoder;
  std::mutex held_mu;
  std::vector<IMediaSample*> held;
public:
  std::atomic<uint64_t> samples{0}, invalid{0};
  VerifySink(const VCStream& s,const std::string& path,bool block,bool hold,bool mjpg,bool flat):stream(s),block_receive(block),hold_samples(hold),jpeg(mjpg),checkFlat(flat){
    if(!path.empty()) hashes.open(path,std::ios::binary);
    if(jpeg)decoder.open(stream,false,80,8*1024*1024);
  }
  ~VerifySink(){for(auto* sample:held)sample->Release();if(flushed)CloseHandle(flushed);}
  STDMETHODIMP QueryInterface(REFIID id,void** out){
    if(!out) return E_POINTER; *out=nullptr;
    if(id==IID_IUnknown || id==IID_IPin) *out=(IPin*)this;
    else if(id==IID_IMemInputPin) *out=(IMemInputPin*)this;
    else return E_NOINTERFACE; AddRef(); return S_OK;
  }
  STDMETHODIMP_(ULONG) AddRef(){ return InterlockedIncrement(&refs); }
  STDMETHODIMP_(ULONG) Release(){ LONG r=InterlockedDecrement(&refs); if(!r) delete this; return r; }
  STDMETHODIMP Connect(IPin*,const AM_MEDIA_TYPE*){ return E_NOTIMPL; }
  STDMETHODIMP ReceiveConnection(IPin*,const AM_MEDIA_TYPE* mt){
    if(!mt || mt->cbFormat<sizeof(VIDEOINFOHEADER) || !mt->pbFormat) return VFW_E_TYPE_NOT_ACCEPTED;
    auto* v=(VIDEOINFOHEADER*)mt->pbFormat;
    return v->bmiHeader.biWidth==stream.width && v->bmiHeader.biHeight==stream.height &&
      (jpeg || v->bmiHeader.biSizeImage==stream.frame_bytes()) && v->AvgTimePerFrame==10000000LL/stream.fps &&
      mt->subtype==(jpeg ? MEDIASUBTYPE_MJPG : MEDIASUBTYPE_NV12_EXPECTED) ? S_OK:VFW_E_TYPE_NOT_ACCEPTED;
  }
  STDMETHODIMP Disconnect(){ return S_OK; }
  STDMETHODIMP ConnectedTo(IPin** p){if(p)*p=nullptr;return VFW_E_NOT_CONNECTED;}
  STDMETHODIMP ConnectionMediaType(AM_MEDIA_TYPE*){return E_NOTIMPL;}
  STDMETHODIMP QueryPinInfo(PIN_INFO* p){if(!p)return E_POINTER;ZeroMemory(p,sizeof(*p));p->dir=PINDIR_INPUT;return S_OK;}
  STDMETHODIMP QueryDirection(PIN_DIRECTION* p){if(!p)return E_POINTER;*p=PINDIR_INPUT;return S_OK;}
  STDMETHODIMP QueryId(LPWSTR*){return E_NOTIMPL;}
  STDMETHODIMP QueryAccept(const AM_MEDIA_TYPE* p){return ReceiveConnection(nullptr,p);}
  STDMETHODIMP EnumMediaTypes(IEnumMediaTypes**){return E_NOTIMPL;}
  STDMETHODIMP QueryInternalConnections(IPin**,ULONG*){return E_NOTIMPL;}
  STDMETHODIMP EndOfStream(){return S_OK;}
  STDMETHODIMP BeginFlush(){
    SetEvent(flushed); std::lock_guard<std::mutex> lock(held_mu);
    for(auto* sample:held) sample->Release(); held.clear(); return S_OK;
  }
  STDMETHODIMP EndFlush(){ResetEvent(flushed);return S_OK;}
  STDMETHODIMP NewSegment(REFERENCE_TIME,REFERENCE_TIME,double){return S_OK;}
  STDMETHODIMP GetAllocator(IMemAllocator** p){return CoCreateInstance(CLSID_MemoryAllocator,nullptr,CLSCTX_INPROC_SERVER,IID_IMemAllocator,(void**)p);}
  STDMETHODIMP NotifyAllocator(IMemAllocator*,BOOL){return S_OK;}
  STDMETHODIMP GetAllocatorRequirements(ALLOCATOR_PROPERTIES*){return E_NOTIMPL;}
  STDMETHODIMP Receive(IMediaSample* sample){
    BYTE* p=nullptr;
    if(!sample || (!jpeg && sample->GetActualDataLength()!=stream.frame_bytes()) || sample->GetActualDataLength()<=0 ||
       sample->GetActualDataLength()>sample->GetSize() || FAILED(sample->GetPointer(&p)) || !p){invalid++;return E_FAIL;}
    const uint8_t* pixels=p;
    if(jpeg){try{pixels=decoder.decompress(p,(size_t)sample->GetActualDataLength()).data();}catch(...){invalid++;return E_FAIL;}}
    if(checkFlat){
      for(size_t i=0;i<stream.frame_bytes();i++){
        const int expected=i<stream.y_bytes()?96:((i-stream.y_bytes())%2?208:32);
        if(abs((int)pixels[i]-expected)>(jpeg?2:0)){invalid++;return E_FAIL;}
      }
    }
    uint64_t hash=vc_fnv1a64(pixels,stream.frame_bytes());
    REFERENCE_TIME start=0,end=0; sample->GetTime(&start,&end);
    if(end-start!=10000000LL/stream.fps) invalid++;
    char hex[17];vc_fnv1a64_hex(hash,hex);
    if(hashes) hashes<<"{\"sample\":"<<samples.load()<<",\"hash\":\""<<hex<<"\",\"start_100ns\":"<<start<<"}\n";
    if(hold_samples){std::lock_guard<std::mutex> lock(held_mu);if(WaitForSingleObject(flushed,0)!=WAIT_OBJECT_0){sample->AddRef();held.push_back(sample);}}
    samples++;if(block_receive)WaitForSingleObject(flushed,INFINITE);return S_OK;
  }
  STDMETHODIMP ReceiveMultiple(IMediaSample** s,LONG n,LONG* done){*done=0;for(LONG i=0;i<n;i++){HRESULT h=Receive(s[i]);if(FAILED(h))return h;(*done)++;}return S_OK;}
  STDMETHODIMP ReceiveCanBlock(){return block_receive?S_OK:S_FALSE;}
};

int main(int argc,char** argv){
  VCStream stream; std::string dll,map,hashlog; int seconds=2; bool capabilities=false,allow_empty=false,block=false,hold=false,disconnect_running=false,jpeg=false,checkFlat=false;
  for(int i=1;i<argc;i++){
    std::string arg=argv[i];
    auto value=[&](){if(i+1>=argc){fprintf(stderr,"missing value\n");exit(2);}return std::string(argv[++i]);};
    if(arg=="--dll") dll=value(); else if(arg=="--shm") map=value(); else if(arg=="--hashlog") hashlog=value();
    else if(arg=="--width") stream.width=vc_cli_int(value().c_str()); else if(arg=="--height") stream.height=vc_cli_int(value().c_str());
    else if(arg=="--fps") stream.fps=vc_cli_int(value().c_str()); else if(arg=="--seconds") seconds=vc_cli_int(value().c_str());
    else if(arg=="--format"){const auto format=value();if(format!="nv12" && format!="mjpeg"){fprintf(stderr,"format must be nv12|mjpeg\n");return 2;}jpeg=format=="mjpeg";}
    else if(arg=="--capabilities") capabilities=true; else if(arg=="--allow-empty") allow_empty=true;
    else if(arg=="--check-flat") checkFlat=true;
    else if(arg=="--block-receive") block=true;
    else if(arg=="--hold-samples") hold=true;
    else if(arg=="--disconnect-running") disconnect_running=true;
    else {fprintf(stderr,"unknown arg %s\n",arg.c_str());return 2;}
  }
  if(!stream.valid() || seconds<1 || seconds>120) return 2;
  if(dll.empty()) return enumerate(stream.width,stream.height,stream.fps);
  HMODULE module=LoadLibraryW(std::filesystem::absolute(dll).wstring().c_str());
  if(!module){fprintf(stderr,"LoadLibrary failed: %lu\n",GetLastError());return 1;}
  auto caps=(const char* (__cdecl*)())GetProcAddress(module,"VCStreamCapabilities");
  if(!caps){fprintf(stderr,"DLL lacks P6 stream capabilities\n");FreeLibrary(module);return 1;}
  if(capabilities){puts(caps());FreeLibrary(module);return 0;}
  if(!map.empty()) SetEnvironmentVariableW(L"VCAM_FRAME_MAP",std::filesystem::path(map).wstring().c_str());
  CoInitializeEx(nullptr,COINIT_MULTITHREADED);
  static const CLSID clsid={0xb84c7e21,0x4a5f,0x4e2b,{0x9c,0x11,0x5a,0x3d,0x7f,0x01,0x22,0x9a}};
  auto get=(HRESULT (STDAPICALLTYPE*)(REFCLSID,REFIID,void**))GetProcAddress(module,"DllGetClassObject");
  IClassFactory* factory=nullptr;IBaseFilter* filter=nullptr;IPin* pin=nullptr;IGraphBuilder* graph=nullptr;
  HRESULT hr=get?get(clsid,IID_IClassFactory,(void**)&factory):E_FAIL;
  if(SUCCEEDED(hr)) hr=factory->CreateInstance(nullptr,IID_IBaseFilter,(void**)&filter);
  if(factory) factory->Release();
  if(SUCCEEDED(hr)) hr=CoCreateInstance(CLSID_FilterGraph,nullptr,CLSCTX_INPROC_SERVER,IID_IGraphBuilder,(void**)&graph);
  if(SUCCEEDED(hr)) hr=graph->AddFilter(filter,L"Isolated VirtualCapture");
  if(SUCCEEDED(hr)) hr=filter->FindPin(L"Capture",&pin);
  VerifySink* sink=new VerifySink(stream,hashlog,block,hold,jpeg,checkFlat);
  IAMStreamConfig* config=nullptr;AM_MEDIA_TYPE* selected=nullptr;
  if(SUCCEEDED(hr)) hr=pin->QueryInterface(IID_IAMStreamConfig,(void**)&config);
  if(SUCCEEDED(hr)){
    int count=0,size=0;hr=config->GetNumberOfCapabilities(&count,&size);
    if(SUCCEEDED(hr) && (count!=2 || size!=sizeof(VIDEO_STREAM_CONFIG_CAPS)))hr=E_FAIL;
    VIDEO_STREAM_CONFIG_CAPS caps={};
    if(SUCCEEDED(hr))hr=config->GetStreamCaps(jpeg?1:0,&selected,(BYTE*)&caps);
    if(SUCCEEDED(hr))hr=config->SetFormat(selected);
    AM_MEDIA_TYPE* actual=nullptr;
    if(SUCCEEDED(hr))hr=config->GetFormat(&actual);
    if(SUCCEEDED(hr) && (!actual || actual->subtype!=selected->subtype))hr=E_FAIL;
    FreeMediaType(actual);
    if(SUCCEEDED(hr) && pin->QueryAccept(selected)!=S_OK)hr=E_FAIL;
    if(SUCCEEDED(hr)){
      auto* video=(VIDEOINFOHEADER*)selected->pbFormat;
      const LONG savedWidth=video->bmiHeader.biWidth;video->bmiHeader.biWidth+=2;
      if(pin->QueryAccept(selected)!=S_FALSE || SUCCEEDED(config->SetFormat(selected)))hr=E_FAIL;
      video->bmiHeader.biWidth=savedWidth;
      const GUID savedSubtype=selected->subtype;selected->subtype=MEDIASUBTYPE_RGB24;
      if(pin->QueryAccept(selected)!=S_FALSE)hr=E_FAIL;selected->subtype=savedSubtype;
      IEnumMediaTypes* types=nullptr;
      if(SUCCEEDED(hr))hr=pin->EnumMediaTypes(&types);
      if(SUCCEEDED(hr)){
        AM_MEDIA_TYPE* entries[2]={};ULONG got=0;
        if(types->Next(2,entries,&got)!=S_OK || got!=2 || entries[0]->subtype!=MEDIASUBTYPE_NV12_EXPECTED || entries[1]->subtype!=MEDIASUBTYPE_MJPG)hr=E_FAIL;
        FreeMediaType(entries[0]);FreeMediaType(entries[1]);
        types->Reset();if(types->Skip(2)!=S_OK || types->Skip(1)!=S_FALSE)hr=E_FAIL;
        types->Release();
      }
    }
  }
  if(SUCCEEDED(hr)) hr=pin->Connect(sink,nullptr);
  if(config)config->Release();FreeMediaType(selected);
  if(SUCCEEDED(hr)) hr=filter->Run(0);
  if(SUCCEEDED(hr)) Sleep((DWORD)seconds*1000);
  auto stop_start=std::chrono::steady_clock::now();
  if(disconnect_running && pin) pin->Disconnect();
  if(filter) filter->Stop();
  double stop_ms=std::chrono::duration<double,std::milli>(std::chrono::steady_clock::now()-stop_start).count();
  bool pass=SUCCEEDED(hr) && sink->invalid==0 && (allow_empty || sink->samples>0) && stop_ms<2000;
  printf("Stop/Disconnect %.3f ms block_receive=%d hold_samples=%d disconnect_running=%d\n",stop_ms,block,hold,disconnect_running);
  printf("DLL stream %dx%d@%d format=%s samples=%llu invalid=%llu hr=0x%08x %s\n",stream.width,stream.height,stream.fps,jpeg?"MJPG":"NV12",
    (unsigned long long)sink->samples.load(),(unsigned long long)sink->invalid.load(),(unsigned)hr,pass?"PASS":"FAIL");
  if(pin){pin->Disconnect();pin->Release();}
  if(filter) filter->Release();
  if(graph) graph->Release();
  sink->Release();
  CoUninitialize();
  auto unload=(HRESULT (STDAPICALLTYPE*)())GetProcAddress(module,"DllCanUnloadNow");
  if(!unload || unload()!=S_OK){fprintf(stderr,"DLL retained COM references after Stop/Disconnect\n");return 1;}
  FreeLibrary(module);
  return pass?0:1;
}
