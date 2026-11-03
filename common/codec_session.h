#pragma once
#include "mf_codec.h"
#include "access_unit.h"
#include <memory>

inline std::wstring vc_wide(const std::string& text){
  int n=MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,text.data(),(int)text.size(),nullptr,0);
  if(!n)return L"";std::wstring out(n,L'\0');MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,text.data(),(int)text.size(),out.data(),n);return out;
}
struct VCCodec {
  std::string codec="raw",backend;
  int hardware=0,bitrate=20000000,max_bitrate=80000000,gop=60,in_flight=2,max_au=8*1024*1024;
  bool specified=false;
  bool option(const std::string& key,const std::string& value){
    if(key=="--codec"){codec=value;return true;}
    if(key=="--codec-backend")backend=value;
    else if(key=="--codec-hardware")hardware=vc_cli_int(value);
    else if(key=="--bitrate")bitrate=vc_cli_int(value);
    else if(key=="--max-bitrate")max_bitrate=vc_cli_int(value);
    else if(key=="--gop")gop=vc_cli_int(value);
    else if(key=="--decode-in-flight")in_flight=vc_cli_int(value);
    else if(key=="--max-au-bytes")max_au=vc_cli_int(value);
    else return false;
    specified=true;return true;
  }
  bool encoded()const{return codec!="raw";}
  int id()const{return codec=="h264"?1:2;}
  GUID subtype()const{return codec=="h264"?MFVideoFormat_H264:MFVideoFormat_HEVC;}
  bool valid(int mtu)const{
    if(!encoded())return !specified;
    GUID clsid={};bool identity=backend.rfind("name:",0)==0?backend.size()>5:
      SUCCEEDED(CLSIDFromString(vc_wide(backend).c_str(),&clsid)) && clsid!=GUID_NULL;
    return (codec=="h264" || codec=="hevc") && identity && (hardware==0 || hardware==1) &&
      bitrate>=100000 && max_bitrate>=bitrate && max_bitrate<=2000000000 && gop>=1 && gop<=1000 &&
      in_flight>=1 && in_flight<=8 && max_au>=1024 && max_au<=16*1024*1024 && mtu>42 &&
      ((size_t)max_au+mtu-43)/(mtu-42)<=65535;
  }
  void open(vcmf::Transform& transform,const VCStream& stream,bool encode,std::function<void(IMFSample*)> output)const{
    GUID clsid={};std::wstring name;
    if(backend.rfind("name:",0)==0)name=vc_wide(backend.substr(5));
    else vcmf::check(CLSIDFromString(vc_wide(backend).c_str(),&clsid),"backend CLSID");
    transform.open(subtype(),stream,encode,hardware!=0,clsid,(UINT32)bitrate,std::move(output),name);
    if(encode)transform.set_gop(gop);
  }
};
struct VCMFRuntime {
  VCMFRuntime(){vcmf::check(CoInitializeEx(nullptr,COINIT_MULTITHREADED),"CoInitializeEx");HRESULT hr=MFStartup(MF_VERSION);if(FAILED(hr)){CoUninitialize();vcmf::check(hr,"MFStartup");}}
  ~VCMFRuntime(){MFShutdown();CoUninitialize();}
};
inline const char* vc_codec_capabilities(){
  return "{\"stream_revision\":2,\"codec_revision\":1,\"codecs\":[\"raw\",\"h264\",\"hevc\"],\"pixel_format\":\"nv12\",\"variable_resolution\":true,\"dda_fps_limit\":true,\"shm_version\":3,\"max_dimension\":8192}";
}
inline std::string vc_json(const std::wstring& text){
  int n=WideCharToMultiByte(CP_UTF8,0,text.data(),(int)text.size(),nullptr,0,nullptr,nullptr);
  std::string utf8(n,'\0');WideCharToMultiByte(CP_UTF8,0,text.data(),(int)text.size(),utf8.data(),n,nullptr,nullptr);
  std::string out="\"";for(unsigned char c:utf8){if(c=='"' || c=='\\')out+='\\';if(c<32){char b[7];snprintf(b,7,"\\u%04x",c);out+=b;}else out+=(char)c;}return out+'"';
}
inline int vc_codec_backends(bool encoder){
  try{
    VCMFRuntime runtime;bool first=true;printf("{\"revision\":1,\"status\":\"candidates_only\",\"backends\":[");
    for(bool hardware:{true,false})for(int kind:{1,2}){
      GUID codec=kind==1?MFVideoFormat_H264:MFVideoFormat_HEVC;
      MFT_REGISTER_TYPE_INFO in={MFMediaType_Video,encoder?MFVideoFormat_NV12:codec},out={MFMediaType_Video,encoder?codec:MFVideoFormat_NV12};
      IMFActivate** candidates=nullptr;UINT32 count=0;
      vcmf::check(MFTEnumEx(encoder?MFT_CATEGORY_VIDEO_ENCODER:MFT_CATEGORY_VIDEO_DECODER,
        hardware?MFT_ENUM_FLAG_HARDWARE:(MFT_ENUM_FLAG_SYNCMFT|MFT_ENUM_FLAG_ASYNCMFT),&in,&out,&candidates,&count),"backend enumeration");
      for(UINT32 i=0;i<count;i++){
        wchar_t* name=nullptr;UINT32 chars=0;candidates[i]->GetAllocatedString(MFT_FRIENDLY_NAME_Attribute,&name,&chars);
        GUID id={};wchar_t guid[40]={};std::wstring identity;
        if(SUCCEEDED(candidates[i]->GetGUID(MFT_TRANSFORM_CLSID_Attribute,&id)) && id!=GUID_NULL){StringFromGUID2(id,guid,40);identity=guid;}
        else if(name)identity=L"name:"+std::wstring(name);
        if(!identity.empty()){
          if(!first)printf(",");first=false;
          printf("{\"codec\":\"%s\",\"identity\":%s,\"name\":%s,\"hardware\":%s,\"frames_tested\":0}",kind==1?"h264":"hevc",vc_json(identity).c_str(),vc_json(name?name:L"").c_str(),hardware?"true":"false");
        }
        CoTaskMemFree(name);candidates[i]->Release();
      }
      CoTaskMemFree(candidates);
    }
    puts("]}");return 0;
  }catch(const std::exception& e){fprintf(stderr,"codec enumeration failed: %s\n",e.what());return 1;}
}
inline int vc_codec_check(const VCCodec& options,const VCStream& stream,bool encoder){
  try{VCMFRuntime runtime;vcmf::Transform transform;options.open(transform,stream,encoder,[](IMFSample*){});
    puts("{\"revision\":1,\"configured\":true,\"frames_tested\":0,\"stream_validated\":false}");return 0;
  }catch(const std::exception& e){fprintf(stderr,"codec configuration failed (no fallback): %s\n",e.what());return 1;}
}
