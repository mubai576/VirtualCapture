#pragma once
#include <cstdint>
#include <cstddef>
#include <string>
#include <algorithm>
#include <cstdlib>
#include <cstdio>
#include <cerrno>
#include <climits>
#ifdef min
#undef min
#endif
#ifdef max
#undef max
#endif

// Session parameters travel over the control plane; the 28-byte wire header stays frozen.
struct VCStream {
  int width=640, height=640, fps=240;
  std::string transform="crop";
  size_t y_bytes() const { return (size_t)width*height; }
  size_t uv_bytes() const { return y_bytes()/2; }
  size_t frame_bytes() const { return y_bytes()+uv_bytes(); }
  bool valid() const {
    return width>=2 && height>=2 && width<=8192 && height<=8192 &&
      !(width&1) && !(height&1) && fps>=1 && fps<=1000 &&
      (transform=="crop" || transform=="fit" || transform=="fill" || transform=="stretch");
  }
};

// Integer nearest-neighbour sampling. Fit rectangles and fill source crops are chroma aligned.
struct VCTransform {
  uint32_t ox=0, oy=0, rw=0, rh=0, dx=0, dy=0, dw=0, dh=0, width=0, height=0;
  bool init(int sw,int sh,const VCStream& s) {
    if(!s.valid() || sw<2 || sh<2 || sw>16384 || sh>16384) return false;
    width=s.width; height=s.height; rw=sw; rh=sh; dw=width; dh=height;
    ox=oy=dx=dy=0;
    if(s.transform=="crop") {
      if(sw<s.width || sh<s.height) return false;
      ox=(sw-s.width)/2; oy=(sh-s.height)/2; rw=width; rh=height;
    } else if(s.transform=="fit") {
      if((uint64_t)sw*height > (uint64_t)sh*width)
        dh=std::max(2u,(uint32_t)((uint64_t)sh*width/sw)&~1u);
      else dw=std::max(2u,(uint32_t)((uint64_t)sw*height/sh)&~1u);
      dx=((width-dw)/2)&~1u; dy=((height-dh)/2)&~1u;
    } else if(s.transform=="fill") {
      if((uint64_t)sw*height > (uint64_t)sh*width)
        rw=std::max(2u,(uint32_t)((uint64_t)sh*width/height)&~1u);
      else rh=std::max(2u,(uint32_t)((uint64_t)sw*height/width)&~1u);
      ox=((sw-rw)/2)&~1u; oy=((sh-rh)/2)&~1u;
    }
    return true;
  }
  bool sample(uint32_t x,uint32_t y,uint32_t& sx,uint32_t& sy) const {
    if(x<dx || y<dy || x>=dx+dw || y>=dy+dh) return false;
    sx=ox+(x-dx)*rw/dw; sy=oy+(y-dy)*rh/dh; return true;
  }
};

static inline int vc_clip8(int v) { return std::max(0,std::min(255,v)); }
static inline void vc_bgrx_nv12(const uint8_t* src,size_t pitch,const VCTransform& t,uint8_t* Y,uint8_t* UV) {
  auto rgb=[&](unsigned x,unsigned y,int& r,int& g,int& b) {
    unsigned sx,sy;
    if(!t.sample(x,y,sx,sy)) { r=g=b=0; return; }
    const uint8_t* p=src+(size_t)sy*pitch+sx*4;
    r=p[2]; g=p[1]; b=p[0];
  };
  for(unsigned y=0;y<t.height;y++) for(unsigned x=0;x<t.width;x++) {
    int r,g,b; rgb(x,y,r,g,b); Y[(size_t)y*t.width+x]=(uint8_t)((77*r+150*g+29*b)>>8);
  }
  for(unsigned y=0;y<t.height;y+=2) for(unsigned x=0;x<t.width;x+=2) {
    int sr=0,sg=0,sb=0;
    for(unsigned yy=0;yy<2;yy++) for(unsigned xx=0;xx<2;xx++) {
      int r,g,b; rgb(x+xx,y+yy,r,g,b); sr+=r; sg+=g; sb+=b;
    }
    sr>>=2; sg>>=2; sb>>=2;
    size_t i=(size_t)(y/2)*t.width+x;
    UV[i]=(uint8_t)vc_clip8(((-43*sr-84*sg+127*sb)>>8)+128);
    UV[i+1]=(uint8_t)vc_clip8(((127*sr-106*sg-21*sb)>>8)+128);
  }
}

// Strict minimum spacing, no catch-up bursts after a late frame. First frame is free.
struct VCFrameLimit {
  uint64_t next=0, period;
  explicit VCFrameLimit(int fps):period((1000000000ull+fps-1)/fps) {}
  bool ready(uint64_t now) const { return now>=next; }
  void sent(uint64_t now) { next=now+period; }
};

static inline bool vc_parse_mac(const std::string& text,uint8_t* out) {
  if(text.size()!=17) return false;
  auto hex=[](char c)->int { if(c>='0'&&c<='9') return c-'0'; if(c>='a'&&c<='f') return c-'a'+10; if(c>='A'&&c<='F') return c-'A'+10; return -1; };
  for(int i=0;i<6;i++) {
    int a=hex(text[i*3]),b=hex(text[i*3+1]);
    if(a<0 || b<0 || (i<5 && text[i*3+2]!=':')) return false;
    out[i]=(uint8_t)(a*16+b);
  }
  return true;
}

static inline const char* vc_stream_capabilities() {
  return "{\"stream_revision\":2,\"codecs\":[\"raw\"],\"pixel_format\":\"nv12\",\"variable_resolution\":true,\"dda_fps_limit\":true,\"shm_version\":3,\"max_dimension\":8192}";
}

static inline long long vc_cli_number(const std::string& value) {
  errno=0; char* end=nullptr;
  long long n=std::strtoll(value.c_str(),&end,10);
  size_t first=(!value.empty() && value[0]=='-')?1:0;
  if(value.size()==first || value.find_first_not_of("0123456789",first)!=std::string::npos || errno==ERANGE || !end || *end){
    std::fprintf(stderr,"invalid integer: %s\n",value.c_str()); std::exit(2);
  }
  return n;
}
static inline int vc_cli_int(const std::string& value) {
  long long n=vc_cli_number(value);
  if(n<INT_MIN || n>INT_MAX){ std::fprintf(stderr,"integer out of range\n"); std::exit(2); }
  return (int)n;
}
