// 收端（T-02，PLAN §1冻结）：收包重组验哈希
// 链路：抓包过滤→按帧号重组→整帧哈希
// 近8帧缓存，新帧到则旧残帧记丢
// 哈希对账落盘按§2.3
// 统计：完成/丢/包数/pps/bps/间隔
// 间隔看近120包均值/p99，无网卡可自检
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <vector>
#include <map>
#include <deque>
#include <fstream>
#include <iostream>
#include <algorithm>
#include <cinttypes>
#include <cctype>
#ifdef _WIN32
#include <windows.h>
#else
#include <time.h>
#endif
#include "../proto/vc_wire.h"
#include "pattern.h"
#include "fnv1a.h"
#include "crc32.h"

#ifdef _WIN32
static volatile LONG g_stop = 0;
static BOOL WINAPI chandler(DWORD e){
  // 截停止信号
  if(e==CTRL_C_EVENT||e==CTRL_BREAK_EVENT||e==CTRL_CLOSE_EVENT){ InterlockedExchange(&g_stop,1); return TRUE; }
  return FALSE;
}
#endif

static uint64_t qpc_freq_=0;
static void qpc_init(){
  // 初始化计时频率
#ifdef _WIN32
  LARGE_INTEGER f; QueryPerformanceFrequency(&f); qpc_freq_=(uint64_t)f.QuadPart;
#else
  qpc_freq_=1000000000ull;
#endif
}
static uint64_t qpc_ns_now(){
  // 取纳秒戳
#ifdef _WIN32
  LARGE_INTEGER c; QueryPerformanceCounter(&c); return (uint64_t)c.QuadPart*1000000000ull/qpc_freq_;
#else
  struct timespec ts; clock_gettime(CLOCK_MONOTONIC,&ts);
  return (uint64_t)ts.tv_sec*1000000000ull+(uint64_t)ts.tv_nsec;
#endif
}

