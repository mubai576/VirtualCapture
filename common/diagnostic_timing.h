// Opt-in diagnostics; no changes to wire, frame SHM or existing stats.
#pragma once
#include <cstdint>
#ifdef _WIN32
#include <windows.h>
#include <cwchar>
#endif
namespace vcdiag {
enum Kind : uint32_t { StatsWrite=1, HashWrite=2, GetBuffer=3, Receive=4, StatsPrepare=5 };
#ifdef _WIN32
constexpr uint32_t Magic=0x56434447, Version=1, Lanes=32;
struct Header {
  uint32_t magic, version, bytes, lanes, pid, reserved;
  uint64_t frequency; uint8_t padding[32];
};
struct alignas(64) Lane {
  volatile LONG sequence, lease; uint32_t tid, kind;
  uint64_t generation, begin_ticks, count, total_ticks, max_ticks, last_ticks, slow_count, failures;
  uint8_t padding[48];
};
static_assert(sizeof(Header)==64 && sizeof(Lane)==128,"diagnostic layout");
inline uint64_t ticks(){ LARGE_INTEGER t; QueryPerformanceCounter(&t); return t.QuadPart; }
class Session {
  HANDLE mapping=nullptr; uint8_t* view=nullptr;
public:
  Session(){
    wchar_t enabled[8]={};
    if(GetEnvironmentVariableW(L"VC_DIAG",enabled,8)!=1 || enabled[0]!=L'1') return;
    wchar_t name[80]; swprintf_s(name,L"Local\\VirtualCapture.Diag.%lu",GetCurrentProcessId());
    const DWORD size=sizeof(Header)+Lanes*sizeof(Lane);
    mapping=CreateFileMappingW(INVALID_HANDLE_VALUE,nullptr,PAGE_READWRITE,0,size,name);
    if(!mapping) return;
    const bool fresh=GetLastError()!=ERROR_ALREADY_EXISTS;
    view=(uint8_t*)MapViewOfFile(mapping,FILE_MAP_ALL_ACCESS,0,0,size);
    if(!view) return;
    auto* h=(Header*)view;
    if(fresh){
      LARGE_INTEGER f; QueryPerformanceFrequency(&f);
      h->version=Version; h->bytes=size; h->lanes=Lanes; h->pid=GetCurrentProcessId(); h->frequency=f.QuadPart;
      MemoryBarrier(); InterlockedExchange((volatile LONG*)&h->magic,Magic);
    }
    if(h->magic!=Magic || h->version!=Version || h->bytes!=size){ UnmapViewOfFile(view); view=nullptr; }
  }
  ~Session(){ if(view) UnmapViewOfFile(view); if(mapping) CloseHandle(mapping); }
  Session(const Session&)=delete; Session& operator=(const Session&)=delete;
  Lane* claim(Kind kind){
    if(!view) return nullptr; auto* lanes=(Lane*)(view+sizeof(Header));
    for(uint32_t i=0;i<Lanes;i++){
      Lane* l=lanes+i;
      if(InterlockedCompareExchange(&l->lease,2,0)!=0) continue;
      InterlockedIncrement(&l->sequence);
      l->tid=GetCurrentThreadId(); l->kind=kind; l->generation=ticks();
      l->begin_ticks=l->count=l->total_ticks=l->max_ticks=l->last_ticks=l->slow_count=l->failures=0;
      InterlockedIncrement(&l->sequence); InterlockedExchange(&l->lease,1); return l;
    }
    return nullptr; // Fixed capacity; diagnostics never wait for a lane.
  }
  uint64_t frequency() const {return view?((Header*)view)->frequency:0;}
};
class Channel {
  friend class Scope; Lane* lane; uint64_t slow_ticks;
public:
  Channel(Session& s,Kind kind):lane(s.claim(kind)),slow_ticks(s.frequency()/50){}
  ~Channel(){if(lane) InterlockedExchange(&lane->lease,0);}
  Channel(const Channel&)=delete; Channel& operator=(const Channel&)=delete;
};
class Scope {
  Channel* channel; uint64_t start=0;
public:
  explicit Scope(Channel* c):channel(c && c->lane?c:nullptr){
    if(!channel) return; start=ticks(); Lane* l=channel->lane;
    InterlockedIncrement(&l->sequence); l->begin_ticks=start; InterlockedIncrement(&l->sequence);
  }
  ~Scope(){finish();}
  void finish(bool success=true){
    if(!channel) return; const uint64_t elapsed=ticks()-start; Lane* l=channel->lane;
    InterlockedIncrement(&l->sequence);
    l->count++; l->total_ticks+=elapsed; l->last_ticks=elapsed;
    if(elapsed>l->max_ticks) l->max_ticks=elapsed;
    if(elapsed>channel->slow_ticks) l->slow_count++;
    if(!success) l->failures++; l->begin_ticks=0;
    InterlockedIncrement(&l->sequence); channel=nullptr;
  }
};
#else
class Session {};
class Channel {public: Channel(Session&,Kind){} };
class Scope {public: explicit Scope(Channel*){} void finish(bool=true){} };
#endif
} // namespace vcdiag
