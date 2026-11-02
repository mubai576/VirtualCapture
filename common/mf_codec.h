#pragma once
#define NOMINMAX
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mftransform.h>
#include <mferror.h>
#include <codecapi.h>
#include <icodecapi.h>
#include <wrl/client.h>
#include <chrono>
#include <functional>
#include <stdexcept>
#include <vector>
#include "stream.h"

namespace vcmf {
using Microsoft::WRL::ComPtr;
inline void check(HRESULT hr,const char* operation) {
  if(FAILED(hr)){char text[256];snprintf(text,sizeof(text),"%s: HRESULT 0x%08x",operation,(unsigned)hr);throw std::runtime_error(text);}
}
inline ComPtr<IMFMediaType> media_type(REFGUID subtype,const VCStream& stream,bool encoded,UINT32 bitrate) {
  ComPtr<IMFMediaType> type;check(MFCreateMediaType(&type),"MFCreateMediaType");
  check(type->SetGUID(MF_MT_MAJOR_TYPE,MFMediaType_Video),"major type");
  check(type->SetGUID(MF_MT_SUBTYPE,subtype),"subtype");
  check(MFSetAttributeSize(type.Get(),MF_MT_FRAME_SIZE,stream.width,stream.height),"frame size");
  check(MFSetAttributeRatio(type.Get(),MF_MT_FRAME_RATE,stream.fps,1),"frame rate");
  check(MFSetAttributeRatio(type.Get(),MF_MT_PIXEL_ASPECT_RATIO,1,1),"aspect ratio");
  check(type->SetUINT32(MF_MT_INTERLACE_MODE,MFVideoInterlace_Progressive),"progressive");
  if(encoded){
    check(type->SetUINT32(MF_MT_AVG_BITRATE,bitrate),"bitrate type");
    check(type->SetUINT32(MF_MT_MPEG2_PROFILE,subtype==MFVideoFormat_H264?77:1),"codec profile");
  } else {
    check(type->SetUINT32(MF_MT_DEFAULT_STRIDE,stream.width),"stride");
    check(type->SetUINT32(MF_MT_SAMPLE_SIZE,(UINT32)stream.frame_bytes()),"sample size");
    check(type->SetUINT32(MF_MT_VIDEO_NOMINAL_RANGE,MFNominalRange_0_255),"full range");
    check(type->SetUINT32(MF_MT_YUV_MATRIX,MFVideoTransferMatrix_BT601),"BT601");
  }
  return type;
}
inline ComPtr<IMFSample> sample(const uint8_t* data,size_t size,LONGLONG timestamp,LONGLONG duration) {
  ComPtr<IMFSample> result;ComPtr<IMFMediaBuffer> buffer;
  check(MFCreateSample(&result),"MFCreateSample");check(MFCreateMemoryBuffer((DWORD)size,&buffer),"MFCreateMemoryBuffer");
  BYTE* bytes=nullptr;check(buffer->Lock(&bytes,nullptr,nullptr),"input Lock");
  memcpy(bytes,data,size);buffer->Unlock();check(buffer->SetCurrentLength((DWORD)size),"input length");
  check(result->AddBuffer(buffer.Get()),"input buffer");check(result->SetSampleTime(timestamp),"input timestamp");
  check(result->SetSampleDuration(duration),"input duration");return result;
}

struct NV12Layout {
  UINT32 width=0,height=0,x=0,y=0;
  bool read(IMFMediaType* type,const VCStream& stream) {
    GUID subtype={};if(FAILED(type->GetGUID(MF_MT_SUBTYPE,&subtype)) || subtype!=MFVideoFormat_NV12 ||
      FAILED(MFGetAttributeSize(type,MF_MT_FRAME_SIZE,&width,&height)) || width>16384 || height>16384 ||
      width<(UINT32)stream.width || height<(UINT32)stream.height || (width&1) || (height&1)) return false;
    x=y=0;
    // Macroblock/CTU padding is not a resize: require an explicit exact, chroma-aligned display aperture.
    for(const GUID* key:{&MF_MT_MINIMUM_DISPLAY_APERTURE,&MF_MT_GEOMETRIC_APERTURE}){
      MFVideoArea area={};UINT32 bytes=0;
      HRESULT hr=type->GetBlob(*key,(UINT8*)&area,sizeof(area),&bytes);
      if(hr==MF_E_ATTRIBUTENOTFOUND) continue;
      if(SUCCEEDED(hr) && bytes==sizeof(area) && !area.OffsetX.fract && !area.OffsetY.fract && area.OffsetX.value>=0 && area.OffsetY.value>=0 &&
        !(area.OffsetX.value&1) && !(area.OffsetY.value&1) && area.Area.cx==stream.width && area.Area.cy==stream.height &&
        (UINT32)area.OffsetX.value+stream.width<=width && (UINT32)area.OffsetY.value+stream.height<=height){
          x=area.OffsetX.value;y=area.OffsetY.value;return true;
        }
      return false;
    }
    return width==(UINT32)stream.width && height==(UINT32)stream.height;
  }
  bool copy(const uint8_t* source,size_t size,UINT32 stride,const VCStream& stream,uint8_t* target) const {
    if(stride<width || stride>32768 || (size_t)stride*height*3/2>size ||
      x+(UINT32)stream.width>width || y+(UINT32)stream.height>height || (x&1) || (y&1)) return false;
    for(int row=0;row<stream.height;row++) memcpy(target+(size_t)row*stream.width,source+(size_t)(y+row)*stride+x,stream.width);
    for(int row=0;row<stream.height/2;row++) memcpy(target+stream.y_bytes()+(size_t)row*stream.width,
      source+(size_t)stride*height+(size_t)(y/2+row)*stride+x,stream.width);
    return true;
  }
};

// Explicit MFT identity and hardware category: never choose a different codec/device on failure.
class Transform {
  ComPtr<IMFActivate> activation;
  ComPtr<IMFTransform> transform;
  ComPtr<IMFMediaEventGenerator> events;
  VCStream stream;
  bool encode=false,async=false,drained=false;
  unsigned inputs=0,outputs=0;
  DWORD input_id=0,output_id=0;
  std::function<void(IMFSample*)> deliver;
  void property(REFGUID key,ULONG value,const char* label,bool boolean=false) {
    ComPtr<ICodecAPI> api;check(transform.As(&api),"ICodecAPI");
    VARIANT v;VariantInit(&v);if(boolean){v.vt=VT_BOOL;v.boolVal=value?VARIANT_TRUE:VARIANT_FALSE;}else{v.vt=VT_UI4;v.ulVal=value;}
    HRESULT hr=api->SetValue(&key,&v);
    if(FAILED(hr)){
      VARIANT current;VariantInit(&current);HRESULT read=api->GetValue(&key,&current);
      fprintf(stderr,"property %s set=0x%08x read=0x%08x type=%u value=%lu\n",label,(unsigned)hr,(unsigned)read,current.vt,current.ulVal);
      VariantClear(&current);
    }
    check(hr,label);
  }
  void output_type() {
    for(DWORD index=0;;index++){
      ComPtr<IMFMediaType> type;HRESULT hr=transform->GetOutputAvailableType(output_id,index,&type);
      if(hr==MF_E_NO_MORE_TYPES) break;check(hr,"GetOutputAvailableType");
      GUID subtype={};type->GetGUID(MF_MT_SUBTYPE,&subtype);
      if(subtype!=MFVideoFormat_NV12) continue;
      NV12Layout layout;if(!layout.read(type.Get(),stream)) continue;
      check(transform->SetOutputType(output_id,type.Get(),0),"decoder stream-change NV12");return;
    }
    throw std::runtime_error("decoder did not offer the exact session NV12 dimensions; no resize fallback");
  }
  bool output() {
    for(int changed=0;changed<3;changed++) {
      MFT_OUTPUT_STREAM_INFO info={};check(transform->GetOutputStreamInfo(output_id,&info),"GetOutputStreamInfo");
      ComPtr<IMFSample> owned;ComPtr<IMFMediaBuffer> buffer;
      if(!(info.dwFlags&MFT_OUTPUT_STREAM_PROVIDES_SAMPLES)) {
        check(MFCreateSample(&owned),"output sample");
        DWORD size=std::max(info.cbSize,(DWORD)stream.frame_bytes());
        check(MFCreateAlignedMemoryBuffer(size,info.cbAlignment?info.cbAlignment-1:0,&buffer),"output buffer");
        check(owned->AddBuffer(buffer.Get()),"output AddBuffer");
      }
      MFT_OUTPUT_DATA_BUFFER out={};out.dwStreamID=output_id;out.pSample=owned.Get();DWORD status=0;
      HRESULT hr=transform->ProcessOutput(0,1,&out,&status);
      if(out.pEvents) out.pEvents->Release();
      ComPtr<IMFSample> provided;if(out.pSample && out.pSample!=owned.Get()) provided.Attach(out.pSample);
      if(hr==MF_E_TRANSFORM_STREAM_CHANGE && !encode){output_type();continue;}
      if(hr==MF_E_TRANSFORM_NEED_MORE_INPUT) return false;
      check(hr,"ProcessOutput");
      if(out.pSample){deliver(out.pSample);return true;}
      return false;
    }
    throw std::runtime_error("repeated decoder stream-change");
  }
public:
  HRESULT b_picture_control=E_NOTIMPL,gop_control=E_NOTIMPL;
  NV12Layout decoded_layout;
  Transform()=default;
  Transform(const Transform&)=delete;
  Transform& operator=(const Transform&)=delete;
  ~Transform(){
    if(transform){transform->ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH,0);transform->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING,0);}
    events.Reset();transform.Reset();if(activation) activation->ShutdownObject();
  }
  bool asynchronous() const{return async;}
  void open(REFGUID codec,const VCStream& config,bool encoder,bool hardware,REFGUID clsid,UINT32 bitrate,
            std::function<void(IMFSample*)> callback,const std::wstring& exact_name=L"") {
    if(transform || !config.valid() || !bitrate) throw std::runtime_error("invalid/repeated transform configuration");
    stream=config;encode=encoder;deliver=std::move(callback);
    MFT_REGISTER_TYPE_INFO in={MFMediaType_Video,encoder?MFVideoFormat_NV12:codec};
    MFT_REGISTER_TYPE_INFO out={MFMediaType_Video,encoder?codec:MFVideoFormat_NV12};
    IMFActivate** found=nullptr;UINT32 count=0;
    check(MFTEnumEx(encoder?MFT_CATEGORY_VIDEO_ENCODER:MFT_CATEGORY_VIDEO_DECODER,
      hardware?MFT_ENUM_FLAG_HARDWARE:(MFT_ENUM_FLAG_SYNCMFT|MFT_ENUM_FLAG_ASYNCMFT),&in,&out,&found,&count),"MFTEnumEx");
    UINT32 matches=0;
    for(UINT32 i=0;i<count;i++){
      GUID id={};found[i]->GetGUID(MFT_TRANSFORM_CLSID_Attribute,&id);
      wchar_t* name=nullptr;UINT32 chars=0;found[i]->GetAllocatedString(MFT_FRIENDLY_NAME_Attribute,&name,&chars);
      bool match=clsid!=GUID_NULL?id==clsid:(!exact_name.empty() && name && exact_name==name);
      if(match){matches++;if(!activation) activation=found[i];}
      CoTaskMemFree(name);
      found[i]->Release();
    }
    CoTaskMemFree(found);
    if(matches!=1) throw std::runtime_error("requested codec/MFT/hardware category not uniquely present; no fallback");
    check(activation->ActivateObject(IID_PPV_ARGS(&transform)),"ActivateObject");
    ComPtr<IMFAttributes> attrs;UINT32 flag=0;
    if(SUCCEEDED(transform->GetAttributes(&attrs)) && attrs){
      attrs->GetUINT32(MF_TRANSFORM_ASYNC,&flag);async=flag!=0;
      if(async) check(attrs->SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK,TRUE),"async unlock");
      check(attrs->SetUINT32(MF_LOW_LATENCY,TRUE),"MF_LOW_LATENCY");
    }
    if(async) check(transform.As(&events),"async events");
    DWORD ins=0,outs=0;check(transform->GetStreamCount(&ins,&outs),"GetStreamCount");
    if(ins!=1 || outs!=1) throw std::runtime_error("MFT must have one input and one output");
    HRESULT ids=transform->GetStreamIDs(1,&input_id,1,&output_id);if(ids!=E_NOTIMPL) check(ids,"GetStreamIDs");
    if(encoder) property(CODECAPI_AVLowLatencyMode,1,"low latency",true);
    auto raw=media_type(MFVideoFormat_NV12,stream,false,bitrate),compressed=media_type(codec,stream,true,bitrate);
    if(encoder){check(transform->SetOutputType(output_id,compressed.Get(),0),"encoder output type");check(transform->SetInputType(input_id,raw.Get(),0),"encoder input type");}
    else {check(transform->SetInputType(input_id,compressed.Get(),0),"decoder input type");check(transform->SetOutputType(output_id,raw.Get(),0),"decoder output type");}
    if(encoder){
      property(CODECAPI_AVEncCommonRateControlMode,eAVEncCommonRateControlMode_CBR,"CBR");
      property(CODECAPI_AVEncCommonMeanBitRate,bitrate,"mean bitrate");
      // These advisory knobs are not portable; low latency + one-in-flight is validated with real frames.
      ComPtr<ICodecAPI> api;check(transform.As(&api),"ICodecAPI");VARIANT v;VariantInit(&v);v.vt=VT_UI4;
      v.ulVal=0;b_picture_control=api->SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount,&v);
      v.ulVal=stream.fps;gop_control=api->SetValue(&CODECAPI_AVEncMPVGOPSize,&v);
    }
    check(transform->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,0),"begin streaming");
    check(transform->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM,0),"start stream");
  }
  void pump() {
    if(!async){while(output()){}return;}
    for(;;){
      ComPtr<IMFMediaEvent> event;HRESULT hr=events->GetEvent(MF_EVENT_FLAG_NO_WAIT,&event);
      if(hr==MF_E_NO_EVENTS_AVAILABLE) break;check(hr,"GetEvent");
      HRESULT status=S_OK;check(event->GetStatus(&status),"event status");check(status,"async transform event");
      MediaEventType kind=MEUnknown;check(event->GetType(&kind),"event kind");
      if(kind==METransformNeedInput) inputs++;
      else if(kind==METransformHaveOutput) outputs++;
      else if(kind==METransformDrainComplete) drained=true;
      else if(kind==MEError) throw std::runtime_error("async MFT MEError");
    }
    while(outputs){--outputs;output();}
  }
  void submit(IMFSample* input) {
    auto deadline=std::chrono::steady_clock::now()+std::chrono::seconds(5);
    for(;;){
      if(async) pump();
      if(!async || inputs){
        HRESULT hr=transform->ProcessInput(input_id,input,0);
        if(hr!=MF_E_NOTACCEPTING){check(hr,"ProcessInput");if(async) --inputs;return;}
        if(!async) pump();
      }
      if(std::chrono::steady_clock::now()>=deadline) throw std::runtime_error("MFT input timeout (5s)");
      Sleep(1);
    }
  }
  void force_keyframe(){if(!encode)throw std::runtime_error("not encoder");property(CODECAPI_AVEncVideoForceKeyFrame,1,"force IDR");}
  void drain() {
    check(transform->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM,input_id),"end of stream");
    check(transform->ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN,0),"drain");
    auto deadline=std::chrono::steady_clock::now()+std::chrono::seconds(5);
    do {pump();if(!async || drained) return;Sleep(1);} while(std::chrono::steady_clock::now()<deadline);
    throw std::runtime_error("MFT drain timeout (5s)");
  }
  std::vector<uint8_t> packed_nv12(IMFSample* decoded) {
    ComPtr<IMFMediaType> type;check(transform->GetOutputCurrentType(output_id,&type),"decoded type");
    if(!decoded_layout.read(type.Get(),stream)) throw std::runtime_error("decoded geometry/display aperture/subtype mismatch");
    ComPtr<IMFMediaBuffer> buffer;check(decoded->ConvertToContiguousBuffer(&buffer),"decoded buffer");
    std::vector<uint8_t> packed(stream.frame_bytes());
    BYTE* p=nullptr;DWORD length=0;check(buffer->Lock(&p,nullptr,&length),"decoded Lock");
    UINT32 stride=decoded_layout.width;type->GetUINT32(MF_MT_DEFAULT_STRIDE,&stride);
    if(!decoded_layout.copy(p,length,stride,stream,packed.data())){buffer->Unlock();throw std::runtime_error("decoded NV12 buffer stride/capacity mismatch");}
    check(buffer->Unlock(),"decoded Unlock");return packed;
  }
};
}
