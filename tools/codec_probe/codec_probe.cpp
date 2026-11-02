#define NOMINMAX
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mftransform.h>
#include <codecapi.h>
#include <icodecapi.h>
#include <wrl/client.h>
#include <cstdio>
#include <string>
#include "../../common/stream.h"
using Microsoft::WRL::ComPtr;

static std::string json_string(const wchar_t* text) {
  if(!text) return "\"\"";
  int n=WideCharToMultiByte(CP_UTF8,0,text,-1,nullptr,0,nullptr,nullptr);
  std::string utf8(n,'\0');
  WideCharToMultiByte(CP_UTF8,0,text,-1,utf8.data(),n,nullptr,nullptr);
  std::string result="\"";
  for(int i=0;i<n-1;i++) {
    unsigned char c=(unsigned char)utf8[i];
    if(c=='"' || c=='\\') result+='\\';
    if(c<32) { char escaped[7]; snprintf(escaped,sizeof(escaped),"\\u%04x",c); result+=escaped; }
    else result+=(char)c;
  }
  return result+'"';
}
static std::string hr_json(HRESULT hr) {
  char b[16];snprintf(b,sizeof(b),"\"0x%08x\"",(unsigned)hr);return b;
}
static ComPtr<IMFMediaType> video_type(REFGUID subtype,const VCStream& stream,bool encoded) {
  ComPtr<IMFMediaType> type;
  if(FAILED(MFCreateMediaType(&type))) return type;
  type->SetGUID(MF_MT_MAJOR_TYPE,MFMediaType_Video);
  type->SetGUID(MF_MT_SUBTYPE,subtype);
  MFSetAttributeSize(type.Get(),MF_MT_FRAME_SIZE,stream.width,stream.height);
  MFSetAttributeRatio(type.Get(),MF_MT_FRAME_RATE,stream.fps,1);
  MFSetAttributeRatio(type.Get(),MF_MT_PIXEL_ASPECT_RATIO,1,1);
  type->SetUINT32(MF_MT_INTERLACE_MODE,MFVideoInterlace_Progressive);
  if(encoded) type->SetUINT32(MF_MT_AVG_BITRATE,50000000);
  else { type->SetUINT32(MF_MT_DEFAULT_STRIDE,stream.width);type->SetUINT32(MF_MT_SAMPLE_SIZE,(UINT32)stream.frame_bytes()); }
  return type;
}

