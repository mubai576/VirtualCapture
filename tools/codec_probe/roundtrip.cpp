#include "../../common/mf_codec.h"
#include "../../common/fnv1a.h"
#include "../../sender/pattern.h"
#include <deque>
#include <fstream>
#include <cmath>

static std::wstring wide(const std::string& text){
  int n=MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,text.c_str(),(int)text.size(),nullptr,0);
  if(!n)return L"";std::wstring result(n,L'\0');MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,text.c_str(),(int)text.size(),result.data(),n);return result;
}
static std::string quoted(const std::string& text){
  std::string result="\"";for(unsigned char c:text){if(c=='"' || c=='\\')result+='\\';if(c<32){char b[7];snprintf(b,sizeof(b),"\\u%04x",c);result+=b;}else result+=(char)c;}return result+'"';
}
static int layout_selftest(){
  HRESULT co=CoInitializeEx(nullptr,COINIT_MULTITHREADED);if(FAILED(co))return 1;
  if(FAILED(MFStartup(MF_VERSION))){CoUninitialize();return 1;}
  int result=1;
  try{
    VCStream stream;stream.width=stream.height=2;
    auto type=vcmf::media_type(MFVideoFormat_NV12,stream,false,1000000);vcmf::NV12Layout layout;
    auto require=[](bool ok){if(!ok)throw std::runtime_error("NV12 layout selftest mismatch");};
    require(layout.read(type.Get(),stream));
    vcmf::check(MFSetAttributeSize(type.Get(),MF_MT_FRAME_SIZE,4,4),"test coded size");
    require(!layout.read(type.Get(),stream));
    MFVideoArea area={};area.OffsetX.value=area.OffsetY.value=2;area.Area.cx=area.Area.cy=2;
    vcmf::check(type->SetBlob(MF_MT_MINIMUM_DISPLAY_APERTURE,(UINT8*)&area,sizeof(area)),"test aperture");
    require(layout.read(type.Get(),stream));
    uint8_t source[24],target[6];for(int i=0;i<24;i++)source[i]=(uint8_t)i;
    require(layout.copy(source,24,4,stream,target));const uint8_t expected[6]={10,11,14,15,22,23};require(!memcmp(target,expected,6));
    require(!layout.copy(source,23,4,stream,target));require(!layout.copy(source,24,3,stream,target));
    for(int invalid=0;invalid<3;invalid++){
      MFVideoArea bad=area;if(invalid==0)bad.OffsetX.value=1;if(invalid==1)bad.OffsetY.fract=1;if(invalid==2)bad.Area.cx=4;
      type->SetBlob(MF_MT_MINIMUM_DISPLAY_APERTURE,(UINT8*)&bad,sizeof(bad));require(!layout.read(type.Get(),stream));
    }
    puts("PASS: codec NV12 exact aperture/stride/UV/capacity selftest");result=0;
  }catch(const std::exception& error){fprintf(stderr,"%s\n",error.what());}
  MFShutdown();CoUninitialize();return result;
}
int main(int argc,char** argv){
  if(argc==2 && std::string(argv[1])=="--selftest")return layout_selftest();
  VCStream stream;int frames=30,bitrate=20000000,in_flight=1;std::string codec="h264",encoder_id,decoder_id,encoder_name,decoder_name,log;
  bool hardware_encoder=false,hardware_decoder=false;
  for(int i=1;i<argc;i++){
    std::string key=argv[i];
    if(key=="--hardware-encoder"){hardware_encoder=true;continue;}
    if(key=="--hardware-decoder"){hardware_decoder=true;continue;}
    if(i+1>=argc){fprintf(stderr,"missing value\n");return 2;}
    std::string value=argv[++i];
    if(key=="--width")stream.width=vc_cli_int(value);else if(key=="--height")stream.height=vc_cli_int(value);
    else if(key=="--fps")stream.fps=vc_cli_int(value);else if(key=="--frames")frames=vc_cli_int(value);
    else if(key=="--max-in-flight")in_flight=vc_cli_int(value);
    else if(key=="--bitrate")bitrate=vc_cli_int(value);else if(key=="--codec")codec=value;
    else if(key=="--encoder-clsid")encoder_id=value;else if(key=="--decoder-clsid")decoder_id=value;
    else if(key=="--encoder-name")encoder_name=value;else if(key=="--decoder-name")decoder_name=value;
    else if(key=="--hashlog")log=value;else{fprintf(stderr,"unknown option %s\n",key.c_str());return 2;}
  }
  if(!stream.valid() || frames<1 || frames>10000 || in_flight<1 || in_flight>8 || bitrate<100000 || (codec!="h264" && codec!="hevc") ||
    (encoder_id.empty()==encoder_name.empty()) || (decoder_id.empty()==decoder_name.empty())){
    fprintf(stderr,"explicit encoder/decoder CLSIDs and valid geometry/codec/bitrate are required\n");return 2;
  }
  GUID encoder={},decoder={};
  if((!encoder_id.empty() && (FAILED(CLSIDFromString(wide(encoder_id).c_str(),&encoder)) || encoder==GUID_NULL)) ||
     (!decoder_id.empty() && (FAILED(CLSIDFromString(wide(decoder_id).c_str(),&decoder)) || decoder==GUID_NULL))){fprintf(stderr,"invalid CLSID\n");return 2;}
  HRESULT co=CoInitializeEx(nullptr,COINIT_MULTITHREADED);if(FAILED(co))return 1;
  HRESULT mf=MFStartup(MF_VERSION);if(FAILED(mf)){CoUninitialize();return 1;}
  int result=1;
  try{
    using namespace vcmf;Transform enc,dec;std::deque<ComPtr<IMFSample>> encoded;
    int encoded_count=0,decoded_count=0,keyframes=0,submitted_count=0,maximum_in_flight=0;uint64_t encoded_bytes=0;
    double mae_sum=0,latency_sum=0,latency_max=0;
    std::ofstream hashes;if(!log.empty()){hashes.open(log,std::ios::binary);if(!hashes)throw std::runtime_error("cannot open hashlog");}
    std::vector<std::vector<uint8_t>> originals(frames);
    std::vector<std::chrono::steady_clock::time_point> submitted(frames);
    const LONGLONG duration=10000000LL/stream.fps;GUID subtype=codec=="h264"?MFVideoFormat_H264:MFVideoFormat_HEVC;
    dec.open(subtype,stream,false,hardware_decoder,decoder,bitrate,[&](IMFSample* sample){
      LONGLONG time=0;check(sample->GetSampleTime(&time),"decoded timestamp");
      int frame=(int)(time/duration);
      if(time<0 || time%duration || frame!=decoded_count || frame>=frames)throw std::runtime_error("decoded frame order/timestamp mismatch");
      auto bytes=dec.packed_nv12(sample);double error=0;
      for(size_t i=0;i<stream.y_bytes();i++)error+=std::abs((int)bytes[i]-(int)originals[frame][i]);
      error/=stream.y_bytes();mae_sum+=error;
      double latency=std::chrono::duration<double,std::milli>(std::chrono::steady_clock::now()-submitted[frame]).count();
      latency_sum+=latency;latency_max=std::max(latency_max,latency);
      char source_hash[17],decoded_hash[17];vc_fnv1a64_hex(vc_fnv1a64(originals[frame].data(),originals[frame].size()),source_hash);
      vc_fnv1a64_hex(vc_fnv1a64(bytes.data(),bytes.size()),decoded_hash);
      if(hashes)hashes<<"{\"frame_id\":"<<frame<<",\"source_hash\":\""<<source_hash
        <<"\",\"decoded_hash\":\""<<decoded_hash<<"\",\"luma_mae\":"<<error<<",\"roundtrip_ms\":"<<latency<<"}\n";
      decoded_count++;
      originals[frame].clear();originals[frame].shrink_to_fit();
    },wide(decoder_name));
    enc.open(subtype,stream,true,hardware_encoder,encoder,bitrate,[&](IMFSample* sample){
      DWORD length=0;check(sample->GetTotalLength(&length),"encoded length");if(!length)throw std::runtime_error("empty access unit");
      UINT32 clean=0;sample->GetUINT32(MFSampleExtension_CleanPoint,&clean);keyframes+=clean!=0;
      LONGLONG timestamp=0;check(sample->GetSampleTime(&timestamp),"encoded timestamp");
      if((timestamp==0 || timestamp==(frames/2)*duration) && !clean)throw std::runtime_error("forced IDR was not marked clean point");
      ComPtr<IMFMediaBuffer> buffer;check(sample->ConvertToContiguousBuffer(&buffer),"encoded buffer");
      BYTE* p=nullptr;DWORD n=0;check(buffer->Lock(&p,nullptr,&n),"encoded Lock");
      // A new byte-only sample proves the decoder does not depend on the encoder's sample object.
      ComPtr<IMFSample> copy;
      try{copy=vcmf::sample(p,n,timestamp,duration);}catch(...){buffer->Unlock();throw;}
      buffer->Unlock();if(clean)copy->SetUINT32(MFSampleExtension_CleanPoint,TRUE);
      encoded_count++;encoded_bytes+=length;encoded.push_back(copy);
    },wide(encoder_name));
    for(int frame=0;frame<frames;frame++){
      originals[frame].resize(stream.frame_bytes());
      vc_gen_motion(frame,originals[frame].data(),originals[frame].data()+stream.y_bytes(),stream.width,stream.height);
      auto input=sample(originals[frame].data(),originals[frame].size(),frame*duration,duration);
      submitted[frame]=std::chrono::steady_clock::now();
      if(frame==0 || frame==frames/2)enc.force_keyframe();
      enc.submit(input.Get());submitted_count++;
      maximum_in_flight=std::max(maximum_in_flight,submitted_count-decoded_count);
      auto deadline=std::chrono::steady_clock::now()+std::chrono::seconds(5);
      while(submitted_count-decoded_count>=in_flight){
        enc.pump();while(!encoded.empty()){dec.submit(encoded.front().Get());encoded.pop_front();}dec.pump();
        if(submitted_count-decoded_count<in_flight)break;
        if(std::chrono::steady_clock::now()>deadline)throw std::runtime_error("bounded in-flight timeout (5s), frame="+std::to_string(frame)+
          " encoded="+std::to_string(encoded_count)+" decoded="+std::to_string(decoded_count)+"; low latency not validated");
        Sleep(1);
      }
    }
    enc.drain();while(!encoded.empty()){dec.submit(encoded.front().Get());encoded.pop_front();}dec.drain();
    if(encoded_count!=frames || decoded_count!=frames || keyframes<1)throw std::runtime_error("encoded/decoded/keyframe counts mismatch");
    printf("{\"backend\":\"windows-media-foundation\",\"codec\":\"%s\",\"encoder_clsid\":%s,\"decoder_clsid\":%s,\"encoder_name\":%s,\"decoder_name\":%s,"
      "\"hardware_encoder\":%s,\"hardware_decoder\":%s,\"encoder_async\":%s,\"decoder_async\":%s,"
      "\"width\":%d,\"height\":%d,\"decoded_surface\":[%u,%u],\"display_origin\":[%u,%u],\"fps_metadata\":%d,\"bitrate\":%d,\"frames\":%d,\"encoded\":%d,\"decoded\":%d,\"keyframes\":%d,"
      "\"encoded_bytes\":%llu,\"b_picture_control_hr\":\"0x%08x\",\"gop_control_hr\":\"0x%08x\",\"in_flight_limit\":%d,\"max_in_flight\":%d,\"roundtrip_avg_ms\":%.6f,\"roundtrip_max_ms\":%.6f,\"luma_mae\":%.6f,"
      "\"pixel_equality_gate\":false,\"transport_tested\":false,\"product_enabled\":false,\"pass\":true}\n",
      codec.c_str(),quoted(encoder_id).c_str(),quoted(decoder_id).c_str(),quoted(encoder_name).c_str(),quoted(decoder_name).c_str(),hardware_encoder?"true":"false",hardware_decoder?"true":"false",
      enc.asynchronous()?"true":"false",dec.asynchronous()?"true":"false",stream.width,stream.height,
      dec.decoded_layout.width,dec.decoded_layout.height,dec.decoded_layout.x,dec.decoded_layout.y,stream.fps,bitrate,frames,
      encoded_count,decoded_count,keyframes,(unsigned long long)encoded_bytes,(unsigned)enc.b_picture_control,(unsigned)enc.gop_control,in_flight,maximum_in_flight,latency_sum/frames,latency_max,mae_sum/frames);
    result=0;
  }catch(const std::exception& error){fprintf(stderr,"MF roundtrip failed: %s\n",error.what());}
  MFShutdown();CoUninitialize();return result;
}
