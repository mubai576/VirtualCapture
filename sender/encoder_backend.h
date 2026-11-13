#pragma once
#include "../common/codec_session.h"
#ifdef VC_HAS_PYROWAVE
#include "../common/pyrowave_codec.h"
#endif

// Independent-frame dev codecs only; the transport sees bytes/timestamps.
class VCEncoder {
#ifdef VC_HAS_MJPEG
  vcjpeg::Codec jpeg;
#endif
#ifdef VC_HAS_PYROWAVE
  vcpyro::Codec pyro;
#endif
  const uint8_t* input=nullptr;size_t input_size=0;LONGLONG input_stamp=0;
  std::function<void(const uint8_t*,size_t,LONGLONG,bool)> deliver;
public:
  void open(const VCCodec& options,const VCStream& stream,
            std::function<void(const uint8_t*,size_t,LONGLONG,bool)> output){
    deliver=std::move(output);
#ifdef VC_HAS_PYROWAVE
    if(options.pyrowave()){
      const size_t budget=std::min((size_t)(options.max_bitrate/8/std::max(1,stream.fps)),
                                   (size_t)(options.max_au-vcau::header_bytes));
      pyro.open(stream,true,budget);return;
    }
#endif
#ifdef VC_HAS_MJPEG
    if(options.mjpeg()){
      jpeg.open(stream,true,options.jpeg_quality,options.max_au-vcau::header_bytes);return;
    }
#endif
    (void)options;(void)stream;
    throw std::runtime_error("requested encoder was not built; no fallback");
  }
  void submit(const uint8_t* data,size_t size,LONGLONG stamp,LONGLONG /*duration*/){
    if(input)throw std::runtime_error("only one input may be in flight");
    input=data;input_size=size;input_stamp=stamp;
  }
  void pump(){
    if(!input)return;
    const uint8_t* frame=input;size_t size=input_size;LONGLONG stamp=input_stamp;
    input=nullptr;
#ifdef VC_HAS_PYROWAVE
    if(pyro.budget()){
      size_t bytes=0;const uint8_t* au=pyro.encode(frame,size,bytes);
      deliver(au,bytes,stamp,true);return;
    }
#endif
#ifdef VC_HAS_MJPEG
    jpeg.compress(frame,size,[this,stamp](const uint8_t* data,size_t n){deliver(data,n,stamp,true);});
    return;
#endif
    throw std::runtime_error("no encoder backend is open");
  }
  void wait_for_event(){}
  void drain(){pump();}
  std::string quality_json()const{return "null";}
  std::string settings()const{
#ifdef VC_HAS_PYROWAVE
    if(pyro.budget())return pyro.settings();
#endif
#ifdef VC_HAS_MJPEG
    return jpeg.settings();
#else
    return "{\"family\":\"none\"}";
#endif
  }
};
