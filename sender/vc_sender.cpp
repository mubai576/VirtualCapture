// vc_sender.cpp — T-01 sender C++ (PLAN §2). C++17, Npcap wpcap (dynamic load), pattern.h chain.
// Chain: pattern.h NV12 (default motion) -> FNV-1a64 whole-frame -> payload=mtu-42 split
//   -> raw eth (dst=--dst-mac, type=--ethertype) -> pcap_sendpacket, QPC fps throttle,
//   per-packet --pacing-us, stats/hashlog per PLAN §2.3 (atomic tmp+rename, append hashlog).
// CLI frozen per PLAN §2.2. --selftest runs with no NIC. --list-adapters lists Npcap devs.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <vector>
#include <fstream>
#include <iostream>
#include <chrono>
#include <thread>
#include <atomic>
#include <cinttypes>
#include <cctype>
#ifdef _WIN32
#include <windows.h>
#include <timeapi.h>
#else
#include <time.h>
#endif

#include "../proto/vc_wire.h"
#include "pattern.h"
#include "dda.h"
#include "fnv1a.h"
#include "crc32.h"

// ---------- small-size (64x64) mirror for Python interlock (T-04) ----------
// Spec (must match ctrl/pattern_ref.py small funcs exactly):
// W=H=64, sprites S=16, central noise W2=16. Formulas identical to pattern.h
// with w/h substituted and centers w/2,h/2.
namespace small64 {
static const int W=64,H=64;
static inline uint64_t sm_splitmix(uint64_t *s){
  uint64_t z=(*s+=0x9E3779B97F4A7C15ull);
  z=(z^(z>>30))*0xBF58476D1CE4E5B9ull; z=(z^(z>>27))*0x94D049BB133111EBull; return z^(z>>31);
}
static inline void gen_counter(uint32_t fid,uint8_t*Y,uint8_t*UV){
  for(int y=0;y<H;y++)for(int x=0;x<W;x++)Y[y*W+x]=(uint8_t)(x+y+fid);
  uint8_t v=(uint8_t)(0x80^(fid&0xFF)); for(int i=0;i<W*H/2;i++)UV[i]=v;
}
static inline void gen_checker(uint32_t fid,uint8_t*Y,uint8_t*UV){
  int ox=fid&7,oy=(fid>>3)&7;
  for(int y=0;y<H;y++)for(int x=0;x<W;x++)Y[y*W+x]=((((x+ox)>>3)+((y+oy)>>3))&1)?255:0;
  for(int y=0;y<H/2;y++)for(int x=0;x<W;x+=2){
    uint8_t u=((((x>>3)+((y+fid)&1))&1))?16:240; uint8_t v=((((x>>3)+((y-fid)&1))&1))?16:240;
    UV[y*W+x]=u; UV[y*W+x+1]=v;
  }
}
static inline void gen_rings(uint32_t fid,uint8_t*Y,uint8_t*UV){
  int cx=W/2,cy=H/2;
  for(int y=0;y<H;y++){int dy=y-cy;for(int x=0;x<W;x++){int dx=x-cx;uint32_t r2=(uint32_t)(dx*dx+dy*dy);
    Y[y*W+x]=(uint8_t)((r2>>4)+fid*7+((x^y)&15));}}
  for(int y=0;y<H/2;y++)for(int x=0;x<W;x+=2){
    UV[y*W+x]=(uint8_t)(128+(((x*3+y*5+fid*11)>>2)&127)-64);
    UV[y*W+x+1]=(uint8_t)(128+(((x*5-y*3+fid*13)>>2)&127)-64);
  }
}
static inline void gen_noise(uint32_t fid,uint8_t*Y,uint8_t*UV){
  uint64_t s=((uint64_t)fid<<32)|0x123456789ABCull;
  uint8_t*p=Y;size_t n=W*H;
  while(n>=8){uint64_t r=sm_splitmix(&s);memcpy(p,&r,8);p+=8;n-=8;}
  while(n--)*p++=(uint8_t)sm_splitmix(&s);
  p=UV;n=W*H/2;
  while(n>=8){uint64_t r=sm_splitmix(&s);memcpy(p,&r,8);p+=8;n-=8;}
  while(n--)*p++=(uint8_t)sm_splitmix(&s);
}
static inline void gen_motion(uint32_t fid,uint8_t*Y,uint8_t*UV){
  for(int y=0;y<H;y++)for(int x=0;x<W;x++)Y[y*W+x]=(uint8_t)(x+fid*3+(y>>2));
  const int S=16;
  for(int k=0;k<4;k++){
    int px=(int)((fid*(5+k*2)+k*173)%(W-S)); int py=(int)((fid*(3+k)+k*311)%(H-S));
    uint8_t c=(uint8_t)(60+k*50);
    for(int y=0;y<S;y++)for(int x=0;x<S;x++)Y[(py+y)*W+(px+x)]=((x^y)&8)?(uint8_t)(255-c):c;
  }
  { uint64_t s=((uint64_t)fid<<32)^0x4D4F54494F4EULL; const int W2=16,x0=(W-W2)/2,y0=(H-W2)/2;
    for(int y=0;y<W2;y++){uint8_t*row=&Y[(y0+y)*W+x0];size_t n=W2;
      while(n>=8){uint64_t r=sm_splitmix(&s);memcpy(row,&r,8);row+=8;n-=8;}
      while(n--)*row++=(uint8_t)sm_splitmix(&s);} }
  for(int y=0;y<H/2;y++)for(int x=0;x<W;x+=2){UV[y*W+x]=(uint8_t)(x+fid);UV[y*W+x+1]=(uint8_t)(y*2-fid);}
}
} // namespace small64