struct Args{
  std::string adapter="";
  uint16_t ethertype=0x88B5; int mtu=9000;
  std::string session="manual", stats="", hashlog="";
  long long timeout_s=0; long long expect_frames=-1;
  bool selftest=false;
};
static uint16_t parse_et(const std::string&s){
  unsigned v=0;
  if(s.size()>2&&(s[0]==48&&(s[1]==120||s[1]==88))) sscanf(s.c_str(),"%x",&v);
  else if(s.find_first_of("abcdefABCDEF")!=std::string::npos) sscanf(s.c_str(),"%x",&v);
  else v=(unsigned)atoi(s.c_str());
  return (uint16_t)v;
}
static void usage(){
  printf("vc-receiver.exe --adapter <substr> --ethertype 0x88B5 --mtu 9000\n"
    "  --session <id> --stats <path> --hashlog <path> --timeout-s 0 [--expect-frames N] [--selftest]\n");
}
static void ensure_parent(const std::string&path){
  if(path.empty()) return;
  size_t p=path.find_last_of("/\\");
  if(p==std::string::npos) return;
  std::string dir=path.substr(0,p);
#ifdef _WIN32
  CreateDirectoryA(dir.c_str(),NULL);
#endif
}
static void atomic_write(const std::string&path,const std::string&data){
  // 原子写文件
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

// ---------- reassembler (testable, no pcap dependency) ----------
struct ReasmStats{
  uint64_t complete=0, dropped=0, packets_recv=0, packets_lost=0;
  uint64_t hash_mismatch=0, gaps=0;
  uint64_t bytes_recv=0;
  uint64_t max_seen_fid=0; bool have_seen=false;
};
struct FrameSlot{
  uint32_t fid=0; uint16_t total=0, got_n=0;
  std::vector<uint8_t> data; std::vector<char> got;
  uint64_t first_ns=0; uint8_t pattern=255; bool active=false;
};
class Reassembler{
public:
  ReasmStats st;
  std::map<uint32_t,FrameSlot> slots; // active only
  std::deque<uint64_t> arrivals; // complete arrival ns, cap 120
  uint64_t last_complete_ns=0;
  size_t payload_max;
  std::vector<std::pair<uint32_t,std::string>> completed_hashes; // for hashlog
  Reassembler(int mtu=9000){ payload_max=(size_t)(mtu-42); }
  // returns true if frame completed by this packet
  bool on_packet(const vc_pkt_hdr_t&h,const uint8_t*payload,std::string*out_hash=nullptr){
    if(h.magic!=VC_MAGIC) return false;
    if(h.total==0||h.seq>=h.total||h.payload_len==0) { st.packets_lost++; return false; }
    if(h.payload_len>((h.seq==(uint16_t)(h.total-1))?payload_max+1:payload_max+1)) { st.packets_lost++; return false; }
    uint32_t c=vc_crc32(payload,h.payload_len);
    if(c!=h.payload_crc){ st.packets_lost++; return false; }
    st.packets_recv++; st.bytes_recv+=14+28+h.payload_len;
    // fid gap detection
    if(!st.have_seen){ st.have_seen=true; st.max_seen_fid=h.frame_id; }
    else if(h.frame_id>st.max_seen_fid){
      if(h.frame_id>st.max_seen_fid+1) st.gaps+=(h.frame_id-st.max_seen_fid-1);
      st.max_seen_fid=h.frame_id;
    }
    auto it=slots.find(h.frame_id);
    if(it==slots.end()){
      // 超8帧逐出最旧
      if(slots.size()>=8){
        auto old=slots.begin();
        st.dropped++; st.packets_lost+=(old->second.total-old->second.got_n);
        slots.erase(old);
      }
      FrameSlot s; s.fid=h.frame_id; s.total=h.total; s.got_n=0;
      s.data.assign(VC_FRAME_BYTES,0); s.got.assign(h.total,0);
      s.first_ns=qpc_ns_now(); s.pattern=h.pattern; s.active=true;
      it=slots.emplace(h.frame_id,std::move(s)).first;
    }
    FrameSlot&s=it->second;
    if(h.total!=s.total){ st.packets_lost++; return false; } // total mismatch -> treat lost
    if(s.got[h.seq]) return false; // dup: counted in packets_recv already
    size_t off=(size_t)h.seq*payload_max;
    if(off+h.payload_len>VC_FRAME_BYTES){ st.packets_lost++; return false; }
    memcpy(s.data.data()+off,payload,h.payload_len);
    s.got[h.seq]=1; s.got_n++;
    if(s.got_n==s.total){
      // complete
      uint64_t fh=vc_fnv1a64(s.data.data(),s.data.size());
      char hx[17]; vc_fnv1a64_hex(fh,hx);
      completed_hashes.emplace_back(s.fid,hx);
      if(out_hash) *out_hash=hx;
      // pattern verify (only known 0..4)
      if(s.pattern<=4){
        std::vector<uint8_t> eY(VC_Y_BYTES),eUV(VC_UV_BYTES);
        switch(s.pattern){
          case 0:vc_gen_counter(s.fid,eY.data(),eUV.data());break;
          case 1:vc_gen_checker(s.fid,eY.data(),eUV.data());break;
          case 2:vc_gen_rings(s.fid,eY.data(),eUV.data());break;
          case 3:vc_gen_noise(s.fid,eY.data(),eUV.data());break;
          default:vc_gen_motion(s.fid,eY.data(),eUV.data());break;
        }
        uint64_t eh=vc_fnv1a64_two(eY.data(),eY.size(),eUV.data(),eUV.size());
        if(eh!=fh) st.hash_mismatch++;
      }
      st.complete++;
      uint64_t now=qpc_ns_now();
      if(last_complete_ns!=0&&arrivals.size()<10000){ arrivals.push_back(now-last_complete_ns); if(arrivals.size()>120)arrivals.pop_front(); }
      last_complete_ns=now;
      uint32_t donefid=s.fid;
      slots.erase(it);
      // 旧残帧记丢（新帧到达规则）
      std::vector<uint32_t> todrop;
      for(auto&p:slots) if(p.first<donefid) todrop.push_back(p.first);
      for(uint32_t f:todrop){ auto jt=slots.find(f); if(jt!=slots.end()){ st.dropped++; st.packets_lost+=(jt->second.total-jt->second.got_n); slots.erase(jt);} }
      return true;
    }
    return false;
  }
  size_t trailing_packets() const { size_t n=0; for(auto&p:slots)n+=p.second.got_n; return n; }
  void inter_stats(double&avg_ms,double&p99_ms) const {
    avg_ms=0;p99_ms=0; if(arrivals.empty())return;
    double sum=0; for(auto v:arrivals)sum+=(double)v;
    avg_ms=sum/arrivals.size()/1e6;
    std::vector<uint64_t> s(arrivals.begin(),arrivals.end()); std::sort(s.begin(),s.end());
    p99_ms=(double)s[(s.size()*99)/100]/1e6;
  }
};

static int run_selftest(){
  // 自检：重组/坏包/哈希
  qpc_init();
  printf("[receiver selftest] in-mem 3 frames + loss/dup/badCRC/hashMismatch\n");
  const int MTU=9000; size_t P=(size_t)(MTU-42);
  size_t T=(VC_FRAME_BYTES+P-1)/P;
  if(T!=69){printf("FAIL total!=69 got %zu\n",T);return 1;}
  Reassembler R(MTU);
  std::vector<uint8_t> Y(VC_Y_BYTES),UV(VC_UV_BYTES);
  auto mkframe=[&](uint32_t fid,uint8_t pat){
    switch(pat){case 0:vc_gen_counter(fid,Y.data(),UV.data());break;
      case 1:vc_gen_checker(fid,Y.data(),UV.data());break;
      case 2:vc_gen_rings(fid,Y.data(),UV.data());break;
      case 3:vc_gen_noise(fid,Y.data(),UV.data());break;
      default:vc_gen_motion(fid,Y.data(),UV.data());break;}
    std::vector<uint8_t> f(VC_FRAME_BYTES);
    memcpy(f.data(),Y.data(),VC_Y_BYTES); memcpy(f.data()+VC_Y_BYTES,UV.data(),VC_UV_BYTES);
    return f;
  };
  auto feed=[&](uint32_t fid,const std::vector<uint8_t>&f,uint8_t pat,bool skip_one=false,bool corrupt_content=false){
    for(size_t s=0;s<T;s++){
      if(skip_one&&s==5) continue; // 漏一个
      size_t off=s*P; size_t cl=(s==T-1)?(VC_FRAME_BYTES-off):P;
      std::vector<uint8_t> chunk(cl); memcpy(chunk.data(),f.data()+off,cl);
      if(corrupt_content&&s==0){ chunk[0]^=0x01; } // CRC对但内容坏→哈希错
      vc_pkt_hdr_t h; memset(&h,0,sizeof(h));
      h.magic=VC_MAGIC; h.frame_id=fid; h.seq=(uint16_t)s; h.total=(uint16_t)T;
      h.qpc_ns=123; h.payload_crc=vc_crc32(chunk.data(),cl); h.payload_len=(uint16_t)cl;
      h.flags=0; h.pattern=pat;
      R.on_packet(h,chunk.data());
    }
  };
  // F0好帧fid=10
  auto f0=mkframe(10,4); feed(10,f0,4);
  // F1缺一包fid=11
  auto f1=mkframe(11,4); feed(11,f1,4,true,false);
  // 重包：在活跃帧内重喂，避开已完成帧建新槽
  // 在F2上测重包：喂半帧→重一包→喂完
  auto f2=mkframe(12,4);
  { // F2半帧含重包
    for(size_t s=0;s<T;s++){
      size_t off=s*P; size_t cl=(s==T-1)?(VC_FRAME_BYTES-off):P;
      vc_pkt_hdr_t h; memset(&h,0,sizeof(h));
      h.magic=VC_MAGIC; h.frame_id=12; h.seq=(uint16_t)s; h.total=(uint16_t)T;
      h.payload_crc=vc_crc32(f2.data()+off,cl); h.payload_len=(uint16_t)cl; h.pattern=4;
      R.on_packet(h,f2.data()+off);
      if(s==3){ // 重发seq3
        R.on_packet(h,f2.data()+off);
      }
      if(s==10) break;
    }
    // 坏CRC包（seq11错crc）
    { size_t s=11; size_t off=s*P; size_t cl=P;
      vc_pkt_hdr_t h; memset(&h,0,sizeof(h));
      h.magic=VC_MAGIC; h.frame_id=12; h.seq=(uint16_t)s; h.total=(uint16_t)T;
      h.payload_crc=0xDEADBEEFu; h.payload_len=(uint16_t)cl; h.pattern=4;
      size_t before=R.st.packets_lost;
      R.on_packet(h,f2.data()+off);
      if(R.st.packets_lost!=before+1){printf("FAIL badCRC not counted\n");return 1;}
      // 再喂对的seq11
      h.payload_crc=vc_crc32(f2.data()+off,cl);
      R.on_packet(h,f2.data()+off);
    }
    for(size_t s=12;s<T;s++){
      size_t off=s*P; size_t cl=(s==T-1)?(VC_FRAME_BYTES-off):P;
      vc_pkt_hdr_t h; memset(&h,0,sizeof(h));
      h.magic=VC_MAGIC; h.frame_id=12; h.seq=(uint16_t)s; h.total=(uint16_t)T;
      h.payload_crc=vc_crc32(f2.data()+off,cl); h.payload_len=(uint16_t)cl; h.pattern=4;
      R.on_packet(h,f2.data()+off);
    }
  }
  // 此时F2完成，旧残F1应已记丢
  // F3内容坏fid=13→凑齐但哈希错
  auto f3=mkframe(13,4); feed(13,f3,4,false,true);
  // 错魔数包忽略，不计数
  { uint64_t before=R.st.packets_recv;
    vc_pkt_hdr_t h; memset(&h,0,sizeof(h));
    h.magic=0xDEADu; h.frame_id=99; h.seq=0; h.total=69; h.payload_crc=0; h.payload_len=100; h.pattern=4;
    uint8_t dummy[100]={0};
    R.on_packet(h,dummy);
    if(R.st.packets_recv!=before){printf("FAIL wrong magic counted\n");return 1;}
  }
  printf("result complete=%llu dropped=%llu recv=%llu lost=%llu mismatch=%llu\n",
    (unsigned long long)R.st.complete,(unsigned long long)R.st.dropped,
    (unsigned long long)R.st.packets_recv,(unsigned long long)R.st.packets_lost,
    (unsigned long long)R.st.hash_mismatch);
  // 期望：完成3丢1错1
  // 收包276=69+68+69+1重+69（坏CRC不计）
  // 丢包2=漏1+坏CRC1（弃帧余量另计防重）
  // 漏包暂不计，坏CRC+1，弃帧补1，共2
  bool ok=true;
  if(R.st.complete!=3){printf("FAIL complete!=3\n");ok=false;}
  if(R.st.dropped!=1){printf("FAIL dropped!=1\n");ok=false;}
  if(R.st.hash_mismatch!=1){printf("FAIL mismatch!=1\n");ok=false;}
  if(R.st.packets_recv!=276){printf("FAIL recv!=276 got %llu\n",(unsigned long long)R.st.packets_recv);ok=false;}
  if(R.st.packets_lost!=2){printf("FAIL lost!=2 got %llu\n",(unsigned long long)R.st.packets_lost);ok=false;}
  if(!ok){printf("FAIL: receiver selftest\n");return 1;}
  printf("PASS: receiver selftest\n");
  return 0;
}

#ifdef _WIN32
#include <windows.h>
struct DynPcapRx{
  HMODULE dll=nullptr;
  typedef int (*findalldevs_fn)(void**,char*);
  typedef void (*freealldevs_fn)(void*);
  typedef void* (*openlive_fn)(const char*,int,int,int,char*);
  typedef int (*nextex_fn)(void*,void**,void*);
  typedef void (*close_fn)(void*);
  typedef int (*setbuff_fn)(void*,int);
  typedef int (*setmintocopy_fn)(void*,int);
  findalldevs_fn findalldevs=nullptr; freealldevs_fn freealldevs=nullptr;
  openlive_fn openlive=nullptr; nextex_fn nextex=nullptr; close_fn closeh=nullptr;
  setbuff_fn setbuff=nullptr; setmintocopy_fn setmintocopy=nullptr;
  std::string err;
  bool load(){
    dll=LoadLibraryA("wpcap.dll");
    if(!dll){ err="wpcap.dll not found"; return false; }
    findalldevs=(findalldevs_fn)GetProcAddress(dll,"pcap_findalldevs");
    freealldevs=(freealldevs_fn)GetProcAddress(dll,"pcap_freealldevs");
    openlive=(openlive_fn)GetProcAddress(dll,"pcap_open_live");
    nextex=(nextex_fn)GetProcAddress(dll,"pcap_next_ex");
    closeh=(close_fn)GetProcAddress(dll,"pcap_close");
    setbuff=(setbuff_fn)GetProcAddress(dll,"pcap_setbuff");
    setmintocopy=(setmintocopy_fn)GetProcAddress(dll,"pcap_setmintocopy");
    if(!openlive||!nextex){ err="missing exports"; return false; }
    return true;
  }
};
struct PcapHdr{ uint32_t ts_sec,ts_usec,caplen,len; };
#endif

int main(int argc,char**argv){
  qpc_init();
  Args a;
  for(int i=1;i<argc;i++){
    std::string k=argv[i];
    auto need=[&](std::string&out){ if(i+1>=argc){fprintf(stderr,"missing value for %s\n",k.c_str());exit(2);} out=argv[++i]; };
    if(k=="--adapter"){need(a.adapter);}
    else if(k=="--ethertype"){a.ethertype=parse_et(argv[++i]);}
    else if(k=="--mtu"){a.mtu=atoi(argv[++i]);}
    else if(k=="--session"){need(a.session);}
    else if(k=="--stats"){need(a.stats);}
    else if(k=="--hashlog"){need(a.hashlog);}
    else if(k=="--timeout-s"){a.timeout_s=atoll(argv[++i]);}
    else if(k=="--expect-frames"){a.expect_frames=atoll(argv[++i]);}
    else if(k=="--selftest"){a.selftest=true;}
    else if(k=="--help"||k=="-h"){usage();return 0;}
    else {fprintf(stderr,"unknown arg %s\n",k.c_str());usage();return 2;}
  }
#ifdef _WIN32
  SetConsoleCtrlHandler(chandler, TRUE);
#endif
  if(a.selftest) return run_selftest();
  if(a.stats.empty()) a.stats="run/"+a.session+".receiver.stats.json";
  if(a.hashlog.empty()) a.hashlog="run/"+a.session+".receiver.hash.jsonl";
  printf("[receiver] session=%s mtu=%d ethertype=0x%04x timeout=%lld expect=%lld\n",
    a.session.c_str(),a.mtu,a.ethertype,(long long)a.timeout_s,(long long)a.expect_frames);
#ifdef _WIN32
  DynPcapRx d; if(!d.load()){fprintf(stderr,"pcap load: %s\n",d.err.c_str());return 1;}
  struct PcapIf{ PcapIf* next; char* name; char* description; void* addresses; uint32_t flags; };
  std::string devName="";
  auto dump_devs = [&](PcapIf* devs){
    int idx=0;
    for(PcapIf* p=devs;p;p=p->next){
      fprintf(stderr,"  tried[%d] name=%s desc=%s\n", idx,
        p->name?p->name:"(null)", p->description?p->description:"(null)");
      idx++;
    }
    if(idx==0) fprintf(stderr,"  (no Npcap devices enumerated)\n");
  };
  char errbuf[256]={0};
  void* h=nullptr;
  if(a.adapter.empty()){
    PcapIf* devs=nullptr; char eb[256]={0};
    if(!d.findalldevs||d.findalldevs((void**)&devs,eb)!=0||!devs){
      fprintf(stderr,"no adapter; pass --adapter <substr> (findalldevs failed: %s)\n", eb);
      return 2;
    }
    for(PcapIf* p=devs;p;p=p->next){
      std::string nm=p->name?p->name:"";
      if(nm.empty()) continue;
      char oe[256]={0};
      void* hh=d.openlive(nm.c_str(),9014+256,1,1000,oe);
      if(hh){ devName=nm; h=hh;
        printf("[receiver] auto adapter %s (%s)\n", nm.c_str(), p->description?p->description:"");
        break;
      } else {
        fprintf(stderr,"[receiver] auto try %s failed: %s\n", nm.c_str(), oe);
      }
    }
    if(!h){
      fprintf(stderr,"all adapters failed; pass --adapter <substr>; use --list-adapters\n");
      dump_devs(devs);
      if(d.freealldevs)d.freealldevs(devs);
      return 2;
    }
    if(d.freealldevs)d.freealldevs(devs);
  } else {
    PcapIf* devs=nullptr; char eb[256]={0};
    if(!d.findalldevs||d.findalldevs((void**)&devs,eb)!=0){fprintf(stderr,"findalldevs failed\n");return 2;}
    std::string needle=a.adapter; for(auto&c:needle)c=(char)tolower(c);
    for(PcapIf*p=devs;p;p=p->next){
      std::string nm=p->name?p->name:"", ds=p->description?p->description:"";
      std::string hay=nm+" "+ds; for(auto&c:hay)c=(char)tolower(c);
      if(hay.find(needle)!=std::string::npos){ devName=nm; printf("[receiver] adapter %s (%s)\n",nm.c_str(),ds.c_str()); break; }
    }
    if(devName.empty()){
      fprintf(stderr,"adapter '%s' not found; use --list-adapters\n",a.adapter.c_str());
      dump_devs(devs);
      if(d.freealldevs)d.freealldevs(devs);
      return 2;
    }
    if(d.freealldevs)d.freealldevs(devs);
    h=d.openlive(devName.c_str(),9014+256,1,1000,errbuf);
    if(!h){fprintf(stderr,"pcap_open_live failed: %s\n",errbuf);return 2;}
  }
  if(d.setbuff) d.setbuff(h,128*1024*1024);
  if(d.setmintocopy) d.setmintocopy(h,16*1024);
#else
  fprintf(stderr,"live capture only on Windows/Npcap\n"); return 1;
#endif
  Reassembler R(a.mtu);
  { ensure_parent(a.hashlog); std::ofstream f(a.hashlog,std::ios::trunc); }
  std::ofstream hashlog(a.hashlog,std::ios::app|std::ios::binary);
  uint64_t t_start=qpc_ns_now(), t_last_stats=0;
  size_t hashlog_flushed=0;
  for(;;){
#ifdef _WIN32
    if(g_stop) break;
#endif
    uint64_t now=qpc_ns_now();
    if(a.timeout_s>0&&(long long)((now-t_start)/1000000000ull)>=a.timeout_s) break;
    if(a.expect_frames>0&&(long long)R.st.complete>=a.expect_frames) break; // 0/-1不限帧
#ifdef _WIN32
    void* pkt_hdr=nullptr; void* pkt_data=nullptr;
    int rc=d.nextex(h,&pkt_hdr,&pkt_data);
    if(rc==1&&pkt_hdr&&pkt_data){
      PcapHdr* ph=(PcapHdr*)pkt_hdr; uint8_t* eth=(uint8_t*)pkt_data;
      if(ph->caplen>=14+28){
        uint16_t et=(uint16_t)((eth[12]<<8)|eth[13]);
        if(et==a.ethertype){
          vc_pkt_hdr_t hdr; memcpy(&hdr,eth+14,sizeof(hdr));
          uint8_t* pl=eth+14+sizeof(hdr);
          size_t avail=ph->caplen-(14+sizeof(hdr));
          if(hdr.payload_len<=avail){
            std::string hx;
            bool done=R.on_packet(hdr,pl,&hx);
            if(done){
              // find fid: last completed is max? use completed_hashes back
              auto bk=R.completed_hashes.back();
              hashlog << "{\"frame_id\":" << bk.first << ",\"hash\":\"" << bk.second << "\",\"qpc_ns\":" << now << "}\n";
              if(++hashlog_flushed%8==0) hashlog.flush();
            }
          }
        }
      }
    } else if(rc==0){
      // timeout: still update stats
    }
#endif
    if(now-t_last_stats>500*1000000ull){
      t_last_stats=now; hashlog.flush();
      double up=(now-t_start)/1e9; if(up<0.001)up=0.001;
      double pps=R.st.packets_recv/up, bps=R.st.bytes_recv*8/up;
      double avg,p99; R.inter_stats(avg,p99);
      char buf[2048];
      snprintf(buf,sizeof(buf),
        "{\"session_id\":\"%s\",\"running\":true,\"complete\":%llu,\"dropped\":%llu,"
        "\"packets_recv\":%llu,\"packets_lost\":%llu,\"pps\":%.1f,\"bps\":%.1f,"
        "\"gaps\":%llu,\"trailing\":%zu,\"hash_mismatch\":%llu,"
        "\"inter_arrival_avg_ms\":%.3f,\"inter_arrival_p99_ms\":%.3f,\"uptime_s\":%.2f,"
        "\"mtu\":%d,\"ethertype\":\"0x%04x\"}",
        a.session.c_str(),(unsigned long long)R.st.complete,(unsigned long long)R.st.dropped,
        (unsigned long long)R.st.packets_recv,(unsigned long long)R.st.packets_lost,
        pps,bps,(unsigned long long)R.st.gaps,R.trailing_packets(),
        (unsigned long long)R.st.hash_mismatch,avg,p99,up,a.mtu,a.ethertype);
      atomic_write(a.stats,buf);
    }
  }
  hashlog.flush();
  // trailing incomplete frames are dropped at shutdown for exact accounting
  for(auto &p : R.slots){ R.st.dropped++; if(p.second.total>p.second.got_n) R.st.packets_lost+=(p.second.total-p.second.got_n); }
  R.slots.clear();
  { double up=(qpc_ns_now()-t_start)/1e9; if(up<0.001)up=0.001;
    double pps=R.st.packets_recv/up, bps=R.st.bytes_recv*8/up;
    double avg,p99; R.inter_stats(avg,p99);
    char buf[2048];
    snprintf(buf,sizeof(buf),
      "{\"session_id\":\"%s\",\"running\":false,\"complete\":%llu,\"dropped\":%llu,"
      "\"packets_recv\":%llu,\"packets_lost\":%llu,\"pps\":%.1f,\"bps\":%.1f,"
      "\"gaps\":%llu,\"trailing\":%zu,\"hash_mismatch\":%llu,"
      "\"inter_arrival_avg_ms\":%.3f,\"inter_arrival_p99_ms\":%.3f,\"uptime_s\":%.2f}",
      a.session.c_str(),(unsigned long long)R.st.complete,(unsigned long long)R.st.dropped,
      (unsigned long long)R.st.packets_recv,(unsigned long long)R.st.packets_lost,
      pps,bps,(unsigned long long)R.st.gaps,R.trailing_packets(),
      (unsigned long long)R.st.hash_mismatch,avg,p99,up);
    atomic_write(a.stats,buf); }
#ifdef _WIN32
  d.closeh(h);
#endif
  printf("[receiver] done complete=%llu dropped=%llu mismatch=%llu\n",
    (unsigned long long)R.st.complete,(unsigned long long)R.st.dropped,
    (unsigned long long)R.st.hash_mismatch);
  return 0;
}

