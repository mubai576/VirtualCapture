#pragma once
#include "../common/codec_session.h"
#ifdef VC_HAS_PYROWAVE
#include "../common/pyrowave_codec.h"
#endif

// Independent-frame dev codecs only; every frame decodes without a GOP.
class VCDecoder {
#ifdef VC_HAS_MJPEG
  vcjpeg::Codec jpeg;
#endif
#ifdef VC_HAS_PYROWAVE
  vcpyro::Codec pyro;
  bool pyro_open=false;
  std::vector<uint8_t> pyro_frame;
#endif
  std::function<void(const uint8_t*,size_t,LONGLONG)> deliver;
  std::function<uint8_t*(size_t)> destination;
  size_t frame_bytes=0;
public:
  void open(const VCCodec& options,const VCStream& stream,
            std::function<void(const uint8_t*,size_t,LONGLONG)> output,
            std::function<uint8_t*(size_t)> acquire={}){
    deliver=std::move(output);
    destination=std::move(acquire);frame_bytes=stream.frame_bytes();
#ifdef VC_HAS_PYROWAVE
    if(options.pyrowave()){pyro.open(stream,false,0);pyro_open=true;pyro_frame.resize(frame_bytes);return;}
#endif
#ifdef VC_HAS_MJPEG
    if(options.mjpeg()){jpeg.open(stream,false,options.jpeg_quality,options.max_au-vcau::header_bytes);return;}
#endif
    (void)options;(void)stream;
    throw std::runtime_error("requested decoder was not built; no fallback");
  }
  void submit(const uint8_t* data,size_t size,LONGLONG stamp,LONGLONG /*duration*/,bool key){
    if(!key)throw std::runtime_error("encoded frame must be independently decodable");
#ifdef VC_HAS_PYROWAVE
    if(pyro_open){
      if(destination){
        auto* bytes=destination(frame_bytes);pyro.decode(data,size,bytes,frame_bytes);deliver(bytes,frame_bytes,stamp);
      }else{
        pyro.decode(data,size,pyro_frame.data(),pyro_frame.size());deliver(pyro_frame.data(),pyro_frame.size(),stamp);
      }
      return;
    }
#endif
#ifdef VC_HAS_MJPEG
    if(destination){
      auto* bytes=destination(frame_bytes);jpeg.decompress_into(data,size,bytes,frame_bytes);deliver(bytes,frame_bytes,stamp);
    }else{
      const auto& bytes=jpeg.decompress(data,size);deliver(bytes.data(),bytes.size(),stamp);
    }
    return;
#endif
    throw std::runtime_error("no decoder backend is open");
  }
  // Soft stream reset: keep the device, drop codec-internal state.
  void reset(){
#ifdef VC_HAS_PYROWAVE
    if(pyro_open){pyro.reset_decoder();return;}
#endif
  }
  void pump(){}
  void drain(){}
};