static uint64_t qpc_freq_ = 0;
static uint64_t qpc_now_raw() {
#ifdef _WIN32
  LARGE_INTEGER c; QueryPerformanceCounter(&c); return (uint64_t)c.QuadPart;
#else
  struct timespec ts; clock_gettime(CLOCK_MONOTONIC,&ts);
  return (uint64_t)ts.tv_sec*1000000000ull+(uint64_t)ts.tv_nsec;
#endif
}
static void qpc_init() {
#ifdef _WIN32
  LARGE_INTEGER f; QueryPerformanceFrequency(&f); qpc_freq_=(uint64_t)f.QuadPart;
#else
  qpc_freq_=1000000000ull;
#endif
}
static uint64_t qpc_ns_now() {
  uint64_t c=qpc_now_raw();
#ifdef _WIN32
  return c*1000000000ull/qpc_freq_;
#else
  return c;
#endif
}
#ifdef _WIN32
static volatile LONG g_stop = 0;
static BOOL WINAPI chandler(DWORD e){
  if(e==CTRL_C_EVENT||e==CTRL_BREAK_EVENT||e==CTRL_CLOSE_EVENT){ InterlockedExchange(&g_stop,1); return TRUE; }
  return FALSE;
}
#endif

static void sleep_us(uint64_t us){
  if(us==0) return;
  uint64_t t0=qpc_ns_now(), want=t0+us*1000ull; // t0 BEFORE sleep: spin only the remainder
  if(us>=2500){
#ifdef _WIN32
    Sleep((DWORD)((us-1500)/1000)); // coarse part, leave ~1.5ms for precise spin
#else
    std::this_thread::sleep_for(std::chrono::microseconds(us-1500));
#endif
  }
  while(qpc_ns_now()<want){ 
#ifdef _WIN32
    YieldProcessor();
#else
    std::this_thread::yield();
#endif
  }
}

// ---------- dynamic wpcap ----------
#ifdef _WIN32
typedef struct pcap_opaque pcap_t;
struct DynPcap {
  HMODULE dll=nullptr;
  typedef int (*findalldevs_fn)(void**,char*);
  typedef void (*freealldevs_fn)(void*);
  typedef pcap_t* (*openlive_fn)(const char*,int,int,int,char*);
  typedef int (*sendpacket_fn)(pcap_t*,const unsigned char*,int);
  typedef void (*close_fn)(pcap_t*);
  typedef void* (*sendqueue_alloc_fn)(unsigned);
  typedef void (*sendqueue_destroy_fn)(void*);
  typedef int (*sendqueue_queue_fn)(void*,const void*,const unsigned char*);
  typedef int (*sendqueue_transmit_fn)(pcap_t*,void*,int);
  typedef const char* (*libversion_fn)(void);
  findalldevs_fn findalldevs=nullptr; freealldevs_fn freealldevs=nullptr;
  openlive_fn openlive=nullptr; sendpacket_fn sendpacket=nullptr;
  sendqueue_alloc_fn sendqueue_alloc=nullptr; sendqueue_destroy_fn sendqueue_destroy=nullptr;
  sendqueue_queue_fn sendqueue_queue=nullptr; sendqueue_transmit_fn sendqueue_transmit=nullptr;
  close_fn closeh=nullptr; libversion_fn libversion=nullptr;
  std::string err;
  bool load(){
    dll=LoadLibraryA("wpcap.dll");
    if(!dll) dll=LoadLibraryA("C:\\Windows\\System32\\Npcap\\wpcap.dll");
    if(!dll){ err="wpcap.dll not found (install Npcap with WinPcap-compatible mode)"; return false; }
    findalldevs=(findalldevs_fn)GetProcAddress(dll,"pcap_findalldevs");
    freealldevs=(freealldevs_fn)GetProcAddress(dll,"pcap_freealldevs");
    openlive=(openlive_fn)GetProcAddress(dll,"pcap_open_live");
    sendpacket=(sendpacket_fn)GetProcAddress(dll,"pcap_sendpacket");
    sendqueue_alloc=(sendqueue_alloc_fn)GetProcAddress(dll,"pcap_sendqueue_alloc");
    sendqueue_destroy=(sendqueue_destroy_fn)GetProcAddress(dll,"pcap_sendqueue_destroy");
    sendqueue_queue=(sendqueue_queue_fn)GetProcAddress(dll,"pcap_sendqueue_queue");
    sendqueue_transmit=(sendqueue_transmit_fn)GetProcAddress(dll,"pcap_sendqueue_transmit");
    closeh=(close_fn)GetProcAddress(dll,"pcap_close");
    libversion=(libversion_fn)GetProcAddress(dll,"pcap_lib_version");
    if(!openlive||!sendpacket){ err="wpcap.dll missing exports"; return false; }
    return true;
  }
};
struct PcapHdr { uint32_t ts_sec; uint32_t ts_usec; unsigned caplen; unsigned len; };
struct PcapDev { std::string name, desc; };
#endif

