#pragma once
#include "../common/codec_session.h"

// Only the independent-frame MJPEG decoder remains; every frame decodes without a GOP.
class VCDecoder {
#ifdef VC_HAS_MJPEG
  vcjpeg::Codec jpeg;
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
#ifdef VC_HAS_MJPEG
    if(!options.mjpeg())throw std::runtime_error("only MJPEG decoding is available; no fallback");
    jpeg.open(stream,false,options.jpeg_quality,options.max_au-vcau::header_bytes);
#else
    (void)options;(void)stream;
    throw std::runtime_error("MJPEG decoder was not built; no fallback");
#endif
  }
  void submit(const uint8_t* data,size_t size,LONGLONG stamp,LONGLONG /*duration*/,bool key){
#ifdef VC_HAS_MJPEG
    if(!key)throw std::runtime_error("MJPEG frame must be independently decodable");
    if(destination){
      auto* bytes=destination(frame_bytes);jpeg.decompress_into(data,size,bytes,frame_bytes);deliver(bytes,frame_bytes,stamp);
    }else{
      const auto& bytes=jpeg.decompress(data,size);deliver(bytes.data(),bytes.size(),stamp);
    }
#else
    (void)data;(void)size;(void)stamp;(void)key;
    throw std::runtime_error("MJPEG decoder was not built; no fallback");
#endif
  }
  void pump(){}
  void drain(){}
};
