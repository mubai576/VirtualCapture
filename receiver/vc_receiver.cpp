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
#include <atomic>
#include <mutex>
#include <thread>
#include <chrono>
#include <condition_variable>
#ifdef _WIN32
#include <windows.h>
#else
#include <time.h>
#endif
#include "../proto/vc_wire.h"
#include "../vcam/vcam_shm.h"
#include "pattern.h"
#include "fnv1a.h"
#include "crc32.h"
#include "../common/stream.h"
#include "../common/diagnostic_timing.h"
#include "../common/codec_session.h"

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
  LARGE_INTEGER c; QueryPerformanceCounter(&c); return vc_qpc_to_ns((uint64_t)c.QuadPart,qpc_freq_); // 无 c*1e9 溢出
#else
  struct timespec ts; clock_gettime(CLOCK_MONOTONIC,&ts);
  return (uint64_t)ts.tv_sec*1000000000ull+(uint64_t)ts.tv_nsec;
#endif
}

struct Args{
  VCStream stream;
  VCCodec codec;
  std::string source_mac;
  std::string adapter="";
  uint16_t ethertype=0x88B5; int mtu=9000;
  std::string session="manual", stats="", hashlog="";
  long long timeout_s=0; long long expect_frames=-1;
  std::string shm="";  // P4-3: publish latest complete frame to named file mapping ("" = off)
  bool selftest=false;
  bool codec_check=false;
};
static uint16_t parse_et(const std::string&s){
  // 解析协议号
  unsigned v=0;
  if(s.size()>2&&(s[0]==48&&(s[1]==120||s[1]==88))) sscanf(s.c_str(),"%x",&v);
  else if(s.find_first_of("abcdefABCDEF")!=std::string::npos) sscanf(s.c_str(),"%x",&v);
  else v=(unsigned)atoi(s.c_str());
  return (uint16_t)v;
}
static void usage(){
  // 打印用法
  printf("vc-receiver.exe --adapter <substr> --ethertype 0x88B5 --mtu 9000\n"
    "  [--width 640 --height 640 --fps 240 --source-mac <session MAC>] [--capabilities]\n"
    "  --session <id> --stats <path> --hashlog <path> --timeout-s 0 [--expect-frames N] [--shm <mapname>] [--selftest]\n");
  puts("  codec: --codec mjpeg --codec-backend turbojpeg:cpu --codec-hardware 0 --gop 1\n"
       "  --bitrate 20000000 --max-bitrate 80000000 --decode-in-flight 2 --max-au-bytes 8388608 --jpeg-quality 1..100 (default 80)\n"
       "  --codec-backends | --codec-check <codec options> | --au-selftest | --mjpeg-selftest");
}
static void ensure_parent(const std::string&path){
  // 建父目录
  if(path.empty()) return;
  size_t p=path.find_last_of("/\\");
  if(p==std::string::npos) return;
  std::string dir=path.substr(0,p);
#ifdef _WIN32
  CreateDirectoryA(dir.c_str(),NULL);
#endif
}
static void atomic_write(const std::string&path,const std::string&data,vcdiag::Channel* timing=nullptr){
  // 原子写文件
  if(path.empty()) return;
  vcdiag::Scope elapsed(timing);
  ensure_parent(path);
  std::string tmp=path+".tmp";
  bool written=false;
  { std::ofstream f(tmp,std::ios::binary|std::ios::trunc); f.write(data.data(),(std::streamsize)data.size()); f.close(); written=!!f; }
#ifdef _WIN32
  bool moved=!!MoveFileExA(tmp.c_str(),path.c_str(),MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH);
#else
  bool moved=rename(tmp.c_str(),path.c_str())==0;
#endif
  elapsed.finish(written && moved);
}