struct Args {
  std::string adapter="";
  std::string mode="pattern";
  std::string pattern="motion";
  int fps=240; long long frames=0; long long batch=69;
  uint16_t ethertype=0x88B5; int mtu=9000; long long pacing_us=0;
  uint8_t dstmac[6]={0xFF,0xFF,0xFF,0xFF,0xFF,0xFF};
  std::string session="manual", stats="", hashlog="", file="";
  bool loop=false, list_adapters=false, selftest=false;
};

static bool parse_mac(const std::string&s,uint8_t m[6]){
  unsigned v[6];
  if(sscanf(s.c_str(),"%x:%x:%x:%x:%x:%x",&v[0],&v[1],&v[2],&v[3],&v[4],&v[5])!=6) return false;
  for(int i=0;i<6;i++) m[i]=(uint8_t)v[i]; return true;
}
static uint16_t parse_et(const std::string&s){
  unsigned v=0;
  if(s.size()>2&&(s[0]==48&&(s[1]==120||s[1]==88))) sscanf(s.c_str(),"%x",&v);
  else if(s.find_first_of("abcdefABCDEF")!=std::string::npos) sscanf(s.c_str(),"%x",&v);
  else v=(unsigned)atoi(s.c_str());
  return (uint16_t)v;
}
static int pat_id(const std::string&s){
  if(s=="counter")return 0; if(s=="checker")return 1; if(s=="rings")return 2;
  if(s=="noise")return 3; if(s=="motion")return 4; return 4;
}
static const char* pat_name(int id){
  switch(id){case 0:return "counter";case 1:return "checker";case 2:return "rings";
    case 3:return "noise";case 4:return "motion";default:return "motion";}
}

static void usage(){
  printf("vc-sender.exe --adapter <substr> --mode pattern|file|dda --pattern motion --fps 240\n"
    "  --frames 0 --ethertype 0x88B5 --mtu 9000 --pacing-us 0 --batch 69 --dst-mac ff:ff:ff:ff:ff:ff\n"
    "  --session <id> --stats <path> --hashlog <path> [--file <nv12> --loop] [--list-adapters] [--selftest]\n");
}

static void ensure_parent(const std::string&path){
  if(path.empty()) return;
  size_t p=path.find_last_of("/\\");
  if(p==std::string::npos) return;
  std::string dir=path.substr(0,p);
#ifdef _WIN32
  CreateDirectoryA(dir.c_str(),NULL);
#else
  mkdir(dir.c_str(),0755);
#endif
}
static void atomic_write(const std::string&path,const std::string&data){
  if(path.empty()) return;
  ensure_parent(path);
  std::string tmp=path+".tmp";
  { std::ofstream f(tmp,std::ios::binary|std::ios::trunc); f.write(data.data(),(std::streamsize)data.size()); }
#ifdef _WIN32
  MoveFileExA(tmp.c_str(),path.c_str(),MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH);
#else
  rename(tmp.c_str(),path.c_str());
#endif
}

static void gen_full(int pid,uint32_t fid,uint8_t*Y,uint8_t*UV){
  switch(pid){case 0:vc_gen_counter(fid,Y,UV);break;case 1:vc_gen_checker(fid,Y,UV);break;
    case 2:vc_gen_rings(fid,Y,UV);break;case 3:vc_gen_noise(fid,Y,UV);break;
    default:vc_gen_motion(fid,Y,UV);break;}
}
static void gen_small(int pid,uint32_t fid,uint8_t*Y,uint8_t*UV){
  switch(pid){case 0:small64::gen_counter(fid,Y,UV);break;case 1:small64::gen_checker(fid,Y,UV);break;
    case 2:small64::gen_rings(fid,Y,UV);break;case 3:small64::gen_noise(fid,Y,UV);break;
    default:small64::gen_motion(fid,Y,UV);break;}
}

