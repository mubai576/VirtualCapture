#pragma once
#include "../common/codec_session.h"

// Only the independent-frame MJPEG encoder remains; the transport sees bytes/timestamps.
class VCEncoder {
#ifdef VC_HAS_MJPEG
  vcjpeg::Codec jpeg;
  const uint8_t* jpeg_input=nullptr;size_t jpeg_size=0;LONGLONG jpeg_stamp=0;
  std::function<void(const uint8_t*,size_t,LONGLONG,bool)> jpeg_deliver;
#endif
public:
  void open(const VCCodec& options,const VCStream& stream,
            std::function<void(const uint8_t*,size_t,LONGLONG,bool)> output){
#ifdef VC_HAS_MJPEG
    if(!options.mjpeg())throw std::runtime_error("only MJPEG encoding is available; no fallback");
    jpeg.open(stream,true,options.jpeg_quality,options.max_au-vcau::header_bytes);jpeg_deliver=std::move(output);return;
#else
    (void)options;(void)stream;(void)output;
    throw std::runtime_error("MJPEG encoder was not built; no fallback");
#endif
  }
  void submit(const uint8_t* data,size_t size,LONGLONG stamp,LONGLONG /*duration*/){
#ifdef VC_HAS_MJPEG
    if(jpeg_input)throw std::runtime_error("MJPEG only allows one input in flight");
    jpeg_input=data;jpeg_size=size;jpeg_stamp=stamp;
#else
    (void)data;(void)size;(void)stamp;
    throw std::runtime_error("MJPEG encoder was not built; no fallback");
#endif
  }
  void pump(){
#ifdef VC_HAS_MJPEG
    if(jpeg_input){jpeg.compress(jpeg_input,jpeg_size,[this](const uint8_t* data,size_t size){jpeg_deliver(data,size,jpeg_stamp,true);});jpeg_input=nullptr;}
#endif
  }
  void wait_for_event(){}
  void drain(){pump();}
  std::string quality_json()const{return "null";}
  std::string settings()const{
#ifdef VC_HAS_MJPEG
    return jpeg.settings();
#else
    return "{\"family\":\"none\"}";
#endif
  }
};
