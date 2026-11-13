#pragma once
#include "stream.h"
#include <turbojpeg.h>
#include <algorithm>
#include <cstdint>
#include <cstring>
#include <functional>
#include <stdexcept>
#include <string>
#include <vector>

namespace vcjpeg {
constexpr const char* identity="turbojpeg:cpu";
class Codec {
  tjhandle handle=nullptr;
  VCStream stream;
  int quality=80;
  bool encode=false;
  std::vector<uint8_t> chroma,output;
  void check(int result,const char* operation)const{
    if(result<0)throw std::runtime_error(std::string(operation)+": "+tj3GetErrorStr(handle));
  }
public:
  Codec()=default;
  Codec(const Codec&)=delete;
  Codec& operator=(const Codec&)=delete;
  ~Codec(){if(handle)tj3Destroy(handle);}
  void open(const VCStream& format,bool encoder,int jpeg_quality,size_t max_jpeg){
    if(handle || !format.valid() || jpeg_quality<1 || jpeg_quality>100 || !max_jpeg)
      throw std::runtime_error("invalid explicit MJPEG configuration");
    stream=format;encode=encoder;quality=jpeg_quality;
    handle=tj3Init(encoder?TJINIT_COMPRESS:TJINIT_DECOMPRESS);
    if(!handle)throw std::runtime_error("TurboJPEG initialization failed");
    check(tj3Set(handle,TJPARAM_STOPONWARNING,1),"JPEG strict warnings");
    check(tj3Set(handle,TJPARAM_FASTDCT,1),"JPEG fast integer DCT");
    chroma.resize(stream.y_bytes()/2);
    if(encoder){
      check(tj3Set(handle,TJPARAM_QUALITY,quality),"JPEG quality");
      check(tj3Set(handle,TJPARAM_SUBSAMP,TJSAMP_420),"JPEG 4:2:0");
      check(tj3Set(handle,TJPARAM_NOREALLOC,1),"JPEG bounded output");
      size_t maximum=tj3JPEGBufSize(stream.width,stream.height,TJSAMP_420);
      if(!maximum)throw std::runtime_error("invalid JPEG buffer size");
      output.resize(std::min(max_jpeg,maximum));
    }else{
      check(tj3Set(handle,TJPARAM_MAXPIXELS,stream.width*stream.height),"JPEG pixel limit");
      check(tj3Set(handle,TJPARAM_MAXMEMORY,16),"JPEG intermediate memory limit");
      check(tj3Set(handle,TJPARAM_SCANLIMIT,1),"JPEG scan limit");
      output.resize(stream.frame_bytes());
    }
  }
  void compress(const uint8_t* nv12,size_t size,const std::function<void(const uint8_t*,size_t)>& deliver){
    if(!handle || !encode || size!=stream.frame_bytes())throw std::runtime_error("invalid JPEG NV12 input");
    size_t y=stream.y_bytes(),uv=y/4;
    for(size_t i=0;i<uv;i++){chroma[i]=nv12[y+2*i];chroma[uv+i]=nv12[y+2*i+1];}
    const unsigned char* planes[]={nv12,chroma.data(),chroma.data()+uv};
    const int strides[]={stream.width,stream.width/2,stream.width/2};
    auto* data=output.data();size_t bytes=output.size();
    check(tj3CompressFromYUVPlanes8(handle,planes,stream.width,strides,stream.height,&data,&bytes),"JPEG encode");
    if(data!=output.data() || bytes>output.size())throw std::runtime_error("JPEG exceeded fixed buffer");
    deliver(data,bytes);
  }
  void decompress_into(const uint8_t* jpeg,size_t size,uint8_t* destination,size_t capacity){
    if(!handle || encode || !destination || capacity!=stream.frame_bytes() || size<4 ||
       jpeg[0]!=0xff || jpeg[1]!=0xd8 || jpeg[size-2]!=0xff || jpeg[size-1]!=0xd9)
      throw std::runtime_error("invalid complete JPEG frame");
    check(tj3DecompressHeader(handle,jpeg,size),"JPEG header");
    if(tj3Get(handle,TJPARAM_JPEGWIDTH)!=stream.width || tj3Get(handle,TJPARAM_JPEGHEIGHT)!=stream.height ||
       tj3Get(handle,TJPARAM_SUBSAMP)!=TJSAMP_420 || tj3Get(handle,TJPARAM_COLORSPACE)!=TJCS_YCbCr ||
       tj3Get(handle,TJPARAM_PRECISION)!=8 || tj3Get(handle,TJPARAM_PROGRESSIVE)!=0 || tj3Get(handle,TJPARAM_ARITHMETIC)!=0)
      throw std::runtime_error("JPEG must be baseline 8-bit YCbCr420 at exact session size; no conversion fallback");
    size_t y=stream.y_bytes(),uv=y/4;
    unsigned char* planes[]={destination,chroma.data(),chroma.data()+uv};
    int strides[]={stream.width,stream.width/2,stream.width/2};
    check(tj3DecompressToYUVPlanes8(handle,jpeg,size,planes,strides),"JPEG decode");
    for(size_t i=0;i<uv;i++){destination[y+2*i]=chroma[i];destination[y+2*i+1]=chroma[uv+i];}
  }
  const std::vector<uint8_t>& decompress(const uint8_t* jpeg,size_t size){
    decompress_into(jpeg,size,output.data(),output.size());
    return output;
  }
  std::string settings()const{
    return "{\"family\":\"turbojpeg\",\"identity\":\"turbojpeg:cpu\",\"version\":\"3.1.2\",\"quality\":"+std::to_string(quality)+
      ",\"width\":"+std::to_string(stream.width)+",\"height\":"+std::to_string(stream.height)+",\"fps\":"+std::to_string(stream.fps)+
      ",\"subsampling\":\"420\",\"dct\":\"fast_integer\",\"rate_control\":\"constant_quality\",\"target_bitrate_applied\":false,\"encoder_in_flight\":1,\"config_submitted\":true}";
  }
};

inline int selftest(){
  try{
    for(const auto& size:std::vector<std::pair<int,int>>{{2,2},{418,282},{640,640}}){
      VCStream format;format.width=size.first;format.height=size.second;
      Codec encoder,decoder;encoder.open(format,true,80,8*1024*1024);decoder.open(format,false,80,8*1024*1024);
      std::vector<uint8_t> input(format.frame_bytes(),96),jpeg;
      for(size_t i=format.y_bytes();i<input.size();i+=2){input[i]=32;input[i+1]=208;}
      for(int repeat=0;repeat<3;repeat++){
        encoder.compress(input.data(),input.size(),[&](const uint8_t* bytes,size_t n){jpeg.assign(bytes,bytes+n);});
        const auto& result=decoder.decompress(jpeg.data(),jpeg.size());
        if(result.size()!=input.size())throw std::runtime_error("JPEG NV12 size mismatch");
        for(size_t i=0;i<input.size();i++)if(abs((int)input[i]-(int)result[i])>2)throw std::runtime_error("JPEG NV12 plane/range mismatch");
        std::vector<uint8_t> direct(input.size()+32,0xa5);
        decoder.decompress_into(jpeg.data(),jpeg.size(),direct.data()+16,input.size());
        if(!std::equal(result.begin(),result.end(),direct.begin()+16) ||
           !std::all_of(direct.begin(),direct.begin()+16,[](uint8_t x){return x==0xa5;}) ||
           !std::all_of(direct.end()-16,direct.end(),[](uint8_t x){return x==0xa5;}))
          throw std::runtime_error("JPEG direct destination or guards mismatch");
      }
      auto must_fail=[&](const std::function<void()>& f){bool failed=false;try{f();}catch(const std::exception&){failed=true;}if(!failed)throw std::runtime_error("JPEG invalid input accepted");};
      must_fail([&]{decoder.decompress(jpeg.data(),jpeg.size()-1);});
      must_fail([&]{decoder.decompress_into(jpeg.data(),jpeg.size(),input.data(),input.size()-1);});
      must_fail([&]{decoder.decompress_into(jpeg.data(),jpeg.size(),nullptr,input.size());});
      auto broken=jpeg;broken[0]=0;must_fail([&]{decoder.decompress(broken.data(),broken.size());});
      VCStream wrong=format;wrong.width+=2;Codec mismatch;mismatch.open(wrong,false,80,8*1024*1024);
      must_fail([&]{mismatch.decompress(jpeg.data(),jpeg.size());});
      Codec limited;limited.open(format,true,80,32);
      must_fail([&]{limited.compress(input.data(),input.size(),[](const uint8_t*,size_t){});});
    }
    puts("PASS: MJPEG NV12 planes/range, tiny and non-MCU sizes, repeated frames, truncated/corrupt/wrong size, bounded output");return 0;
  }catch(const std::exception& e){fprintf(stderr,"MJPEG selftest failed: %s\n",e.what());return 1;}
}
}