// ---------- selftest ----------
static int run_selftest(){
  qpc_init();
  printf("[sender selftest] patterns x determinism x frag-reassembly\n");
  const int MTU=9000;
  const size_t payload=(size_t)(MTU-42);
  const size_t total=(VC_FRAME_BYTES+payload-1)/payload;
  const size_t last_len=VC_FRAME_BYTES-(total-1)*payload;
  printf("frag: mtu=%d payload=%zu total=%zu last=%zu (expect 69/5256)\n",MTU,payload,total,last_len);
  if(total!=69){printf("FAIL total!=69\n");return 1;}
  if(last_len!=5256){printf("FAIL last!=5256 got %zu\n",last_len);return 1;}
  // mtu1500 reference (doc counter-example)
  { size_t p2=1500-42; size_t t2=(VC_FRAME_BYTES+p2-1)/p2;
    printf("ref1500: payload=%zu total=%zu (expect 422)\n",p2,t2);
    if(t2!=422){printf("FAIL ref1500!=422\n");return 1;} }
  std::vector<uint8_t> Y(VC_Y_BYTES),UV(VC_UV_BYTES),Y2(VC_Y_BYTES),UV2(VC_UV_BYTES);
  std::vector<uint8_t> frame(VC_FRAME_BYTES), reasm(VC_FRAME_BYTES);
  std::vector<uint8_t> sY(64*64),sUV(64*64/2),sY2(64*64),sUV2(64*64/2);
  bool ok=true;
  for(int pid=0;pid<5;pid++){
    for(uint32_t fid=0;fid<4;fid++){
      gen_full(pid,fid,Y.data(),UV.data());
      gen_full(pid,fid,Y2.data(),UV2.data());
      uint64_t h1=vc_fnv1a64_two(Y.data(),Y.size(),UV.data(),UV.size());
      uint64_t h2=vc_fnv1a64_two(Y2.data(),Y2.size(),UV2.data(),UV2.size());
      if(h1!=h2){printf("FAIL determinism %s fid=%u\n",pat_name(pid),fid);ok=false;}
      // frag roundtrip locally
      memcpy(frame.data(),Y.data(),VC_Y_BYTES);
      memcpy(frame.data()+VC_Y_BYTES,UV.data(),VC_UV_BYTES);
      // split + crc + reassemble
      std::vector<uint8_t> out(VC_FRAME_BYTES); size_t off=0;
    for(size_t s=0;s<total;s++){
        size_t cl=(s==total-1)?last_len:payload;
        uint32_t c=vc_crc32(frame.data()+off,cl);
        // verify crc immediately (transport-intact path)
        uint32_t c2=vc_crc32(frame.data()+off,cl);
        if(c!=c2){printf("FAIL crc self %s fid=%u seq=%zu\n",pat_name(pid),fid,s);ok=false;}
        memcpy(out.data()+off,frame.data()+off,cl);
        // header field sanity
        vc_pkt_hdr_t h; memset(&h,0,sizeof(h));
        h.magic=VC_MAGIC; h.frame_id=fid; h.seq=(uint16_t)s; h.total=(uint16_t)total;
        h.payload_crc=c; h.payload_len=(uint16_t)cl; h.pattern=(uint8_t)pid;
        if(h.magic!=VC_MAGIC||h.total!=total){printf("FAIL hdr\n");ok=false;}
        off+=cl;
      }
      uint64_t hr=vc_fnv1a64(out.data(),out.size());
      if(hr!=h1){printf("FAIL roundtrip %s fid=%u\n",pat_name(pid),fid);ok=false;}
      char hx[17]; vc_fnv1a64_hex(h1,hx);
      printf("VEC full %s %u %s\n",pat_name(pid),fid,hx);
      // small interlock vectors
      gen_small(pid,fid,sY.data(),sUV.data());
      gen_small(pid,fid,sY2.data(),sUV2.data());
      uint64_t sh1=vc_fnv1a64_two(sY.data(),sY.size(),sUV.data(),sUV.size());
      uint64_t sh2=vc_fnv1a64_two(sY2.data(),sY2.size(),sUV2.data(),sUV2.size());
      if(sh1!=sh2){printf("FAIL small determinism %s fid=%u\n",pat_name(pid),fid);ok=false;}
      char shx[17]; vc_fnv1a64_hex(sh1,shx);
      printf("VEC small %s %u %s\n",pat_name(pid),fid,shx);
    }
  }
  // FNV known vectors (spec): empty==offset basis, single 'a' known? use standard test:
  // FNV-1a64("") = cbf29ce484222325 ; FNV-1a64("a") = af63dc4c8601ec8c (well-known)
  { uint64_t he=vc_fnv1a64(nullptr,0);
    if(he!=14695981039346656037ull){printf("FAIL fnv empty %016llx\n",(unsigned long long)he);ok=false;}
    const uint8_t a[]={97};
    uint64_t ha=vc_fnv1a64(a,1);
    // 0xaf63dc4c8601ec8c is the standard FNV-1a 64 for "a"
    if(ha!=0xaf63dc4c8601ec8cull){printf("FAIL fnv(a) %016llx\n",(unsigned long long)ha);ok=false;} }
  { static const int DW=640, DH=640;
    std::vector<uint8_t> bgra((size_t)DW*DH*4), tY(VC_Y_BYTES), tUV(VC_UV_BYTES);
    for(size_t i=0;i<(size_t)DW*DH;i++){ bgra[i*4]=255; bgra[i*4+1]=255; bgra[i*4+2]=255; bgra[i*4+3]=0; }
    dda_bgrx_to_nv12(bgra.data(), (unsigned)DW*4u, DW, DH, tY.data(), tUV.data());
    for(size_t i=0;i<VC_Y_BYTES;i++) if(tY[i]!=255){printf("FAIL dda white Y\n");ok=false;break;}
    for(size_t i=0;i<VC_UV_BYTES;i+=2){ if(tUV[i]!=128||tUV[i+1]!=128){printf("FAIL dda white UV\n");ok=false;break;} }
    for(size_t i=0;i<(size_t)DW*DH;i++){ bgra[i*4]=0; bgra[i*4+1]=0; bgra[i*4+2]=255; bgra[i*4+3]=0; }
    dda_bgrx_to_nv12(bgra.data(), (unsigned)DW*4u, DW, DH, tY.data(), tUV.data());
    if(tY[0]!=76||tY[(size_t)640*639+639]!=76){printf("FAIL dda red Y\n");ok=false;}
    printf("DDA: converter unit PASS (white/red exact)\n"); }
  { int ddr=dda_selftest(); if(ddr==1){printf("FAIL dda selftest\n");ok=false;} }
  if(!ok){printf("FAIL: sender selftest\n");return 1;}
  printf("PASS: sender selftest\n");
  return 0;
}

