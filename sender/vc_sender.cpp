// 发端（T-01，PLAN §1冻结）：生成NV12发包
// 链路：NV12→整帧哈希→按mtu-42切包
// 发raw以太网包，QPC控帧率
// stats/hashlog按§1.4落盘
// CLI冻结，无网卡可跑--selftest
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <vector>
#include <deque>
#include <algorithm>
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
#include "capture.h"
#include "fnv1a.h"
#include "crc32.h"
#include "../common/codec_session.h"

// 64小图镜像（对账用）
// 须与pattern_ref.py小图函数一致
// 宽高64，块16，中央噪声16，公式同pattern.h
// 宽高代入，中心取半
namespace small64 {
static const int W=64,H=64;
static inline uint64_t sm_splitmix(uint64_t *s){
  // 小图随机数
  uint64_t z=(*s+=0x9E3779B97F4A7C15ull);
  z=(z^(z>>30))*0xBF58476D1CE4E5B9ull; z=(z^(z>>27))*0x94D049BB133111EBull; return z^(z>>31);
}
static inline void gen_counter(uint32_t fid,uint8_t*Y,uint8_t*UV){
  // 小图烟雾
  for(int y=0;y<H;y++)for(int x=0;x<W;x++)Y[y*W+x]=(uint8_t)(x+y+fid);
  uint8_t v=(uint8_t)(0x80^(fid&0xFF)); for(int i=0;i<W*H/2;i++)UV[i]=v;
}
static inline void gen_checker(uint32_t fid,uint8_t*Y,uint8_t*UV){
  // 小图棋盘
  int ox=fid&7,oy=(fid>>3)&7;
  for(int y=0;y<H;y++)for(int x=0;x<W;x++)Y[y*W+x]=((((x+ox)>>3)+((y+oy)>>3))&1)?255:0;
  for(int y=0;y<H/2;y++)for(int x=0;x<W;x+=2){
    uint8_t u=((((x>>3)+((y+fid)&1))&1))?16:240; uint8_t v=((((x>>3)+((y-fid)&1))&1))?16:240;
    UV[y*W+x]=u; UV[y*W+x+1]=v;
  }
}
static inline void gen_rings(uint32_t fid,uint8_t*Y,uint8_t*UV){
  // 小图同心环
  int cx=W/2,cy=H/2;
  for(int y=0;y<H;y++){int dy=y-cy;for(int x=0;x<W;x++){int dx=x-cx;uint32_t r2=(uint32_t)(dx*dx+dy*dy);
    Y[y*W+x]=(uint8_t)((r2>>4)+fid*7+((x^y)&15));}}
  for(int y=0;y<H/2;y++)for(int x=0;x<W;x+=2){
    UV[y*W+x]=(uint8_t)(128+(((x*3+y*5+fid*11)>>2)&127)-64);
    UV[y*W+x+1]=(uint8_t)(128+(((x*5-y*3+fid*13)>>2)&127)-64);
  }
}
static inline void gen_noise(uint32_t fid,uint8_t*Y,uint8_t*UV){
  // 小图噪声
  uint64_t s=((uint64_t)fid<<32)^0x123456789ABCull; // D11: XOR，见 pattern.h
  uint8_t*p=Y;size_t n=W*H;
  while(n>=8){uint64_t r=sm_splitmix(&s);memcpy(p,&r,8);p+=8;n-=8;}
  while(n--)*p++=(uint8_t)sm_splitmix(&s);
  p=UV;n=W*H/2;
  while(n>=8){uint64_t r=sm_splitmix(&s);memcpy(p,&r,8);p+=8;n-=8;}
  while(n--)*p++=(uint8_t)sm_splitmix(&s);
}
static inline void gen_motion(uint32_t fid,uint8_t*Y,uint8_t*UV){
  // 小图拟游戏
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
  // 读原始tick
#ifdef _WIN32
  LARGE_INTEGER c; QueryPerformanceCounter(&c); return (uint64_t)c.QuadPart;
#else
  struct timespec ts; clock_gettime(CLOCK_MONOTONIC,&ts);
  return (uint64_t)ts.tv_sec*1000000000ull+(uint64_t)ts.tv_nsec;
#endif
}
static void qpc_init() {
  // 初始化计时频率
#ifdef _WIN32
  LARGE_INTEGER f; QueryPerformanceFrequency(&f); qpc_freq_=(uint64_t)f.QuadPart;
#else
  qpc_freq_=1000000000ull;
#endif
}
static uint64_t qpc_ns_now() {
  // 取纳秒戳
  uint64_t c=qpc_now_raw();
#ifdef _WIN32
  return (c/qpc_freq_)*1000000000ull+(c%qpc_freq_)*1000000000ull/qpc_freq_;  // 无 c*1e9 溢出
#else
  return c;
#endif
}
static uint64_t qpc_to_ns_raw(uint64_t c){
#ifdef _WIN32
  return (c/qpc_freq_)*1000000000ull+(c%qpc_freq_)*1000000000ull/qpc_freq_;
#else
  return c;
#endif
}
#ifdef _WIN32
// 发送线程提优先级（MMCSS "Capture"，失败退回 TIME_CRITICAL），压 p99/max 尾部
static void sender_boost_thread(){
  typedef HANDLE (WINAPI *avset_fn)(LPCSTR,LPDWORD);
  HMODULE av=LoadLibraryA("avrt.dll"); DWORD task=0; bool mm=false;
  if(av){ avset_fn f=(avset_fn)GetProcAddress(av,"AvSetMmThreadCharacteristicsA"); if(f&&f("Capture",&task)) mm=true; }
  if(!mm) SetThreadPriority(GetCurrentThread(),THREAD_PRIORITY_TIME_CRITICAL);
  SetPriorityClass(GetCurrentProcess(),HIGH_PRIORITY_CLASS);
  printf("[sender] tx thread priority: %s\n",mm?"MMCSS Capture":"TIME_CRITICAL");
}
#endif
// P4-2步骤1：PC1段=采集/生成完成→末包发出，同机QPC，不跨机比较
static inline void pc1_stats(const std::deque<uint64_t>&v,double&a,double&p99,double&mx){
  a=0;p99=0;mx=0; if(v.empty())return;
  double sum=0; for(auto x:v)sum+=(double)x; a=sum/v.size()/1e6;
  std::vector<uint64_t> s(v.begin(),v.end()); std::sort(s.begin(),s.end());
  p99=(double)s[(s.size()*99)/100]/1e6; mx=(double)s.back()/1e6;
}
#ifdef _WIN32
static volatile LONG g_stop = 0;
static BOOL WINAPI chandler(DWORD e){
  // 截停止信号
  if(e==CTRL_C_EVENT||e==CTRL_BREAK_EVENT||e==CTRL_CLOSE_EVENT){ InterlockedExchange(&g_stop,1); return TRUE; }
  return FALSE;
}
#endif

static void sleep_us(uint64_t us){
  // 微秒休眠（粗睡+精旋）
  if(us==0) return;
  uint64_t t0=qpc_ns_now(), want=t0+us*1000ull; // 先记t0，休眠后只旋余量
  if(us>=2500){
#ifdef _WIN32
    Sleep((DWORD)((us-1500)/1000)); // 粗睡，留1.5ms精旋
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

// 动态加载wpcap
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
  VCStream stream;
  VCCodec codec;
  std::string source_mac="02:00:00:00:00:01";
  std::string adapter="";
  std::string mode="pattern";
  std::string pattern="motion";
  int fps=240; long long frames=0; long long batch=69;
  uint16_t ethertype=0x88B5; int mtu=9000; long long pacing_us=0;
  uint8_t dstmac[6]={0xFF,0xFF,0xFF,0xFF,0xFF,0xFF};
  std::string session="manual", stats="", hashlog="", file="";
  bool loop=false, list_adapters=false, selftest=false, gpu_convert=false;
  bool codec_check=false;
  bool wgc_cursor=true;
  int output=0;
  std::string wgc_window;
  long long test_drop_frame=-1;
};

static bool parse_mac(const std::string&s,uint8_t m[6]){
  return vc_parse_mac(s,m);
}
static uint16_t parse_et(const std::string&s){
  // 解析协议号（hex/十进制）
  unsigned v=0;
  if(s.size()>2&&(s[0]==48&&(s[1]==120||s[1]==88))) sscanf(s.c_str(),"%x",&v);
  else if(s.find_first_of("abcdefABCDEF")!=std::string::npos) sscanf(s.c_str(),"%x",&v);
  else v=(unsigned)atoi(s.c_str());
  return (uint16_t)v;
}
static int pat_id(const std::string&s){
  // 名转号
  if(s=="counter")return 0; if(s=="checker")return 1; if(s=="rings")return 2;
  if(s=="noise")return 3; if(s=="motion")return 4; return 4;
}
static const char* pat_name(int id){
  // 号转名
  switch(id){case 0:return "counter";case 1:return "checker";case 2:return "rings";
    case 3:return "noise";case 4:return "motion";default:return "motion";}
}

static void usage(){
  // 打印用法
  printf("vc-sender.exe --adapter <substr> --mode pattern|file|dda|wgc --pattern motion --fps 240\n"
    "  --frames 0 --ethertype 0x88B5 --mtu 9000 --pacing-us 0 --batch 69 --dst-mac ff:ff:ff:ff:ff:ff\n"
    "  --session <id> --stats <path> --hashlog <path> [--file <nv12> --loop] [--list-adapters] [--selftest]\n"
    "  [--width 640 --height 640 --transform crop|fit|fill|stretch --source-mac <session MAC>] [--capabilities]\n"
    "  [--output 0: 显示器序号（dda/wgc）] [--wgc-window <标题子串> --wgc-no-cursor]\n"
    "  [--gpu-convert: dda/wgc 走GPU转换，失败显式报错不回退]\n");
  puts("  codec: --codec mjpeg --codec-backend turbojpeg:cpu --codec-hardware 0 --gop 1\n"
       "         --codec pyrowave --codec-backend pyrowave:vulkan --codec-hardware 0 --gop 1\n"
       "  --bitrate 20000000 --max-bitrate 80000000 --decode-in-flight 2 --max-au-bytes 8388608 --jpeg-quality 1..100 (default 80)\n"
       "  --codec-backends | --codec-check <codec options> | --au-selftest | --mjpeg-selftest | --pyrowave-selftest; diagnostic only: --test-drop-frame N");
}

static void ensure_parent(const std::string&path){
  // 建父目录
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
  // 原子写文件（tmp换名）
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

static void gen_full(int pid,uint32_t fid,uint8_t*Y,uint8_t*UV,int w=640,int h=640){
  // 按号生成全帧
  switch(pid){case 0:vc_gen_counter(fid,Y,UV,w,h);break;case 1:vc_gen_checker(fid,Y,UV,w,h);break;
    case 2:vc_gen_rings(fid,Y,UV,w,h);break;case 3:vc_gen_noise(fid,Y,UV,w,h);break;
    default:vc_gen_motion(fid,Y,UV,w,h);break;}
}
static void gen_small(int pid,uint32_t fid,uint8_t*Y,uint8_t*UV){
  // 按号生成小图
  switch(pid){case 0:small64::gen_counter(fid,Y,UV);break;case 1:small64::gen_checker(fid,Y,UV);break;
    case 2:small64::gen_rings(fid,Y,UV);break;case 3:small64::gen_noise(fid,Y,UV);break;
    default:small64::gen_motion(fid,Y,UV);break;}
}

// 自检
static int run_selftest(){
  // 自检：分包/哈希/小图互锁
  qpc_init();
  printf("[sender selftest] patterns x determinism x frag-reassembly\n");
  const int MTU=9000;
  const size_t payload=(size_t)(MTU-42);
  const size_t total=(VC_FRAME_BYTES+payload-1)/payload;
  const size_t last_len=VC_FRAME_BYTES-(total-1)*payload;
  printf("frag: mtu=%d payload=%zu total=%zu last=%zu (expect 69/5256)\n",MTU,payload,total,last_len);
  if(total!=69){printf("FAIL total!=69\n");return 1;}
  if(last_len!=5256){printf("FAIL last!=5256 got %zu\n",last_len);return 1;}
  // mtu1500对照（反例）
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
      // 本地分包往返
      memcpy(frame.data(),Y.data(),VC_Y_BYTES);
      memcpy(frame.data()+VC_Y_BYTES,UV.data(),VC_UV_BYTES);
      // 切包+CRC+重组
      std::vector<uint8_t> out(VC_FRAME_BYTES); size_t off=0;
    for(size_t s=0;s<total;s++){
        size_t cl=(s==total-1)?last_len:payload;
        uint32_t c=vc_crc32(frame.data()+off,cl);
        // 先验CRC（传输完好路径）
        uint32_t c2=vc_crc32(frame.data()+off,cl);
        if(c!=c2){printf("FAIL crc self %s fid=%u seq=%zu\n",pat_name(pid),fid,s);ok=false;}
        memcpy(out.data()+off,frame.data()+off,cl);
        // 头字段合法性
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
      // 小图互锁向量
      gen_small(pid,fid,sY.data(),sUV.data());
      gen_small(pid,fid,sY2.data(),sUV2.data());
      uint64_t sh1=vc_fnv1a64_two(sY.data(),sY.size(),sUV.data(),sUV.size());
      uint64_t sh2=vc_fnv1a64_two(sY2.data(),sY2.size(),sUV2.data(),sUV2.size());
      if(sh1!=sh2){printf("FAIL small determinism %s fid=%u\n",pat_name(pid),fid);ok=false;}
      char shx[17]; vc_fnv1a64_hex(sh1,shx);
      printf("VEC small %s %u %s\n",pat_name(pid),fid,shx);
    }
  }
  // FNV标准向量
  // 空串=初值，a=af63dc4c8601ec8c
  { uint64_t he=vc_fnv1a64(nullptr,0);
    if(he!=14695981039346656037ull){printf("FAIL fnv empty %016llx\n",(unsigned long long)he);ok=false;}
    const uint8_t a[]={97};
    uint64_t ha=vc_fnv1a64(a,1);
    // 上值为标准a向量
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
  { // slice-by-8 CRC 与逐字节参考逐位一致（含标准向量 "123456789"=cbf43926 与各种长度/对齐）
    const uint8_t v[]={'1','2','3','4','5','6','7','8','9'};
    if(vc_crc32(v,9)!=0xCBF43926u){printf("FAIL crc32 std vector %08x\n",vc_crc32(v,9));ok=false;}
    std::vector<uint8_t> buf(9100); uint32_t sd=0x9e3779b9u; for(auto&b:buf){ sd=sd*1664525u+1013904223u; b=(uint8_t)(sd>>24); }
    int bad=0; for(size_t off=0;off<8;off++) for(size_t len: {0,1,7,8,9,63,5256,8958,9000}) if(vc_crc32(buf.data()+off,len)!=vc_crc32_ref(buf.data()+off,len)) bad++;
    if(bad){printf("FAIL crc32 slice8 != ref (%d cases)\n",bad);ok=false;} else printf("crc32: slice-by-8 == bytewise ref (72 cases)\n"); }
  { int ddr=dda_selftest(); if(ddr==1){printf("FAIL dda selftest\n");ok=false;} }
  { int dgr=dda_gpu_selftest(); if(dgr==1){printf("FAIL dda gpu selftest\n");ok=false;} }
  { int wgr=wgc_selftest(); if(wgr==1){printf("FAIL wgc selftest\n");ok=false;} }
#ifdef VC_HAS_PYROWAVE
  { int pwr=vcpyro::selftest(); if(pwr==1){printf("FAIL pyrowave selftest\n");ok=false;} }
#endif
  for(auto size : {std::pair<int,int>{2,2},{418,282},{416,416},{1280,720}}){
    std::vector<uint8_t> y((size_t)size.first*size.second),uv(y.size()/2);
    for(int pid=0;pid<5;pid++) for(uint32_t fid=0;fid<2;fid++){
      gen_full(pid,fid,y.data(),uv.data(),size.first,size.second);
      printf("VEC variable %dx%d %s %u %016llx\n",size.first,size.second,pat_name(pid),fid,
        (unsigned long long)vc_fnv1a64_two(y.data(),y.size(),uv.data(),uv.size()));
    }
  }
  { VCFrameLimit limit(240);
    if(!limit.ready(0)) ok=false;
    limit.sent(1000);
    if(limit.ready(1000+limit.period-1) || !limit.ready(1000+limit.period)) ok=false;
    limit.sent(1000000000ull);
    if(limit.ready(1000000001ull)) ok=false;
    if(!ok) printf("FAIL stream frame limiter\n");
  }
  if(!ok){printf("FAIL: sender selftest\n");return 1;}
  printf("PASS: sender selftest\n");
  return 0;
}

#ifdef _WIN32
static int run_list_adapters(){
  // 列网卡
  DynPcap d; if(!d.load()){ fprintf(stderr,"list-adapters: %s\n",d.err.c_str()); return 1; }
  if(!d.findalldevs){ fprintf(stderr,"list-adapters: no pcap_findalldevs\n"); return 1; }
  // 网卡结构：链首/名/描述/地址/标志
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

#include "encoded_sender.h"

int main(int argc,char**argv){
  qpc_init();
  Args a;
  for(int i=1;i<argc;i++){
    std::string k=argv[i];
    auto need=[&](std::string&out){ if(i+1>=argc){fprintf(stderr,"missing value for %s\n",k.c_str());exit(2);} out=argv[++i]; };
    if(k=="--capabilities"){ puts(vc_codec_capabilities()); return 0; }
    else if(k=="--codec-backends"){return vc_codec_backends(true);}
    else if(k=="--codec-check"){a.codec_check=true;}
    else if(k=="--au-selftest"){return vcau::selftest();}
#ifdef VC_HAS_MJPEG
    else if(k=="--mjpeg-selftest"){return vcjpeg::selftest();}
#endif
#ifdef VC_HAS_PYROWAVE
    else if(k=="--pyrowave-selftest"){return vcpyro::selftest();}
#endif
    else if(k=="--test-drop-frame"){std::string v;need(v);a.test_drop_frame=vc_cli_number(v);if(a.test_drop_frame<0)return 2;}
    else if(k=="--width"){ std::string v; need(v); a.stream.width=vc_cli_int(v); }
    else if(k=="--height"){ std::string v; need(v); a.stream.height=vc_cli_int(v); }
    else if(k=="--transform"){ need(a.stream.transform); }
    else if(k=="--source-mac"){ need(a.source_mac); }
    else if(k=="--adapter"){need(a.adapter);}
    else if(k=="--mode"){need(a.mode);}
    else if(k=="--output"){std::string v; need(v); a.output=vc_cli_int(v);}
    else if(k=="--wgc-window"){need(a.wgc_window);}
    else if(k=="--wgc-no-cursor"){a.wgc_cursor=false;}
    else if(k=="--wgc-selftest"){return wgc_selftest();}
    else if(k=="--pattern"){need(a.pattern);}
    else if(k=="--fps"){std::string v; need(v); a.fps=vc_cli_int(v);}
    else if(k=="--frames"){std::string v; need(v); a.frames=vc_cli_number(v);}
    else if(k=="--ethertype"){std::string v; need(v); a.ethertype=parse_et(v);}
    else if(k=="--mtu"){std::string v; need(v); a.mtu=vc_cli_int(v);}
    else if(k=="--pacing-us"){std::string v; need(v); a.pacing_us=vc_cli_number(v);}
    else if(k=="--dst-mac"){ std::string s; need(s); if(!parse_mac(s,a.dstmac)){fprintf(stderr,"bad --dst-mac\n");return 2;} }
    else if(k=="--batch"){std::string v; need(v); a.batch=vc_cli_number(v);}
    else if(k=="--session"){need(a.session);}
    else if(k=="--stats"){need(a.stats);}
    else if(k=="--hashlog"){need(a.hashlog);}
    else if(k=="--file"){need(a.file);}
    else if(k=="--loop"){a.loop=true;}
    else if(k=="--list-adapters"){a.list_adapters=true;}
    else if(k=="--selftest"){a.selftest=true;}
    else if(k=="--gpu-convert"){a.gpu_convert=true;}
    else if(k=="--help"||k=="-h"){usage();return 0;}
    else {std::string v;need(v);if(!a.codec.option(k,v)){fprintf(stderr,"unknown arg %s\n",k.c_str());usage();return 2;}}
  }
#ifdef _WIN32
  SetConsoleCtrlHandler(chandler, TRUE);
#endif
  if(a.selftest){
    if(!encoded_timing_selftest()){fprintf(stderr,"FAIL: encoded rolling timings/preparation bounds\n");return 1;}
    printf("PASS: encoded rolling timings/preparation bounds\n");
    return run_selftest();
  }
#ifdef _WIN32
  if(a.list_adapters) return run_list_adapters();
#else
  if(a.list_adapters){ fprintf(stderr,"list-adapters only on Windows/Npcap\n"); return 1; }
#endif
  if(a.stats.empty()) a.stats="run/"+a.session+".sender.stats.json";
  if(a.hashlog.empty()) a.hashlog="run/"+a.session+".sender.hash.jsonl";
  if(a.mode!="pattern"&&a.mode!="file"&&a.mode!="dda"&&a.mode!="wgc"){fprintf(stderr,"--mode must be pattern|file|dda|wgc\n");return 2;}
  if(a.output<0||a.output>15){fprintf(stderr,"--output must be 0..15\n");return 2;}
  if(!a.wgc_window.empty()&&a.mode!="wgc"){fprintf(stderr,"--wgc-window requires --mode wgc\n");return 2;}
  a.stream.fps=a.fps;
  uint8_t source_mac[6];
  if(!a.stream.valid() || !vc_parse_mac(a.source_mac,source_mac) || !a.codec.valid(a.mtu) || a.mtu<=42 || a.mtu>9042 ||
    (!a.codec.encoded() && a.test_drop_frame>=0)){ fprintf(stderr,"invalid stream/explicit codec configuration\n"); return 2; }
  if(a.codec_check){if(!a.codec.encoded())return 2;return vc_codec_check(a.codec,a.stream,true);}
  const size_t y_bytes=a.stream.y_bytes(), uv_bytes=a.stream.uv_bytes(), frame_bytes=a.stream.frame_bytes();
  int pid=pat_id(a.pattern);
  if(a.mtu<=42 || a.mtu>9042){ fprintf(stderr,"bad mtu\n"); return 2; }
  size_t payload=(size_t)(a.mtu-42);
  if(payload==0||payload>9000){fprintf(stderr,"bad mtu\n");return 2;}
  size_t total=(frame_bytes+payload-1)/payload;
  if(a.batch<1 || a.batch>10000 || a.frames<0 || a.pacing_us<0){fprintf(stderr,"bad --batch\n");return 2;}
  if(!a.codec.encoded() && total>65535){fprintf(stderr,"mtu too small\n");return 2;}
#ifdef _WIN32
  timeBeginPeriod(1); // 1ms精度，否则240Hz会睡成64Hz
#endif
  printf("[sender] session=%s mode=%s pattern=%s fps=%d frames=%lld mtu=%d payload=%zu total=%zu ethertype=0x%04x\n",
    a.session.c_str(),a.mode.c_str(),a.pattern.c_str(),a.fps,(long long)a.frames,
    a.mtu,payload,total,a.ethertype);

  // 文件模式读帧
  std::vector<uint8_t> fileFrames;
  size_t fileNFrames=0;
  if(a.mode=="file"){
    if(a.file.empty()){fprintf(stderr,"--file required in file mode\n");return 2;}
    std::ifstream f(a.file,std::ios::binary|std::ios::ate);
    if(!f){fprintf(stderr,"cannot open --file %s\n",a.file.c_str());return 1;}
    size_t sz=(size_t)f.tellg(); f.seekg(0);
    if(sz<frame_bytes||sz%frame_bytes!=0){fprintf(stderr,"file size %zu not multiple of %zu\n",sz,frame_bytes);return 2;}
    fileFrames.resize(sz); f.read((char*)fileFrames.data(),(std::streamsize)sz);
    fileNFrames=sz/frame_bytes;
    printf("[sender] file %s frames=%zu loop=%d\n",a.file.c_str(),fileNFrames,(int)a.loop);
  }

#ifdef _WIN32
  DynPcap d; if(!d.load()){fprintf(stderr,"pcap load: %s\n",d.err.c_str());return 1;}
  // 按子串找网卡
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

  std::vector<uint8_t> Y(y_bytes),UV(uv_bytes);
  DDACap dda_cap; dda_cap.stream=a.stream; dda_cap.output_index=a.output;
  WGCCap wgc_cap; wgc_cap.stream=a.stream; wgc_cap.output=a.output;
  wgc_cap.window_title=a.wgc_window; wgc_cap.include_cursor=a.wgc_cursor;
  CaptureHandle cap;
  double gpu_accum_ms=0;
  if(a.mode=="dda"){
    std::string dd; if(!dda_cap.init(dd)){fprintf(stderr,"dda init failed: %s\n",dda_cap.err());return 1;}
    cap.dda=&dda_cap; printf("[sender] dda output=%s center640\n",dd.c_str());
  } else if(a.mode=="wgc"){
    std::string wd; if(!wgc_cap.init(wd)){fprintf(stderr,"wgc init failed: %s\n",wgc_cap.err());return 1;}
    cap.wgc=&wgc_cap; printf("[sender] wgc output=%s center640\n",wd.c_str());
  }
#ifdef _WIN32
  if(cap.active()&&a.gpu_convert){ std::string gd; if(!cap.start_gpu(gd)){fprintf(stderr,"gpu-convert start failed: %s\n",gd.c_str());return 1;} printf("[sender] gpu-convert on (%s)\n",gd.c_str()); }
#else
  if(a.gpu_convert){fprintf(stderr,"--gpu-convert is Windows-only\n");return 2;}
#endif
  std::vector<uint8_t> eth(14+28+payload);
  if(a.codec.encoded()){
    sender_boost_thread();
    int result=run_encoded_sender(a,d,h,cap);
    d.closeh(h);timeEndPeriod(1);return result;
  }
  // 组以太网头
  memcpy(eth.data(),a.dstmac,6);
  uint8_t srcmac[6]={0x02,0x00,0x00,0x00,0x00,0x01};
  memcpy(eth.data()+6,source_mac,6);
  eth[12]=(uint8_t)(a.ethertype>>8); eth[13]=(uint8_t)(a.ethertype&0xFF);

  uint64_t sent_frames=0, sent_packets=0, send_errors=0, sent_bytes=0;
  std::deque<uint64_t> pc1_ns; // P4-2步骤1：PC1段样本，同机QPC，最新10000帧窗口
  std::deque<uint64_t> cap_lag_ns; // dda：LastPresentTime -> 转换完成，同机QPC
#ifdef _WIN32
  // sendqueue 复用：重置 len，免每批 2MB alloc/free（结构见 npcap pcap.h pcap_send_queue）
  struct SqHdr{ unsigned maxlen, len; char* buffer; };
  auto sq_reset=[](void* q){ ((SqHdr*)q)->len=0; };
  sender_boost_thread();
#endif
  void* sq=nullptr; int sq_n=0;
  uint64_t t_start=qpc_ns_now(), t_last_stats=0;
#ifdef _WIN32
  static ULARGE_INTEGER cpu_prev_k, cpu_prev_u; static uint64_t cpu_prev_w=0;
#endif
  // 起始清空hashlog
  { ensure_parent(a.hashlog); std::ofstream f(a.hashlog,std::ios::trunc); }
  std::ofstream hashlog(a.hashlog,std::ios::app|std::ios::binary);
  VCFrameLimit tx_limit(a.fps);
  uint64_t prepare_ns=0;
  uint32_t fid=0;
  size_t fileIdx=0, file_played=0;
  for(;;){
#ifdef _WIN32
    if(g_stop) break;
#endif
    if(a.frames>0&&(long long)sent_frames>=a.frames) break;
    if(a.mode=="file"&&!a.loop&&file_played>=fileNFrames) break;
    // Wait before acquisition: DDA coalesces intervening presents and yields the latest image.
    const uint64_t acquire_at=tx_limit.next>prepare_ns?tx_limit.next-prepare_ns:0;
    while(qpc_ns_now()<acquire_at) {
      uint64_t now=qpc_ns_now();
      if(now<acquire_at) sleep_us(std::max(1ull,(acquire_at-now)/1000ull));
#ifdef _WIN32
      if(g_stop) break;
#endif
    }
#ifdef _WIN32
    if(g_stop) break;
#endif
    uint64_t f0=qpc_ns_now();
    // 生成一帧
    uint8_t pat_field=255;
    bool new_image=true;
    if(a.mode=="pattern"){ gen_full(pid,fid,Y.data(),UV.data(),a.stream.width,a.stream.height); pat_field=(uint8_t)pid; }
    else if(a.mode=="dda"||a.mode=="wgc"){
      uint64_t g0=qpc_ns_now();
      if(!cap.capture(Y.data(),UV.data())){
        if(cap.err_code()!=DDACap::E_TIMEOUT){fprintf(stderr,"%s capture failed: %s\n",a.mode.c_str(),cap.err());return 1;}
        new_image=false;
      } else {
        if(cap.last_present_qpc()){ // 采集滞后 = 合成 present -> 转换完成（同机 QPC）
          uint64_t pn=qpc_to_ns_raw(cap.last_present_qpc()), dn=qpc_ns_now();
          if(cap_lag_ns.size()>=10000) cap_lag_ns.pop_front(); cap_lag_ns.push_back(dn>=pn?dn-pn:0); }
        gpu_accum_ms+=(qpc_ns_now()-g0)/1e6;
      }
    }
    else { memcpy(Y.data(),fileFrames.data()+fileIdx*frame_bytes,y_bytes);
           memcpy(UV.data(),fileFrames.data()+fileIdx*frame_bytes+y_bytes,uv_bytes);
           fileIdx=(fileIdx+1)%fileNFrames; file_played++; }
    if(new_image){ // Idle DDA still reaches the stats heartbeat without publishing stale pixels.
    uint64_t t_gen_done=qpc_ns_now(); // 采集/生成完成（D8 PC1段起点；整帧哈希已移到末包之后）
    // Predict conversion only (not DDA's wait); the output guard is authoritative, never this estimate.
    prepare_ns=std::min(tx_limit.period/2,(a.mode=="dda"||a.mode=="wgc")?qpc_to_ns_raw(cap.last_convert_qpc()):t_gen_done-f0);
    // A blocking AcquireNextFrame can finish much later than f0. Bound output as well as acquisition.
    while(!tx_limit.ready(qpc_ns_now())) {
      uint64_t now=qpc_ns_now();
      if(now<tx_limit.next) sleep_us(std::max(1ull,(tx_limit.next-now)/1000ull));
#ifdef _WIN32
      if(g_stop) break;
#endif
    }
#ifdef _WIN32
    if(g_stop) break;
#endif
    uint64_t send_start_ns=qpc_ns_now(); tx_limit.sent(send_start_ns);
    // 拼连续缓冲好切包
    // 免拷贝：先切Y再切UV
    size_t pkts_before = sent_packets; // 防停机半帧
    for(size_t s=0;s<total;s++){
      size_t off=s*payload;
      size_t cl=(s==total-1)?(frame_bytes-off):payload;
      // 载荷直接拷进以太帧缓冲（零分配、单次拷贝），处理Y/UV边界
      uint8_t* pl=eth.data()+14+sizeof(vc_pkt_hdr_t);
      if(off+cl<=y_bytes) memcpy(pl,Y.data()+off,cl);
      else if(off>=y_bytes) memcpy(pl,UV.data()+(off-y_bytes),cl);
      else { size_t nY=y_bytes-off; memcpy(pl,Y.data()+off,nY);
             memcpy(pl+nY,UV.data(),cl-nY); }
      uint32_t crc=vc_crc32(pl,cl);
      vc_pkt_hdr_t hdr; memset(&hdr,0,sizeof(hdr));
      hdr.magic=VC_MAGIC; hdr.frame_id=fid; hdr.seq=(uint16_t)s; hdr.total=(uint16_t)total;
      hdr.qpc_ns=f0; hdr.payload_crc=crc; hdr.payload_len=(uint16_t)cl;
      hdr.flags=(fid==0?1:0); hdr.pattern=pat_field;
      memcpy(eth.data()+14,&hdr,sizeof(hdr));
#ifdef _WIN32
      int plen=(int)(14+sizeof(hdr)+cl);
      // 注意：sendqueue成功返回正数，判错须用<0（曾误报16万错）
      if(a.batch<=1){ int rc=d.sendpacket(h,eth.data(),plen); if(rc<0) send_errors++; }
      else {
        if(!sq){
          if(!d.sendqueue_alloc||!d.sendqueue_queue||!d.sendqueue_transmit||!d.sendqueue_destroy){fprintf(stderr,"wpcap missing sendqueue exports\n");return 1;}
          size_t count=std::min(total,(size_t)a.batch);
          size_t capacity=std::max((size_t)2*1024*1024,count*(sizeof(PcapHdr)+14+sizeof(vc_pkt_hdr_t)+payload));
          sq=d.sendqueue_alloc((unsigned)capacity);if(!sq){fprintf(stderr,"sendqueue_alloc failed\n");return 1;}
          sq_n=0; }
        PcapHdr ph; memset(&ph,0,sizeof(ph)); ph.caplen=(unsigned)plen; ph.len=(unsigned)plen;
        if(d.sendqueue_queue(sq,(const void*)&ph,eth.data())!=0){ send_errors++; }
        else if(++sq_n>=a.batch){ if(d.sendqueue_transmit(h,sq,0)<0) send_errors+=(uint64_t)sq_n; sq_reset(sq); sq_n=0; }
      }
#endif
      sent_packets++; sent_bytes+=14+sizeof(hdr)+cl;
#ifdef _WIN32
      if(g_stop) break;
#endif
      if(a.pacing_us>0) sleep_us((uint64_t)a.pacing_us);
    }
#ifdef _WIN32
    if(g_stop && sent_packets - pkts_before < total) break; // 停机半帧丢弃，不记数
#endif
#ifdef _WIN32
    if(sq&&sq_n>0){ if(d.sendqueue_transmit(h,sq,0)<0) send_errors+=(uint64_t)sq_n; sq_reset(sq); sq_n=0; }
#endif
    { uint64_t t_last_tx=qpc_ns_now(); // 末包发出（D8 PC1段终点）
      if(pc1_ns.size()>=10000) pc1_ns.pop_front(); pc1_ns.push_back(t_last_tx>=t_gen_done?t_last_tx-t_gen_done:0); } // 最新10000帧窗口
    // 整帧哈希在末包之后算（只用于对账，不在延迟关键路径上；Y/UV 下一轮才会被覆盖）
    uint64_t fh=vc_fnv1a64_two(Y.data(),Y.size(),UV.data(),UV.size());
    char fhx[17]; vc_fnv1a64_hex(fh,fhx);
    hashlog << "{\"frame_id\":" << fid << ",\"hash\":\"" << fhx << "\",\"qpc_ns\":" << f0
      << ",\"send_qpc_ns\":" << send_start_ns << "}\n";
    if((sent_frames&7)==7) hashlog.flush();
    sent_frames++;
    fid++;
    }

    uint64_t now=qpc_ns_now();
    if(now-t_last_stats>500*1000000ull||(a.frames>0&&(long long)sent_frames>=a.frames)){
      t_last_stats=now;
      double up=(now-t_start)/1e9; if(up<0.001)up=0.001;
      double pps=sent_packets/up, bps=sent_bytes*8/up;
      double cpu_pct=0;
      double pc1_a,pc1_p99,pc1_mx; pc1_stats(pc1_ns,pc1_a,pc1_p99,pc1_mx);
      double cl_a,cl_p99,cl_mx; pc1_stats(cap_lag_ns,cl_a,cl_p99,cl_mx);
#ifdef _WIN32
      { FILETIME ctc,etc,ktc,utc; ULARGE_INTEGER kk,uu; GetSystemTimeAsFileTime(&ctc); uint64_t wall=((uint64_t)ctc.dwHighDateTime<<32)|ctc.dwLowDateTime; if(GetProcessTimes(GetCurrentProcess(),&ctc,&etc,&ktc,&utc)){ kk.HighPart=ktc.dwHighDateTime; kk.LowPart=ktc.dwLowDateTime; uu.HighPart=utc.dwHighDateTime; uu.LowPart=utc.dwLowDateTime; if(cpu_prev_w!=0&&wall>cpu_prev_w){ uint64_t dw=wall-cpu_prev_w; uint64_t dc=(kk.QuadPart-cpu_prev_k.QuadPart)+(uu.QuadPart-cpu_prev_u.QuadPart); if(dw>0)cpu_pct=(double)dc/(double)dw*100.0; } cpu_prev_k=kk; cpu_prev_u=uu; cpu_prev_w=wall; } }
#endif
      char buf[2048];
      snprintf(buf,sizeof(buf),
        "{\"session_id\":\"%s\",\"running\":true,\"sent_frames\":%llu,\"sent_packets\":%llu,"
        "\"pps\":%.1f,\"bps\":%.1f,\"fps_actual\":%.2f,\"send_errors\":%llu,\"uptime_s\":%.2f,"
        "\"pattern\":\"%s\",\"fps_target\":%d,\"mtu\":%d,\"ethertype\":\"0x%04x\",\"gpu_ms\":%.3f,\"gpu_dispatch_ms\":%.3f,\"gpu_dispatch_samples\":%llu,\"pc1_seg_avg_ms\":%.3f,\"pc1_seg_p99_ms\":%.3f,\"pc1_seg_max_ms\":%.3f,\"gpu_convert\":%d,\"cpu_pct\":%.1f,\"cap_lag_avg_ms\":%.3f,\"cap_lag_p99_ms\":%.3f,\"cap_lag_max_ms\":%.3f}",
        a.session.c_str(),(unsigned long long)sent_frames,(unsigned long long)sent_packets,
        pps,bps,sent_frames/up,(unsigned long long)send_errors,up,
        a.mode=="pattern"?pat_name(pid):(a.mode=="dda"?"dda":(a.mode=="wgc"?"wgc":"file")),a.fps,a.mtu,a.ethertype,
        sent_frames>0?gpu_accum_ms/(double)sent_frames:0,
        cap.gpu_samples()>0?cap.gpu_accum_ms()/(double)cap.gpu_samples():0,
        (unsigned long long)cap.gpu_samples(),pc1_a,pc1_p99,pc1_mx,(a.gpu_convert?1:0),cpu_pct,cl_a,cl_p99,cl_mx);
      { std::string stats=buf; stats.pop_back();
        stats+=",\"width\":"+std::to_string(a.stream.width)+",\"height\":"+std::to_string(a.stream.height)+
          ",\"frame_bytes\":"+std::to_string(frame_bytes)+",\"fps_limit\":"+std::to_string(a.fps)+
          ",\"transform\":\""+a.stream.transform+"\",\"codec\":\"raw\",\"source_skipped\":"+std::to_string(cap.source_skipped())+"}";
        atomic_write(a.stats,stats); }
    }
  }
  { double up=(qpc_ns_now()-t_start)/1e9; if(up<0.001)up=0.001;
      double cpu_pct_fin=0;
      double pc1_fa,pc1_fp99,pc1_fmx; pc1_stats(pc1_ns,pc1_fa,pc1_fp99,pc1_fmx);
      double cl_fa,cl_fp99,cl_fmx; pc1_stats(cap_lag_ns,cl_fa,cl_fp99,cl_fmx);
#ifdef _WIN32
      { FILETIME ctc,etc,ktc,utc; ULARGE_INTEGER kk,uu; GetSystemTimeAsFileTime(&ctc); uint64_t wall=((uint64_t)ctc.dwHighDateTime<<32)|ctc.dwLowDateTime; if(GetProcessTimes(GetCurrentProcess(),&ctc,&etc,&ktc,&utc)){ kk.HighPart=ktc.dwHighDateTime; kk.LowPart=ktc.dwLowDateTime; uu.HighPart=utc.dwHighDateTime; uu.LowPart=utc.dwLowDateTime; if(cpu_prev_w!=0&&wall>cpu_prev_w){ uint64_t dw=wall-cpu_prev_w; uint64_t dc=(kk.QuadPart-cpu_prev_k.QuadPart)+(uu.QuadPart-cpu_prev_u.QuadPart); if(dw>0)cpu_pct_fin=(double)dc/(double)dw*100.0; } } }
#endif
    char buf[2048];
    snprintf(buf,sizeof(buf),
      "{\"session_id\":\"%s\",\"running\":false,\"sent_frames\":%llu,\"sent_packets\":%llu,"
      "\"pps\":%.1f,\"bps\":%.1f,\"send_errors\":%llu,\"uptime_s\":%.2f,\"gpu_ms\":%.3f,\"gpu_dispatch_ms\":%.3f,\"gpu_dispatch_samples\":%llu,\"pc1_seg_avg_ms\":%.3f,\"pc1_seg_p99_ms\":%.3f,\"pc1_seg_max_ms\":%.3f,\"gpu_convert\":%d,\"cpu_pct\":%.1f,\"cap_lag_avg_ms\":%.3f,\"cap_lag_p99_ms\":%.3f,\"cap_lag_max_ms\":%.3f}",
      a.session.c_str(),(unsigned long long)sent_frames,(unsigned long long)sent_packets,
      sent_packets/up,sent_bytes*8/up,(unsigned long long)send_errors,up,
      sent_frames>0?gpu_accum_ms/(double)sent_frames:0,
      cap.gpu_samples()>0?cap.gpu_accum_ms()/(double)cap.gpu_samples():0,
      (unsigned long long)cap.gpu_samples(),pc1_fa,pc1_fp99,pc1_fmx,(a.gpu_convert?1:0),cpu_pct_fin,cl_fa,cl_fp99,cl_fmx);
    { std::string stats=buf; stats.pop_back();
        stats+=",\"width\":"+std::to_string(a.stream.width)+",\"height\":"+std::to_string(a.stream.height)+
          ",\"frame_bytes\":"+std::to_string(frame_bytes)+",\"fps_limit\":"+std::to_string(a.fps)+
          ",\"transform\":\""+a.stream.transform+"\",\"codec\":\"raw\",\"source_skipped\":"+std::to_string(cap.source_skipped())+"}";
        atomic_write(a.stats,stats); } }
#ifdef _WIN32
#ifdef _WIN32
  if(sq){d.sendqueue_destroy(sq);sq=nullptr;}  // 只在退出时销毁一次
#endif
  d.closeh(h);
  timeEndPeriod(1);
#endif
  printf("[sender] done frames=%llu packets=%llu errors=%llu\n",
    (unsigned long long)sent_frames,(unsigned long long)sent_packets,(unsigned long long)send_errors);
  if(sent_frames>0){ uint64_t okpk=(sent_packets>=send_errors)?(sent_packets-send_errors):0; if(okpk==0){ fprintf(stderr,"live send failed\n"); return 3; } } return 0;
}