// Enumeration/activation/type negotiation only. No frames means no throughput/quality claim.
static void probe(REFGUID codec,const char* name,bool encode,bool hardware,const VCStream& stream,bool& first) {
  MFT_REGISTER_TYPE_INFO in={MFMediaType_Video,encode?MFVideoFormat_NV12:codec};
  MFT_REGISTER_TYPE_INFO out={MFMediaType_Video,encode?codec:MFVideoFormat_NV12};
  IMFActivate** candidates=nullptr;UINT32 count=0;
  UINT32 flags=hardware?MFT_ENUM_FLAG_HARDWARE:(MFT_ENUM_FLAG_SYNCMFT|MFT_ENUM_FLAG_ASYNCMFT);
  HRESULT enumerated=MFTEnumEx(encode?MFT_CATEGORY_VIDEO_ENCODER:MFT_CATEGORY_VIDEO_DECODER,
                              flags,&in,&out,&candidates,&count);
  if(FAILED(enumerated)) {
    if(!first) printf(",");first=false;
    printf("{\"codec\":\"%s\",\"direction\":\"%s\",\"hardware_enumeration\":%s,\"enumeration_hr\":%s}",
           name,encode?"encoder":"decoder",hardware?"true":"false",hr_json(enumerated).c_str());
  }
  for(UINT32 i=0;i<count;i++) {
    if(!first) printf(",");first=false;
    wchar_t* friendly=nullptr;UINT32 chars=0;
    candidates[i]->GetAllocatedString(MFT_FRIENDLY_NAME_Attribute,&friendly,&chars);
    GUID clsid=GUID_NULL; wchar_t clsid_text[40]={};
    HRESULT clsid_hr=candidates[i]->GetGUID(MFT_TRANSFORM_CLSID_Attribute,&clsid);
    StringFromGUID2(clsid,clsid_text,40);
    ComPtr<IMFTransform> transform;
    HRESULT activated=candidates[i]->ActivateObject(IID_PPV_ARGS(&transform));
    HRESULT configured=activated;bool async=false,low_latency=false;
    if(SUCCEEDED(activated)) {
      ComPtr<IMFAttributes> attrs;UINT32 value=0;
      if(SUCCEEDED(transform->GetAttributes(&attrs)) && attrs) {
        attrs->GetUINT32(MF_TRANSFORM_ASYNC,&value);async=value!=0;
        if(async) attrs->SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK,TRUE);
      }
      ComPtr<ICodecAPI> api;
      if(SUCCEEDED(transform.As(&api))) low_latency=api->IsSupported(&CODECAPI_AVLowLatencyMode)==S_OK;
      auto raw=video_type(MFVideoFormat_NV12,stream,false),compressed=video_type(codec,stream,true);
      if(!raw || !compressed) configured=E_OUTOFMEMORY;
      else if(encode) {
        configured=transform->SetOutputType(0,compressed.Get(),0);
        if(SUCCEEDED(configured)) configured=transform->SetInputType(0,raw.Get(),0);
      } else {
        configured=transform->SetInputType(0,compressed.Get(),0);
        if(SUCCEEDED(configured)) configured=transform->SetOutputType(0,raw.Get(),0);
      }
      transform->ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH,0);
    }
    printf("{\"codec\":\"%s\",\"direction\":\"%s\",\"name\":%s,\"clsid\":%s,\"clsid_hr\":%s,\"hardware_enumeration\":%s,"
           "\"activation_hr\":%s,\"type_hr\":%s,\"asynchronous\":%s,\"low_latency_property\":%s,"
           "\"frames_tested\":0,\"stream_supported\":false}",name,encode?"encoder":"decoder",
           json_string(friendly).c_str(),SUCCEEDED(clsid_hr)?json_string(clsid_text).c_str():"null",hr_json(clsid_hr).c_str(),hardware?"true":"false",hr_json(activated).c_str(),hr_json(configured).c_str(),
           async?"true":"false",low_latency?"true":"false");
    CoTaskMemFree(friendly);
    transform.Reset();candidates[i]->ShutdownObject();candidates[i]->Release();
  }
  CoTaskMemFree(candidates);
}

int main(int argc,char** argv) {
  VCStream stream;
  for(int i=1;i<argc;i++) {
    std::string flag=argv[i];
    if(i+1>=argc){fprintf(stderr,"missing value\n");return 2;}
    int n=vc_cli_int(argv[++i]);
    if(flag=="--width") stream.width=n;else if(flag=="--height") stream.height=n;
    else if(flag=="--fps") stream.fps=n;else {fprintf(stderr,"unknown argument\n");return 2;}
  }
  if(!stream.valid()) return 2;
  HRESULT com=CoInitializeEx(nullptr,COINIT_MULTITHREADED);
  if(FAILED(com)) return 1;
  HRESULT mf=MFStartup(MF_VERSION,MFSTARTUP_FULL);
  if(FAILED(mf)){CoUninitialize();return 1;}
  printf("{\"backend\":\"windows-media-foundation\",\"probe_revision\":1,\"width\":%d,\"height\":%d,\"fps\":%d,"
         "\"status\":\"candidates_only_not_stream_validated\",\"candidates\":[",stream.width,stream.height,stream.fps);
  bool first=true;
  for(bool hardware:{true,false}) for(bool encoder:{true,false}) {
    probe(MFVideoFormat_H264,"h264",encoder,hardware,stream,first);
    probe(MFVideoFormat_HEVC,"hevc",encoder,hardware,stream,first);
  }
  printf("]}\n");
  MFShutdown();CoUninitialize();return 0;
}
