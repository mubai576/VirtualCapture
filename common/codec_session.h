#pragma once
#include "access_unit.h"
#include <cstdio>
#include <string>
#include <stdexcept>
#ifdef VC_HAS_MJPEG
#include "mjpeg_codec.h"
#endif

// H.264/H.265 were removed on 2026-11-13; only the independent-frame MJPEG path remains.
struct VCCodec {
  std::string codec="raw",backend;
  int hardware=0,bitrate=20000000,max_bitrate=80000000,gop=1,in_flight=2,max_au=8*1024*1024;
  bool specified=false;
  int jpeg_quality=80;
  bool jpeg_specified=false;
  bool mjpeg()const{return codec=="mjpeg";}
  bool option(const std::string& key,const std::string& value){
    if(key=="--codec"){codec=value;return true;}
    if(key=="--codec-backend")backend=value;
    else if(key=="--codec-hardware")hardware=vc_cli_int(value);
    else if(key=="--bitrate")bitrate=vc_cli_int(value);
    else if(key=="--max-bitrate")max_bitrate=vc_cli_int(value);
    else if(key=="--gop")gop=vc_cli_int(value);
    else if(key=="--decode-in-flight")in_flight=vc_cli_int(value);
    else if(key=="--max-au-bytes")max_au=vc_cli_int(value);
    else if(key=="--jpeg-quality"){jpeg_quality=vc_cli_int(value);jpeg_specified=true;}
    else return false;
    specified=true;return true;
  }
  bool encoded()const{return codec!="raw";}
  int id()const{return codec=="mjpeg"?3:0;}
  bool valid(int mtu,bool encoder=true)const{
    (void)encoder;
    if(!encoded())return !specified;
    const bool identity=backend=="turbojpeg:cpu" && hardware==0 && gop==1;
    return mjpeg() && identity && (!jpeg_specified || mjpeg()) && jpeg_quality>=1 && jpeg_quality<=100 &&
      bitrate>=100000 && max_bitrate>=bitrate && max_bitrate<=2000000000 &&
      in_flight>=1 && in_flight<=8 && max_au>=1024 && max_au<=16*1024*1024 && mtu>42 &&
      ((size_t)max_au+mtu-43)/(mtu-42)<=65535;
  }
};
inline const char* vc_codec_capabilities(){
  return "{\"stream_revision\":2,\"codec_revision\":1,"
#ifdef VC_HAS_MJPEG
    "\"mjpeg_revision\":1,"
#endif
    "\"codecs\":[\"raw\""
#ifdef VC_HAS_MJPEG
    ",\"mjpeg\""
#endif
    "],\"pixel_format\":\"nv12\",\"variable_resolution\":true,\"dda_fps_limit\":true,\"shm_version\":3,\"max_dimension\":8192}";
}
inline int vc_codec_backends(bool /*encoder*/){
#ifdef VC_HAS_MJPEG
  puts("{\"revision\":1,\"status\":\"candidates_only\",\"backends\":["
       "{\"codec\":\"mjpeg\",\"identity\":\"turbojpeg:cpu\",\"name\":\"libjpeg-turbo 3.1.2 CPU SIMD\","
       "\"family\":\"turbojpeg\",\"hardware\":false,\"frames_tested\":0}]}");
#else
  puts("{\"revision\":1,\"status\":\"candidates_only\",\"backends\":[]}");
#endif
  return 0;
}
inline int vc_codec_check(const VCCodec& options,const VCStream& stream,bool encoder){
  try{
    if(!options.mjpeg())throw std::runtime_error("only MJPEG is available; no fallback");
#ifdef VC_HAS_MJPEG
    vcjpeg::Codec transform;transform.open(stream,encoder,options.jpeg_quality,options.max_au-vcau::header_bytes);
    printf("{\"revision\":1,\"configured\":true,\"frames_tested\":0,\"stream_validated\":false,\"mjpeg_revision\":1,\"encoder_configuration\":%s}\n",transform.settings().c_str());
    return 0;
#else
    throw std::runtime_error("MJPEG codec was not built; no fallback");
#endif
  }catch(const std::exception& e){fprintf(stderr,"codec configuration failed (no fallback): %s\n",e.what());return 1;}
}