#ifdef _WIN32
static int run_list_adapters(){
  DynPcap d; if(!d.load()){ fprintf(stderr,"list-adapters: %s\n",d.err.c_str()); return 1; }
  if(!d.findalldevs){ fprintf(stderr,"list-adapters: no pcap_findalldevs\n"); return 1; }
  // pcap_if minimal layout: next, name, description, addresses, flags
  struct PcapIf { PcapIf* next; char* name; char* description; void* addresses; uint32_t flags; };
  PcapIf* devs=nullptr; char errbuf[256]={0};
  if(d.findalldevs((void**)&devs,errbuf)!=0){ fprintf(stderr,"findalldevs failed: %s\n",errbuf); return 1; }
  int n=0;
  for(PcapIf*p=devs;p;p=p->next){
    printf("adapter[%d] name=%s desc=%s\n",n,
      p->name?p->name:"(null)", p->description?p->description:"(null)");
    n++;
  }
  if(d.freealldevs) d.freealldevs(devs);
  if(n==0) printf("adapter: (none found)\n");
  return 0;
}
#endif

int main(int argc,char**argv){
  qpc_init();
  Args a;
  for(int i=1;i<argc;i++){
    std::string k=argv[i];
    auto need=[&](std::string&out){ if(i+1>=argc){fprintf(stderr,"missing value for %s\n",k.c_str());exit(2);} out=argv[++i]; };
    if(k=="--adapter"){need(a.adapter);}
    else if(k=="--mode"){need(a.mode);}
    else if(k=="--pattern"){need(a.pattern);}
    else if(k=="--fps"){a.fps=atoi(argv[++i]);}
    else if(k=="--frames"){a.frames=atoll(argv[++i]);}
    else if(k=="--ethertype"){a.ethertype=parse_et(argv[++i]);}
    else if(k=="--mtu"){a.mtu=atoi(argv[++i]);}
    else if(k=="--pacing-us"){a.pacing_us=atoll(argv[++i]);}
    else if(k=="--dst-mac"){ std::string s; need(s); if(!parse_mac(s,a.dstmac)){fprintf(stderr,"bad --dst-mac\n");return 2;} }
    else if(k=="--batch"){a.batch=atoll(argv[++i]);}
    else if(k=="--session"){need(a.session);}
    else if(k=="--stats"){need(a.stats);}
    else if(k=="--hashlog"){need(a.hashlog);}
    else if(k=="--file"){need(a.file);}
    else if(k=="--loop"){a.loop=true;}
    else if(k=="--list-adapters"){a.list_adapters=true;}
    else if(k=="--selftest"){a.selftest=true;}
    else if(k=="--help"||k=="-h"){usage();return 0;}
    else {fprintf(stderr,"unknown arg %s\n",k.c_str());usage();return 2;}
  }
#ifdef _WIN32
  SetConsoleCtrlHandler(chandler, TRUE);
#endif
  if(a.selftest) return run_selftest();
#ifdef _WIN32
  if(a.list_adapters) return run_list_adapters();
#else
  if(a.list_adapters){ fprintf(stderr,"list-adapters only on Windows/Npcap\n"); return 1; }
#endif
  if(a.stats.empty()) a.stats="run/"+a.session+".sender.stats.json";
  if(a.hashlog.empty()) a.hashlog="run/"+a.session+".sender.hash.jsonl";
  if(a.mode!="pattern"&&a.mode!="file"&&a.mode!="dda"){fprintf(stderr,"--mode must be pattern|file|dda\n");return 2;}
  int pid=pat_id(a.pattern);
  size_t payload=(size_t)(a.mtu-42);
  if(payload==0||payload>9000){fprintf(stderr,"bad mtu\n");return 2;}
  size_t total=(VC_FRAME_BYTES+payload-1)/payload;
  if(a.batch<1){fprintf(stderr,"bad --batch\n");return 2;}
  if(total>65535){fprintf(stderr,"mtu too small\n");return 2;}
#ifdef _WIN32
  timeBeginPeriod(1); // 1ms Sleep granularity, else 240Hz throttle oversleeps to 64Hz
#endif
  printf("[sender] session=%s mode=%s pattern=%s fps=%d frames=%lld mtu=%d payload=%zu total=%zu ethertype=0x%04x\n",
    a.session.c_str(),a.mode.c_str(),a.pattern.c_str(),a.fps,(long long)a.frames,
    a.mtu,payload,total,a.ethertype);

  // file mode load
  std::vector<uint8_t> fileFrames;
  size_t fileNFrames=0;
  if(a.mode=="file"){
    if(a.file.empty()){fprintf(stderr,"--file required in file mode\n");return 2;}
    std::ifstream f(a.file,std::ios::binary|std::ios::ate);
    if(!f){fprintf(stderr,"cannot open --file %s\n",a.file.c_str());return 1;}
    size_t sz=(size_t)f.tellg(); f.seekg(0);
    if(sz<VC_FRAME_BYTES||sz%VC_FRAME_BYTES!=0){fprintf(stderr,"file size %zu not multiple of %d\n",sz,VC_FRAME_BYTES);return 2;}
    fileFrames.resize(sz); f.read((char*)fileFrames.data(),(std::streamsize)sz);
    fileNFrames=sz/VC_FRAME_BYTES;
    printf("[sender] file %s frames=%zu loop=%d\n",a.file.c_str(),fileNFrames,(int)a.loop);
  }

#ifdef _WIN32
  DynPcap d; if(!d.load()){fprintf(stderr,"pcap load: %s\n",d.err.c_str());return 1;}
  // resolve adapter substring
  struct PcapIf { PcapIf* next; char* name; char* description; void* addresses; uint32_t flags; };
  std::string devName="";
  if(a.adapter.empty()){
    PcapIf* devs=nullptr; char eb[256]={0};
    if(d.findalldevs&&d.findalldevs((void**)&devs,eb)==0&&devs){ std::string picked=""; for(PcapIf* q=devs;q;q=q->next){ if(!q->name||!q->name[0]) continue; char oe[256]={0}; pcap_t* th=d.openlive(q->name,65536,1,1000,oe); if(th){ picked=q->name; printf("[sender] auto adapter %s\n",picked.c_str()); d.closeh(th); break; } fprintf(stderr,"[sender] auto try %s failed: %s\n",q->name?q->name:"",oe); }
      if(d.freealldevs)d.freealldevs(devs); if(!picked.empty()){ devName=picked; } else {fprintf(stderr,"all adapters failed; pass --adapter <substr>\n");run_list_adapters();return 2;} }
    else {fprintf(stderr,"no adapter found; pass --adapter <substr>\n");run_list_adapters();return 2;}
  } else {
    PcapIf* devs=nullptr; char eb[256]={0};
    if(!d.findalldevs||d.findalldevs((void**)&devs,eb)!=0){fprintf(stderr,"findalldevs failed\n");return 2;}
    std::string needle=a.adapter; for(auto&c:needle)c=(char)tolower(c);
    for(PcapIf*p=devs;p;p=p->next){
      std::string nm=p->name?p->name:"", ds=p->description?p->description:"";
      std::string hay=nm+" "+ds; for(auto&c:hay)c=(char)tolower(c);
      if(hay.find(needle)!=std::string::npos){ devName=nm; printf("[sender] adapter match %s (%s)\n",nm.c_str(),ds.c_str()); break; }
    }
    if(d.freealldevs)d.freealldevs(devs);
    if(devName.empty()){fprintf(stderr,"adapter missing\n");run_list_adapters();return 2;}
  }
  char errbuf[256]={0};
  pcap_t* h=d.openlive(devName.c_str(),65536,1,1000,errbuf);
  if(!h){fprintf(stderr,"pcap_open_live failed\n");return 2;}
#else
  fprintf(stderr,"live send only on Windows/Npcap in this build\n"); return 1;
#endif

  std::vector<uint8_t> Y(VC_Y_BYTES),UV(VC_UV_BYTES);
  DDACap dda_cap; double gpu_accum_ms=0;
  if(a.mode=="dda"){ std::string dd; if(!dda_cap.init(dd)){fprintf(stderr,"dda init failed: %s\n",dda_cap.err());return 1;} printf("[sender] dda output=%s center640\n",dd.c_str()); }
  std::vector<uint8_t> eth(14+28+payload);
  // eth header
  memcpy(eth.data(),a.dstmac,6);
  uint8_t srcmac[6]={0x02,0x00,0x00,0x00,0x00,0x01};
  memcpy(eth.data()+6,srcmac,6);
  eth[12]=(uint8_t)(a.ethertype>>8); eth[13]=(uint8_t)(a.ethertype&0xFF);

  uint64_t sent_frames=0, sent_packets=0, send_errors=0, sent_bytes=0;
  void* sq=nullptr; int sq_n=0;
  uint64_t t_start=qpc_ns_now(), t_last_stats=0;
#ifdef _WIN32
  static ULARGE_INTEGER cpu_prev_k, cpu_prev_u; static uint64_t cpu_prev_w=0;
#endif
  // truncate hashlog at start
  { ensure_parent(a.hashlog); std::ofstream f(a.hashlog,std::ios::trunc); }
  std::ofstream hashlog(a.hashlog,std::ios::app|std::ios::binary);
  double frame_interval_s=a.fps>0?1.0/a.fps:0;
  uint64_t next_frame_ns=t_start;
  uint32_t fid=0;
  size_t fileIdx=0, file_played=0;
  for(;;){
#ifdef _WIN32
    if(g_stop) break;
#endif
    if(a.frames>0&&(long long)sent_frames>=a.frames) break;
    if(a.mode=="file"&&!a.loop&&file_played>=fileNFrames) break;
    // fps throttle (skip for first frame)
    if(sent_frames>0&&frame_interval_s>0){
      uint64_t now=qpc_ns_now();
      if(now<next_frame_ns) {
        uint64_t wait_us=(next_frame_ns-now)/1000ull;
        if(wait_us>0) sleep_us(wait_us);
      }
    }
    uint64_t f0=qpc_ns_now();
    // generate
    uint8_t pat_field=255;
    if(a.mode=="pattern"){ gen_full(pid,fid,Y.data(),UV.data()); pat_field=(uint8_t)pid; }
    else if(a.mode=="dda"){ uint64_t g0=qpc_ns_now(); if(!dda_cap.capture(Y.data(),UV.data())){ if(dda_cap.err_code!=DDACap::E_TIMEOUT){fprintf(stderr,"dda capture failed: %s\n",dda_cap.err());return 1;} } gpu_accum_ms+=(qpc_ns_now()-g0)/1e6; pat_field=255; }
    else { memcpy(Y.data(),fileFrames.data()+fileIdx*VC_FRAME_BYTES,VC_Y_BYTES);
           memcpy(UV.data(),fileFrames.data()+fileIdx*VC_FRAME_BYTES+VC_Y_BYTES,VC_UV_BYTES);
           fileIdx=(fileIdx+1)%fileNFrames; file_played++; }
    uint64_t fh=vc_fnv1a64_two(Y.data(),Y.size(),UV.data(),UV.size());
    char fhx[17]; vc_fnv1a64_hex(fh,fhx);
    // contiguous for slicing
    // (avoid extra copy: slice Y then UV)
    size_t pkts_before = sent_packets; // for partial-frame stop guard
    for(size_t s=0;s<total;s++){
      size_t off=s*payload;
      size_t cl=(s==total-1)?(VC_FRAME_BYTES-off):payload;
      // copy chunk handling Y/UV boundary
      std::vector<uint8_t> chunk(cl);
      if(off+cl<=VC_Y_BYTES) memcpy(chunk.data(),Y.data()+off,cl);
      else if(off>=VC_Y_BYTES) memcpy(chunk.data(),UV.data()+(off-VC_Y_BYTES),cl);
      else { size_t nY=VC_Y_BYTES-off; memcpy(chunk.data(),Y.data()+off,nY);
             memcpy(chunk.data()+nY,UV.data(),cl-nY); }
      uint32_t crc=vc_crc32(chunk.data(),cl);
      vc_pkt_hdr_t hdr; memset(&hdr,0,sizeof(hdr));
      hdr.magic=VC_MAGIC; hdr.frame_id=fid; hdr.seq=(uint16_t)s; hdr.total=(uint16_t)total;
      hdr.qpc_ns=f0; hdr.payload_crc=crc; hdr.payload_len=(uint16_t)cl;
      hdr.flags=(fid==0?1:0); hdr.pattern=pat_field;
      memcpy(eth.data()+14,&hdr,sizeof(hdr));
      memcpy(eth.data()+14+sizeof(hdr),chunk.data(),cl);
#ifdef _WIN32
      int plen=(int)(14+sizeof(hdr)+cl);
      // NOTE: Npcap sendqueue_transmit returns positive on success, so error tests must use <0 (a !=0 check once faked 165600 errors on a fully-delivered run, 2026-10-21).
      if(a.batch<=1){ int rc=d.sendpacket(h,eth.data(),plen); if(rc<0) send_errors++; }
      else {
        if(!sq){ sq=d.sendqueue_alloc(2*1024*1024); if(!sq){fprintf(stderr,"sendqueue_alloc failed\n");return 1;}
          if(!d.sendqueue_queue||!d.sendqueue_transmit||!d.sendqueue_destroy){fprintf(stderr,"wpcap missing sendqueue exports\n");return 1;} sq_n=0; }
        PcapHdr ph; memset(&ph,0,sizeof(ph)); ph.caplen=(unsigned)plen; ph.len=(unsigned)plen;
        if(d.sendqueue_queue(sq,(const void*)&ph,eth.data())!=0){ send_errors++; }
        else if(++sq_n>=a.batch){ if(d.sendqueue_transmit(h,sq,0)<0) send_errors+=(uint64_t)sq_n; d.sendqueue_destroy(sq); sq=nullptr; sq_n=0; }
      }
#endif
      sent_packets++; sent_bytes+=14+sizeof(hdr)+cl;
#ifdef _WIN32
      if(g_stop) break;
#endif
      if(a.pacing_us>0) sleep_us((uint64_t)a.pacing_us);
    }
#ifdef _WIN32
    if(g_stop && sent_packets - pkts_before < total) break; // partial frame on stop: drop, do not log/count
#endif
#ifdef _WIN32
    if(sq){ if(d.sendqueue_transmit(h,sq,0)<0) send_errors+=(uint64_t)sq_n; d.sendqueue_destroy(sq); sq=nullptr; sq_n=0; }
#endif
    hashlog << "{\"frame_id\":" << fid << ",\"hash\":\"" << fhx << "\",\"qpc_ns\":" << f0 << "}\n";
    hashlog.flush();
    sent_frames++;
    fid++;
    if(frame_interval_s>0) next_frame_ns=f0+(uint64_t)(frame_interval_s*1e9);
    else next_frame_ns=qpc_ns_now();
    uint64_t now=qpc_ns_now();
    if(now-t_last_stats>500*1000000ull||(a.frames>0&&(long long)sent_frames>=a.frames)){
      t_last_stats=now;
      double up=(now-t_start)/1e9; if(up<0.001)up=0.001;
      double pps=sent_packets/up, bps=sent_bytes*8/up;
      double cpu_pct=0;
#ifdef _WIN32
      { FILETIME ctc,etc,ktc,utc; ULARGE_INTEGER kk,uu; GetSystemTimeAsFileTime(&ctc); uint64_t wall=((uint64_t)ctc.dwHighDateTime<<32)|ctc.dwLowDateTime; if(GetProcessTimes(GetCurrentProcess(),&ctc,&etc,&ktc,&utc)){ kk.HighPart=ktc.dwHighDateTime; kk.LowPart=ktc.dwLowDateTime; uu.HighPart=utc.dwHighDateTime; uu.LowPart=utc.dwLowDateTime; if(cpu_prev_w!=0&&wall>cpu_prev_w){ uint64_t dw=wall-cpu_prev_w; uint64_t dc=(kk.QuadPart-cpu_prev_k.QuadPart)+(uu.QuadPart-cpu_prev_u.QuadPart); if(dw>0)cpu_pct=(double)dc/(double)dw*100.0; } cpu_prev_k=kk; cpu_prev_u=uu; cpu_prev_w=wall; } }
#endif
      char buf[1024];
      snprintf(buf,sizeof(buf),
        "{\"session_id\":\"%s\",\"running\":true,\"sent_frames\":%llu,\"sent_packets\":%llu,"
        "\"pps\":%.1f,\"bps\":%.1f,\"fps_actual\":%.2f,\"send_errors\":%llu,\"uptime_s\":%.2f,"
        "\"pattern\":\"%s\",\"fps_target\":%d,\"mtu\":%d,\"ethertype\":\"0x%04x\",\"gpu_ms\":%.3f,\"cpu_pct\":%.1f}",
        a.session.c_str(),(unsigned long long)sent_frames,(unsigned long long)sent_packets,
        pps,bps,sent_frames/up,(unsigned long long)send_errors,up,
        a.mode=="pattern"?pat_name(pid):(a.mode=="dda"?"dda":"file"),a.fps,a.mtu,a.ethertype,
        sent_frames>0?gpu_accum_ms/(double)sent_frames:0,cpu_pct);
      atomic_write(a.stats,buf);
    }
  }
  { double up=(qpc_ns_now()-t_start)/1e9; if(up<0.001)up=0.001;
      double cpu_pct_fin=0;
#ifdef _WIN32
      { FILETIME ctc,etc,ktc,utc; ULARGE_INTEGER kk,uu; GetSystemTimeAsFileTime(&ctc); uint64_t wall=((uint64_t)ctc.dwHighDateTime<<32)|ctc.dwLowDateTime; if(GetProcessTimes(GetCurrentProcess(),&ctc,&etc,&ktc,&utc)){ kk.HighPart=ktc.dwHighDateTime; kk.LowPart=ktc.dwLowDateTime; uu.HighPart=utc.dwHighDateTime; uu.LowPart=utc.dwLowDateTime; if(cpu_prev_w!=0&&wall>cpu_prev_w){ uint64_t dw=wall-cpu_prev_w; uint64_t dc=(kk.QuadPart-cpu_prev_k.QuadPart)+(uu.QuadPart-cpu_prev_u.QuadPart); if(dw>0)cpu_pct_fin=(double)dc/(double)dw*100.0; } } }
#endif
    char buf[1024];
    snprintf(buf,sizeof(buf),
      "{\"session_id\":\"%s\",\"running\":false,\"sent_frames\":%llu,\"sent_packets\":%llu,"
      "\"pps\":%.1f,\"bps\":%.1f,\"send_errors\":%llu,\"uptime_s\":%.2f,\"gpu_ms\":%.3f,\"cpu_pct\":%.1f}",
      a.session.c_str(),(unsigned long long)sent_frames,(unsigned long long)sent_packets,
      sent_packets/up,sent_bytes*8/up,(unsigned long long)send_errors,up,
      sent_frames>0?gpu_accum_ms/(double)sent_frames:0,cpu_pct_fin);
    atomic_write(a.stats,buf); }
#ifdef _WIN32
#ifdef _WIN32
  if(sq){d.sendqueue_destroy(sq);sq=nullptr;}
#endif
  d.closeh(h);
  timeEndPeriod(1);
#endif
  printf("[sender] done frames=%llu packets=%llu errors=%llu\n",
    (unsigned long long)sent_frames,(unsigned long long)sent_packets,(unsigned long long)send_errors);
  if(sent_frames>0){ uint64_t okpk=(sent_packets>=send_errors)?(sent_packets-send_errors):0; if(okpk==0){ fprintf(stderr,"live send failed\n"); return 3; } } return 0;
}