// 重组器（可单测，不依赖抓包）
struct ReasmStats{
  uint64_t complete=0, dropped=0, packets_recv=0, packets_lost=0;
  std::atomic<uint64_t> hash_mismatch{0};   // live: written by the hash thread
  uint64_t gaps=0;
  uint64_t bytes_recv=0;
  uint64_t max_seen_fid=0; bool have_seen=false;
};
// 整帧哈希 + 测试源比对（live 模式在哈希线程，selftest 内联）
static bool rx_verify(const uint8_t* f,uint32_t fid,uint8_t pattern,uint64_t& fh,
                      std::vector<uint8_t>& eY,std::vector<uint8_t>& eUV,const VCStream& stream=VCStream()){
  fh=vc_fnv1a64(f,stream.frame_bytes());
  if(pattern>4) return true;
  eY.resize(stream.y_bytes()); eUV.resize(stream.uv_bytes());
  switch(pattern){
    case 0:vc_gen_counter(fid,eY.data(),eUV.data(),stream.width,stream.height);break;
    case 1:vc_gen_checker(fid,eY.data(),eUV.data(),stream.width,stream.height);break;
    case 2:vc_gen_rings(fid,eY.data(),eUV.data(),stream.width,stream.height);break;
    case 3:vc_gen_noise(fid,eY.data(),eUV.data(),stream.width,stream.height);break;
    default:vc_gen_motion(fid,eY.data(),eUV.data(),stream.width,stream.height);break;
  }
  return vc_fnv1a64_two(eY.data(),eY.size(),eUV.data(),eUV.size())==fh;
}
// 帧缓冲池。--shm 时缓冲就是 shm v2 槽（原地重组，发布=翻 seq+置事件，零整帧拷贝）；
// 无 --shm 时是私有缓冲，生命周期相同。状态：filling(主线程) -> latest -> hashing(哈希线程) -> free。
struct FramePool{
  uint8_t* shm=nullptr;
#ifdef _WIN32
  HANDLE hMap=nullptr; HANDLE evt[2]={nullptr,nullptr};
#endif
  static const int N=(int)VC_SHM_SLOTS;
  std::vector<std::vector<uint8_t>> own;
  bool filling[N]={}; uint64_t last_use[N]={}; int latest=-1; uint64_t tick=0, published=0;
  std::mutex mu; std::condition_variable cv; bool hashing[N]={};   // hashing 受 mu 保护
  VCStream stream;
  explicit FramePool(const VCStream& config=VCStream()):stream(config){ own.resize(N); }
  uint8_t* data(int i){ if(shm) return vc_shm_data(shm,(uint32_t)i); if(own[i].empty()) own[i].resize(stream.frame_bytes()); return own[i].data(); }
#ifdef _WIN32
  bool open_shm(const std::string& name){
    hMap=CreateFileMappingA(INVALID_HANDLE_VALUE,nullptr,PAGE_READWRITE,0,(DWORD)vc_shm_size(stream.frame_bytes()),name.c_str());
    if(!hMap) return false;
    if(GetLastError()==ERROR_ALREADY_EXISTS){ CloseHandle(hMap); hMap=nullptr; SetLastError(ERROR_ALREADY_EXISTS); return false; }
    shm=(uint8_t*)MapViewOfFile(hMap,FILE_MAP_WRITE,0,0,vc_shm_size(stream.frame_bytes()));
    if(!shm){ CloseHandle(hMap); hMap=nullptr; return false; }  // 例如旧 v1（更小）映射仍被占用
    for(int k=0;k<2;k++){
      std::string en=name+".evt"+std::to_string(k);
      evt[k]=CreateEventA(nullptr,TRUE,FALSE,en.c_str());
      if(!evt[k]) return false;
      ResetEvent(evt[k]);
    }
    vc_shm_hdr_t* h=(vc_shm_hdr_t*)shm;
    h->magic=0; MemoryBarrier();
    h->version=VC_SHM_VERSION; h->slots=VC_SHM_SLOTS; h->frame_bytes=(uint32_t)stream.frame_bytes();
    h->width=stream.width; h->height=stream.height; h->fps=stream.fps;
    h->data_off=VC_SHM_DATA_OFF; h->latest_slot=-1; h->writer_pid=GetCurrentProcessId();
    for(uint32_t i=0;i<VC_SHM_SLOTS;i++){ vc_shm_slot_t* sl=vc_shm_slot(shm,i); sl->seq|=1; sl->hash_fid=VC_SHM_HASH_NONE; }
    MemoryBarrier(); h->magic=VC_SHM_MAGIC;
    return true;
  }
  void close_shm(){
    if(shm){ UnmapViewOfFile(shm); shm=nullptr; }
    if(hMap){ CloseHandle(hMap); hMap=nullptr; }
    for(auto& e:evt) if(e){ CloseHandle(e); e=nullptr; }
  }
  ~FramePool(){ close_shm(); }
#endif
  int pick_locked(){
    int best=-1;
    for(int i=0;i<N;i++){
      if(filling[i]||hashing[i]||i==latest) continue;
      if(best<0||last_use[i]<last_use[best]) best=i;
    }
    return best;
  }
  int acquire(){
    int i=-1;
    { std::unique_lock<std::mutex> lk(mu);
      i=pick_locked();
      if(i<0) cv.wait_for(lk,std::chrono::milliseconds(100),[&]{ return (i=pick_locked())>=0; });
      if(i<0) return -1;
      filling[i]=true; }
#ifdef _WIN32
    if(shm){
      vc_shm_slot_t* sl=vc_shm_slot(shm,(uint32_t)i);
      // 偶->奇（填充中）；已是奇（弃帧残留）则 +2，值变化仍为奇
      InterlockedExchangeAdd((volatile LONG*)&sl->seq,(sl->seq&1)?2:1);
      sl->hash_fid=VC_SHM_HASH_NONE;
    }
#endif
    return i;
  }
  void abandon(int i){ if(i>=0) filling[i]=false; }   // shm seq 保持奇数：读端永远不会接受
  void publish(int i,uint32_t fid,uint64_t first_ns,uint64_t pub_ns){
    filling[i]=false;
    { std::lock_guard<std::mutex> lk(mu); hashing[i]=true; latest=i; }
    last_use[i]=++tick;
#ifdef _WIN32
    if(shm){
      vc_shm_hdr_t* h=(vc_shm_hdr_t*)shm; vc_shm_slot_t* sl=vc_shm_slot(shm,(uint32_t)i);
      sl->frame_id=fid; sl->qpc_first_ns=first_ns; sl->qpc_pub_ns=pub_ns;
      InterlockedIncrement((volatile LONG*)&sl->seq);            // 奇->偶：就绪（全屏障）
      InterlockedExchange((volatile LONG*)&h->latest_slot,i);
      LONG n=InterlockedIncrement((volatile LONG*)&h->latest_seq);
      ResetEvent(evt[(n+1)&1]); SetEvent(evt[n&1]);
      published++;
    }
#else
    (void)fid;(void)first_ns;(void)pub_ns;
#endif
  }
  void hashed(int i,uint32_t fid,uint64_t fh){
#ifdef _WIN32
    if(shm){ vc_shm_slot_t* sl=vc_shm_slot(shm,(uint32_t)i); sl->hash=fh; MemoryBarrier(); sl->hash_fid=fid; }
#else
    (void)fid;(void)fh;
#endif
    { std::lock_guard<std::mutex> lk(mu); hashing[i]=false; }
    cv.notify_all();
  }
};
struct FrameSlot{
  uint32_t fid=0; uint16_t total=0, got_n=0;
  int buf=-1; std::vector<char> got;
  uint64_t first_ns=0; uint8_t pattern=255; bool active=false;
};
struct DoneFrame{ int buf=-1; uint32_t fid=0; uint8_t pattern=255; uint64_t first_ns=0, done_ns=0; };
class Reassembler{
public:
  ReasmStats st;
  std::map<uint32_t,FrameSlot> slots; // 仅存活帧
  std::deque<uint64_t> arrivals; // 完成到达时间，留120个
  uint64_t last_complete_ns=0;
  uint32_t last_complete_fid=0;
  std::deque<uint64_t> rseg_ns; // P4-2步骤1：R(N)=首包到达→重组完成，同机QPC，最新10000帧窗口
  size_t payload_max;
  FramePool own_pool;
  FramePool* pool;
  bool inline_verify=true;   // selftest：完成即哈希/比对/释放；live：调用方发布 + 哈希线程异步
  std::vector<uint8_t> vY,vUV;
  VCStream stream;
  Reassembler(int mtu=9000,FramePool* p=nullptr,const VCStream& config=VCStream()):own_pool(config),stream(config){ payload_max=(size_t)(mtu-42); pool=p?p:&own_pool; }
  void drop_slot(std::map<uint32_t,FrameSlot>::iterator it){
    st.dropped++; st.packets_lost+=(it->second.total-it->second.got_n);
    pool->abandon(it->second.buf); slots.erase(it);
  }
  // 本包凑齐整帧则真（live 模式下 out 给出待发布缓冲）
  bool on_packet(const vc_pkt_hdr_t&h,const uint8_t*payload,DoneFrame*out=nullptr){
    if(h.magic!=VC_MAGIC) return false;
    size_t expected_total=(stream.frame_bytes()+payload_max-1)/payload_max;
    if(h.total!=expected_total || h.seq>=h.total){ st.packets_lost++; return false; }
    size_t expected_len=std::min(payload_max,stream.frame_bytes()-(size_t)h.seq*payload_max);
    if(h.payload_len!=expected_len){ st.packets_lost++; return false; }
    uint32_t c=vc_crc32(payload,h.payload_len);
    if(c!=h.payload_crc){ st.packets_lost++; return false; }
    st.packets_recv++; st.bytes_recv+=14+28+h.payload_len;
    // 帧号断档检测
    if(!st.have_seen){ st.have_seen=true; st.max_seen_fid=h.frame_id; }
    else if(h.frame_id>st.max_seen_fid){
      if(h.frame_id>st.max_seen_fid+1) st.gaps+=(h.frame_id-st.max_seen_fid-1);
      st.max_seen_fid=h.frame_id;
    }
    if(last_complete_ns && h.frame_id<=last_complete_fid) return false;
    auto it=slots.find(h.frame_id);
    if(it==slots.end()){
      // 超8帧逐出最旧
      if(slots.size()>=8) drop_slot(slots.begin());
      int b=pool->acquire();
      if(b<0){ st.packets_lost++; return false; }   // 池耗尽（哈希线程卡住>100ms）
      FrameSlot s; s.fid=h.frame_id; s.total=h.total; s.got_n=0; s.buf=b;
      s.got.assign(h.total,0);   // 不清零缓冲：只有每字节都到齐才会发布
      s.first_ns=qpc_ns_now(); s.pattern=h.pattern; s.active=true;
      it=slots.emplace(h.frame_id,std::move(s)).first;
    }
    FrameSlot&s=it->second;
    if(h.total!=s.total || h.pattern!=s.pattern){ st.packets_lost++; return false; } // 总包数对不上记丢
    if(s.got[h.seq]) return false; // 重包：已计入收包数
    size_t off=(size_t)h.seq*payload_max;
    if(off+h.payload_len>stream.frame_bytes()){ st.packets_lost++; return false; }
    memcpy(pool->data(s.buf)+off,payload,h.payload_len);
    s.got[h.seq]=1; s.got_n++;
    if(s.got_n==s.total){
      // 凑齐
      uint64_t now=qpc_ns_now();
      if(inline_verify||!out){
        uint64_t fh=0;
        if(!rx_verify(pool->data(s.buf),s.fid,s.pattern,fh,vY,vUV,stream)) st.hash_mismatch++;
        pool->abandon(s.buf);
      } else {
        out->buf=s.buf; out->fid=s.fid; out->pattern=s.pattern; out->first_ns=s.first_ns; out->done_ns=now;
      }
      st.complete++;
      if(rseg_ns.size()>=10000) rseg_ns.pop_front(); rseg_ns.push_back(now>=s.first_ns?now-s.first_ns:0); // 最新10000帧窗口（满了丢最旧）
      if(last_complete_ns!=0&&arrivals.size()<10000){ arrivals.push_back(now-last_complete_ns); if(arrivals.size()>120)arrivals.pop_front(); }
      last_complete_ns=now; last_complete_fid=s.fid;
      uint32_t donefid=s.fid;
      slots.erase(it);
      // 旧残帧记丢（新帧到达规则）
      std::vector<uint32_t> todrop;
      for(auto&p:slots) if(p.first<donefid) todrop.push_back(p.first);
      for(uint32_t f:todrop){ auto jt=slots.find(f); if(jt!=slots.end()) drop_slot(jt); }
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
  void rseg_stats(double&avg_ms,double&p50_ms,double&p99_ms,double&max_ms) const {
    // P4-2步骤1：R(N)分布，同机QPC，不跨机比较
    avg_ms=0;p50_ms=0;p99_ms=0;max_ms=0; if(rseg_ns.empty())return;
    double sum=0; for(auto v:rseg_ns)sum+=(double)v;
    avg_ms=sum/rseg_ns.size()/1e6;
    std::vector<uint64_t> s(rseg_ns.begin(),rseg_ns.end()); std::sort(s.begin(),s.end());
    p50_ms=(double)s[(s.size()*50)/100]/1e6;
    p99_ms=(double)s[(s.size()*99)/100]/1e6;
    max_ms=(double)s.back()/1e6;
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
    (unsigned long long)R.st.hash_mismatch.load());
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
  { double ra,rp50,rp99,rmx; R.rseg_stats(ra,rp50,rp99,rmx);
    if(R.rseg_ns.size()!=R.st.complete){printf("FAIL rseg samples!=complete got %zu\n",R.rseg_ns.size());ok=false;} }
  for(auto size : {std::pair<int,int>{2,2},{416,416},{418,282},{1280,720},{1920,1080},{2560,1440}}){
    VCStream stream; stream.width=size.first; stream.height=size.second;
    Reassembler variable(9000,nullptr,stream);
    std::vector<uint8_t> frame(stream.frame_bytes());
    vc_gen_motion(9,frame.data(),frame.data()+stream.y_bytes(),stream.width,stream.height);
    size_t total=(frame.size()+P-1)/P;
    vc_pkt_hdr_t head={}; head.magic=VC_MAGIC; head.frame_id=9; head.pattern=4;
    head.total=(uint16_t)(total+1); head.payload_len=(uint16_t)std::min(P,frame.size());
    head.payload_crc=vc_crc32(frame.data(),head.payload_len);
    variable.on_packet(head,frame.data());
    head.total=(uint16_t)total; head.payload_len--;
    head.payload_crc=vc_crc32(frame.data(),head.payload_len);
    variable.on_packet(head,frame.data());
    if(variable.st.complete || !variable.slots.empty() || variable.st.packets_lost!=2){ printf("FAIL malformed geometry packet\n");ok=false; }
    for(size_t n=total;n>0;n--){
      size_t seq=n-1,off=seq*P,len=std::min(P,frame.size()-off);
      head.seq=(uint16_t)seq; head.payload_len=(uint16_t)len; head.payload_crc=vc_crc32(frame.data()+off,len);
      variable.on_packet(head,frame.data()+off);
    }
    variable.on_packet(head,frame.data()); // A late packet must not publish the same frame twice.
    if(variable.st.complete!=1 || variable.st.hash_mismatch!=0 || !variable.slots.empty()){
      printf("FAIL variable reassembly %dx%d\n",stream.width,stream.height);ok=false;
    } else printf("RAW: %dx%d reverse-order complete, exact coverage, malformed/late rejection PASS\n",stream.width,stream.height);
  }
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
// 收包线程提优先级（MMCSS "Capture"，失败退回 TIME_CRITICAL），压 p99/max 尾部
static void rx_boost_thread(){
  typedef HANDLE (WINAPI *avset_fn)(LPCSTR,LPDWORD);
  HMODULE av=LoadLibraryA("avrt.dll"); DWORD task=0; bool mm=false;
  if(av){ avset_fn f=(avset_fn)GetProcAddress(av,"AvSetMmThreadCharacteristicsA"); if(f&&f("Capture",&task)) mm=true; }
  if(!mm) SetThreadPriority(GetCurrentThread(),THREAD_PRIORITY_TIME_CRITICAL);
  SetPriorityClass(GetCurrentProcess(),HIGH_PRIORITY_CLASS);
  printf("[receiver] rx thread priority: %s\n",mm?"MMCSS Capture":"TIME_CRITICAL");
}
#endif
// vcam -> receiver 回写环（vseg = receiver 发布 -> vcam 交付下游，同机 QPC）
struct VstatReader{
  std::string name; uint64_t next=0; std::deque<uint64_t> v; uint64_t repeats=0, n_total=0;
#ifdef _WIN32
  HANDLE hm=nullptr; uint8_t* base=nullptr;
  void poll(){
    if(name.empty()) return;
    if(!base){
      hm=OpenFileMappingA(FILE_MAP_READ,FALSE,name.c_str()); if(!hm) return;
      base=(uint8_t*)MapViewOfFile(hm,FILE_MAP_READ,0,0,VC_VSTAT_SIZE);
      if(!base){ CloseHandle(hm); hm=nullptr; return; }
      vc_vstat_hdr_t* h=(vc_vstat_hdr_t*)base; next=(uint64_t)(uint32_t)h->widx;  // 只统计本次 attach 之后
    }
    vc_vstat_hdr_t* h=(vc_vstat_hdr_t*)base;
    if(h->magic!=VC_VSTAT_MAGIC) return;
    uint64_t w=(uint64_t)(uint32_t)*(volatile LONG*)&h->widx;
    if(w<next) next=w;                       // vcam 重建映射
    if(w-next>VC_VSTAT_CAP) next=w-VC_VSTAT_CAP;
    vc_vstat_entry_t* r=(vc_vstat_entry_t*)(base+sizeof(vc_vstat_hdr_t));
    for(;next<w;next++){
      volatile vc_vstat_entry_t* e=&r[next%VC_VSTAT_CAP];
      if(e->n!=(uint32_t)(next+1)) continue;
      uint64_t pub=e->pub_ns, del=e->deliver_ns; uint32_t fl=e->flags;
      if(e->n!=(uint32_t)(next+1)) continue;
      n_total++;
      if(fl&VC_VSTAT_FLAG_REPEAT){ repeats++; continue; }
      if(v.size()>=10000) v.pop_front(); v.push_back(del>=pub?del-pub:0);
    }
  }
  ~VstatReader(){ if(base) UnmapViewOfFile(base); if(hm) CloseHandle(hm); }
#else
  void poll(){}
#endif
};
static std::string vseg_json(const VstatReader& vs,unsigned long long hq_max){
  double a=0,p50=0,p99=0,mx=0;
  if(!vs.v.empty()){
    double sum=0; for(auto x:vs.v) sum+=(double)x; a=sum/vs.v.size()/1e6;
    std::vector<uint64_t> s(vs.v.begin(),vs.v.end()); std::sort(s.begin(),s.end());
    p50=s[s.size()*50/100]/1e6; p99=s[s.size()*99/100]/1e6; mx=s.back()/1e6;
  }
  char b[320];
  snprintf(b,sizeof(b),"\"vseg_avg_ms\":%.3f,\"vseg_p50_ms\":%.3f,\"vseg_p99_ms\":%.3f,\"vseg_max_ms\":%.3f,"
    "\"vcam_delivered\":%llu,\"vcam_repeats\":%llu,\"hash_queue_max\":%llu",
    a,p50,p99,mx,(unsigned long long)vs.n_total,(unsigned long long)vs.repeats,hq_max);
  return b;
}

#include "encoded_receiver.h"

int main(int argc,char**argv){
  qpc_init();
  Args a;
  for(int i=1;i<argc;i++){
    std::string k=argv[i];
    auto need=[&](std::string&out){ if(i+1>=argc){fprintf(stderr,"missing value for %s\n",k.c_str());exit(2);} out=argv[++i]; };
    if(k=="--capabilities"){ puts(vc_codec_capabilities()); return 0; }
    else if(k=="--codec-backends"){return vc_codec_backends(false);}
    else if(k=="--codec-check"){a.codec_check=true;}
    else if(k=="--au-selftest"){return vcau::selftest();}
#ifdef VC_HAS_MJPEG
    else if(k=="--mjpeg-selftest"){return vcjpeg::selftest();}
#endif
    else if(k=="--width"){ std::string v; need(v); a.stream.width=vc_cli_int(v); }
    else if(k=="--height"){ std::string v; need(v); a.stream.height=vc_cli_int(v); }
    else if(k=="--fps"){ std::string v; need(v); a.stream.fps=vc_cli_int(v); }
    else if(k=="--source-mac"){ need(a.source_mac); }
    else if(k=="--adapter"){need(a.adapter);}
    else if(k=="--ethertype"){std::string v; need(v); a.ethertype=parse_et(v);}
    else if(k=="--mtu"){std::string v; need(v); a.mtu=vc_cli_int(v);}
    else if(k=="--session"){need(a.session);}
    else if(k=="--stats"){need(a.stats);}
    else if(k=="--hashlog"){need(a.hashlog);}
    else if(k=="--timeout-s"){std::string v; need(v); a.timeout_s=vc_cli_number(v);}
    else if(k=="--expect-frames"){std::string v; need(v); a.expect_frames=vc_cli_number(v);}
    else if(k=="--shm"){need(a.shm);}
    else if(k=="--selftest"){a.selftest=true;}
    else if(k=="--help"||k=="-h"){usage();return 0;}
    else {std::string v;need(v);if(!a.codec.option(k,v)){fprintf(stderr,"unknown arg %s\n",k.c_str());usage();return 2;}}
  }
#ifdef _WIN32
  SetConsoleCtrlHandler(chandler, TRUE);
#endif
  if(a.selftest) return run_selftest();
  uint8_t source_mac[6];
  if(!a.stream.valid() || !a.codec.valid(a.mtu,false) || a.mtu<=42 || a.mtu>9042 || (!a.codec.encoded() && (a.stream.frame_bytes()+(a.mtu-43))/(a.mtu-42)>65535) ||
      (!a.source_mac.empty() && !vc_parse_mac(a.source_mac,source_mac))){ fprintf(stderr,"invalid stream configuration\n"); return 2; }
  if(a.codec_check){if(!a.codec.encoded())return 2;return vc_codec_check(a.codec,a.stream,false);}
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
    h=d.openlive(devName.c_str(),9014+256,1,1,errbuf);  // to_ms=1: bound tail wait (see mintocopy note)
    if(!h){fprintf(stderr,"pcap_open_live failed: %s\n",errbuf);return 2;}
  }
  if(d.setbuff) d.setbuff(h,128*1024*1024);
  // mintocopy=0: deliver immediately. With 16KB the tail packets of a frame sat in the
  // kernel buffer until the NEXT frame pushed them out, so at low source fps (idle dda)
  // rseg ~= inter-frame gap (p99 ~400ms). Npcap still batches whatever is queued.
  if(d.setmintocopy) d.setmintocopy(h,0);
#else
  fprintf(stderr,"live capture only on Windows/Npcap\n"); return 1;
#endif
  if(a.codec.encoded()){int result=run_encoded_receiver(a,d,h);d.closeh(h);return result;}
  vcdiag::Session diagnostics;
  vcdiag::Channel stats_write(diagnostics,vcdiag::StatsWrite), stats_prepare(diagnostics,vcdiag::StatsPrepare);
  FramePool pool(a.stream);
  Reassembler R(a.mtu,&pool,a.stream);
  R.inline_verify=false;   // 哈希/比对/hashlog 移出关键路径（哈希线程）
  { ensure_parent(a.hashlog); std::ofstream f(a.hashlog,std::ios::trunc); }
  std::ofstream hashlog(a.hashlog,std::ios::app|std::ios::binary);
#ifdef _WIN32
  VstatReader vst;
  if(!a.shm.empty()){
    if(!pool.open_shm(a.shm)){ fprintf(stderr,"shm open failed: %s (err=%lu; mapping already held by another receiver/reader; close downstream clients and retry)\n",a.shm.c_str(),GetLastError()); return 1; }
    printf("[receiver] shm v3 publish -> %s (%zu bytes, %u slots, events %s.evt0/1)\n",a.shm.c_str(),vc_shm_size(a.stream.frame_bytes()),(unsigned)VC_SHM_SLOTS,a.shm.c_str());
    vst.name=a.shm+".vcam";
  }
  rx_boost_thread();
#else
  if(!a.shm.empty()){ fprintf(stderr,"--shm is Windows-only in this build\n"); return 1; }
#endif
  // 哈希线程：FIFO 消费已发布帧 -> 哈希 -> 比对 -> 写 slot.hash / hashlog -> 释放槽
  std::mutex hq_mu; std::condition_variable hq_cv; std::deque<DoneFrame> hq; bool hq_stop=false;
  std::thread hasher([&]{
    vcdiag::Channel hash_write(diagnostics,vcdiag::HashWrite);
    std::vector<uint8_t> eY,eUV; size_t nflush=0;
    for(;;){
      DoneFrame f;
      { std::unique_lock<std::mutex> lk(hq_mu);
        hq_cv.wait(lk,[&]{ return hq_stop||!hq.empty(); });
        if(hq.empty()) break;
        f=hq.front(); hq.pop_front(); }
      uint64_t fh=0;
      if(!rx_verify(pool.data(f.buf),f.fid,f.pattern,fh,eY,eUV,a.stream)) R.st.hash_mismatch++;
      pool.hashed(f.buf,f.fid,fh);
      char hx[17]; vc_fnv1a64_hex(fh,hx);
      vcdiag::Scope hash_time(&hash_write);
      hashlog << "{\"frame_id\":" << f.fid << ",\"hash\":\"" << hx << "\",\"qpc_ns\":" << f.done_ns << "}\n";
      if(++nflush%8==0) hashlog.flush();
      hash_time.finish(!!hashlog);
    }
    vcdiag::Scope final_flush(&hash_write);
    hashlog.flush();
    final_flush.finish(!!hashlog);
  });
  std::atomic<uint64_t> hq_max{0};
  uint64_t t_start=qpc_ns_now(), t_last_stats=0;
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
        if(et==a.ethertype && (a.source_mac.empty() || memcmp(eth+6,source_mac,6)==0)){
          vc_pkt_hdr_t hdr; memcpy(&hdr,eth+14,sizeof(hdr));
          uint8_t* pl=eth+14+sizeof(hdr);
          size_t avail=ph->caplen-(14+sizeof(hdr));
          if(hdr.payload_len<=avail){
            DoneFrame df;
            if(R.on_packet(hdr,pl,&df)&&df.buf>=0){
              pool.publish(df.buf,df.fid,df.first_ns,qpc_ns_now());   // 先发布（vcam 被事件唤醒）
              { std::lock_guard<std::mutex> lk(hq_mu); hq.push_back(df); if(hq.size()>hq_max) hq_max=hq.size(); }
              hq_cv.notify_one();
            }
          }
        }
      }
    } else if(rc==0){
      // 超时仍刷统计
    }
#endif
    if(now-t_last_stats>500*1000000ull){
      vcdiag::Scope prepare_time(&stats_prepare);
      t_last_stats=now;
      uint64_t shm_n=pool.shm?pool.published:0;
#ifdef _WIN32
      vst.poll();
#endif
      double up=(now-t_start)/1e9; if(up<0.001)up=0.001;
      double pps=R.st.packets_recv/up, bps=R.st.bytes_recv*8/up;
      double avg,p99; R.inter_stats(avg,p99);
      double rg_a,rg_p50,rg_p99,rg_mx; R.rseg_stats(rg_a,rg_p50,rg_p99,rg_mx);
      char buf[2048];
      snprintf(buf,sizeof(buf),
        "{\"session_id\":\"%s\",\"running\":true,\"complete\":%llu,\"dropped\":%llu,"
        "\"packets_recv\":%llu,\"packets_lost\":%llu,\"pps\":%.1f,\"bps\":%.1f,"
        "\"gaps\":%llu,\"trailing\":%zu,\"hash_mismatch\":%llu,"
        "\"inter_arrival_avg_ms\":%.3f,\"inter_arrival_p99_ms\":%.3f,\"rseg_avg_ms\":%.3f,"
        "\"rseg_p50_ms\":%.3f,\"rseg_p99_ms\":%.3f,\"rseg_max_ms\":%.3f,\"uptime_s\":%.2f,"
        "\"mtu\":%d,\"ethertype\":\"0x%04x\",\"shm_published\":%llu,%s}",
        a.session.c_str(),(unsigned long long)R.st.complete,(unsigned long long)R.st.dropped,
        (unsigned long long)R.st.packets_recv,(unsigned long long)R.st.packets_lost,
        pps,bps,(unsigned long long)R.st.gaps,R.trailing_packets(),
        (unsigned long long)R.st.hash_mismatch.load(),avg,p99,rg_a,rg_p50,rg_p99,rg_mx,up,a.mtu,a.ethertype,
        (unsigned long long)shm_n,vseg_json(vst,(unsigned long long)hq_max.load()).c_str());
      { std::string stats=buf; stats.pop_back();
        stats+=",\"width\":"+std::to_string(a.stream.width)+",\"height\":"+std::to_string(a.stream.height)+
          ",\"frame_bytes\":"+std::to_string(a.stream.frame_bytes())+",\"fps_limit\":"+std::to_string(a.stream.fps)+",\"codec\":\"raw\"}";
        prepare_time.finish();
        atomic_write(a.stats,stats,&stats_write); }
    }
  }
  { std::lock_guard<std::mutex> lk(hq_mu); hq_stop=true; }
  hq_cv.notify_all(); hasher.join();   // 收尾前哈希全部落盘，mismatch 计数完整
  // 收尾残帧记丢，保证账实相符
  while(!R.slots.empty()) R.drop_slot(R.slots.begin());
  { double up=(qpc_ns_now()-t_start)/1e9; if(up<0.001)up=0.001;
    double pps=R.st.packets_recv/up, bps=R.st.bytes_recv*8/up;
    double avg,p99; R.inter_stats(avg,p99);
    double rg_a,rg_p50,rg_p99,rg_mx; R.rseg_stats(rg_a,rg_p50,rg_p99,rg_mx);
    uint64_t shm_n=pool.shm?pool.published:0;
#ifdef _WIN32
    vst.poll();
#endif
    char buf[2048];
    snprintf(buf,sizeof(buf),
      "{\"session_id\":\"%s\",\"running\":false,\"complete\":%llu,\"dropped\":%llu,"
      "\"packets_recv\":%llu,\"packets_lost\":%llu,\"pps\":%.1f,\"bps\":%.1f,"
      "\"gaps\":%llu,\"trailing\":%zu,\"hash_mismatch\":%llu,"
      "\"inter_arrival_avg_ms\":%.3f,\"inter_arrival_p99_ms\":%.3f,\"rseg_avg_ms\":%.3f,"
      "\"rseg_p50_ms\":%.3f,\"rseg_p99_ms\":%.3f,\"rseg_max_ms\":%.3f,\"uptime_s\":%.2f,"
      "\"shm_published\":%llu,%s}",
      a.session.c_str(),(unsigned long long)R.st.complete,(unsigned long long)R.st.dropped,
      (unsigned long long)R.st.packets_recv,(unsigned long long)R.st.packets_lost,
      pps,bps,(unsigned long long)R.st.gaps,R.trailing_packets(),
      (unsigned long long)R.st.hash_mismatch.load(),avg,p99,rg_a,rg_p50,rg_p99,rg_mx,up,(unsigned long long)shm_n,vseg_json(vst,(unsigned long long)hq_max.load()).c_str());
    { std::string stats=buf; stats.pop_back();
        stats+=",\"width\":"+std::to_string(a.stream.width)+",\"height\":"+std::to_string(a.stream.height)+
          ",\"frame_bytes\":"+std::to_string(a.stream.frame_bytes())+",\"fps_limit\":"+std::to_string(a.stream.fps)+",\"codec\":\"raw\"}";
        atomic_write(a.stats,stats,&stats_write); } }
#ifdef _WIN32
  d.closeh(h);
#endif
  printf("[receiver] done complete=%llu dropped=%llu mismatch=%llu\n",
    (unsigned long long)R.st.complete,(unsigned long long)R.st.dropped,
    (unsigned long long)R.st.hash_mismatch.load());
  return 0;
}

