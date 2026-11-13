#pragma once
#include "stream.h"
#include "crc32.h"
#include "fnv1a.h"
#include "../proto/vc_wire.h"
#include <cstring>
#include <map>
#include <vector>
#include <stdexcept>

namespace vcau {
// This is a codec payload, NOT an extension of the frozen Ethernet/VC header.
constexpr size_t header_bytes=48;
constexpr uint32_t magic=0x55414356; // VCAU, little endian
inline uint64_t get(const uint8_t* p,int n){uint64_t v=0;for(int i=0;i<n;i++)v|=(uint64_t)p[i]<<(8*i);return v;}
inline void put(uint8_t* p,uint64_t v,int n){for(int i=0;i<n;i++)p[i]=(uint8_t)(v>>(8*i));}
inline bool random_access(const uint8_t* bytes,size_t size,int codec){
  if(codec==3)return size>=4 && bytes[0]==0xff && bytes[1]==0xd8 && bytes[size-2]==0xff && bytes[size-1]==0xd9;
  // PyroWave is intra-only; per-block headers are validated by the decoder itself.
  if(codec==4)return size>=8;
  return false;
}
inline bool random_access(const std::vector<uint8_t>& bytes,int codec){return random_access(bytes.data(),bytes.size(),codec);}
struct Unit {
  uint32_t fid=0; uint64_t first_ns=0,hash=0; bool key=false,eos=false;
  std::vector<uint8_t> bytes;
};
// Input is borrowed until return and must not alias out. Reuse capacity, not old header bytes.
inline uint64_t pack_into(std::vector<uint8_t>& out,const uint8_t* bytes,size_t size,
                          uint32_t fid,bool key,bool eos,const VCStream& s,int codec){
  out.resize(header_bytes+size);std::fill_n(out.data(),header_bytes,0);
  put(out.data(),magic,4);out[4]=1;out[5]=(uint8_t)codec;out[6]=key?1:(eos?2:0);out[7]=header_bytes;
  put(out.data()+8,s.width,4);put(out.data()+12,s.height,4);put(out.data()+16,s.fps,4);
  put(out.data()+20,size,4);put(out.data()+24,fid,4);
  uint64_t hash=vc_fnv1a64(bytes,size);put(out.data()+32,hash,8);
  if(size)memcpy(out.data()+header_bytes,bytes,size);
  return hash;
}
inline std::vector<uint8_t> pack(const Unit& u,const VCStream& s,int codec){
  std::vector<uint8_t> out;pack_into(out,u.bytes.data(),u.bytes.size(),u.fid,u.key,u.eos,s,codec);return out;
}
inline bool unpack(const std::vector<uint8_t>& data,uint32_t fid,const VCStream& s,int codec,size_t cap,Unit& u){
  if(data.size()<header_bytes || data.size()>cap || get(data.data(),4)!=magic || data[4]!=1 || data[5]!=codec ||
    data[6]>2 || data[7]!=header_bytes || get(data.data()+8,4)!=(unsigned)s.width || get(data.data()+12,4)!=(unsigned)s.height ||
    get(data.data()+16,4)!=(unsigned)s.fps || get(data.data()+20,4)!=data.size()-header_bytes ||
    get(data.data()+24,4)!=fid || get(data.data()+28,4) || get(data.data()+40,8)) return false;
  u.fid=fid;u.key=data[6]==1;u.eos=data[6]==2;u.hash=get(data.data()+32,8);
  if(u.eos!=(data.size()==header_bytes))return false;
  if((codec==3 || codec==4) && !u.eos && !u.key)return false;
  u.bytes.assign(data.begin()+header_bytes,data.end());
  return u.hash==vc_fnv1a64(u.bytes.data(),u.bytes.size());
}
class Assembler {
  struct Slot {uint16_t total=0;uint8_t pattern=0,flags=0;uint64_t stamp=0,first=0;size_t got=0,tail=0;std::vector<uint8_t> data,seen;};
  std::map<uint32_t,Slot> slots;
  uint32_t last=0;bool completed=false;
  void drop(std::map<uint32_t,Slot>::iterator it){lost+=it->second.total-it->second.got;slots.erase(it);dropped++;}
public:
  size_t payload,cap; uint64_t packets=0,bytes=0,bad=0,dropped=0,lost=0,units=0;
  Assembler(size_t p,size_t limit):payload(p),cap(limit){}
  bool accept(const vc_pkt_hdr_t& h,const uint8_t* p,uint64_t now,std::vector<uint8_t>& out,uint64_t& first){
    if(h.magic!=VC_MAGIC)return false;
    if(!h.total || h.total>(cap+payload-1)/payload || h.seq>=h.total || !h.payload_len || h.payload_len>payload ||
      (h.seq+1<h.total && h.payload_len!=payload) || (size_t)h.seq*payload+h.payload_len>cap ||
      (h.pattern>4 && h.pattern!=255) || (h.flags&~1u) || vc_crc32(p,h.payload_len)!=h.payload_crc){bad++;return false;}
    packets++;bytes+=42+h.payload_len;
    if(completed && h.frame_id<=last)return false;
    auto it=slots.find(h.frame_id);
    if(it==slots.end()){
      if(slots.size()>=2)drop(slots.begin());
      Slot slot;slot.total=h.total;slot.pattern=h.pattern;slot.flags=h.flags;slot.stamp=h.qpc_ns;slot.first=now;
      slot.data.resize(std::min(cap,(size_t)h.total*payload));slot.seen.resize(h.total);
      it=slots.emplace(h.frame_id,std::move(slot)).first;
    }
    auto& slot=it->second;
    if(slot.total!=h.total || slot.pattern!=h.pattern || slot.flags!=h.flags || slot.stamp!=h.qpc_ns){bad++;return false;}
    size_t off=(size_t)h.seq*payload;
    if(off+h.payload_len>slot.data.size()){bad++;return false;}
    if(slot.seen[h.seq]){
      if((h.seq+1==h.total && slot.tail!=h.payload_len) || memcmp(slot.data.data()+off,p,h.payload_len)){bad++;drop(it);}
      return false;
    }
    memcpy(slot.data.data()+off,p,h.payload_len);slot.seen[h.seq]=1;slot.got++;
    if(h.seq+1==h.total)slot.tail=h.payload_len;
    if(slot.got!=slot.total)return false;
    out=std::move(slot.data);out.resize((size_t)(slot.total-1)*payload+slot.tail);first=slot.first;
    last=h.frame_id;completed=true;units++;slots.erase(it);
    while(!slots.empty() && slots.begin()->first<last)drop(slots.begin());
    return true;
  }
  void finish(){while(!slots.empty())drop(slots.begin());}
};
inline int selftest(){
  try{
    auto require=[](bool good){if(!good)throw std::runtime_error("access-unit selftest failed");};
    VCStream stream;stream.width=418;stream.height=282;
    for(int codec:{3,4})for(size_t size:{0u,1u,37u,1459u,8959u,27001u}){
      Unit unit;unit.fid=7;unit.eos=size==0;unit.key=size!=0;unit.bytes.resize(size);
      for(size_t i=0;i<size;i++)unit.bytes[i]=(uint8_t)(i*17);
      auto data=pack(unit,stream,codec);Unit restored;
      std::vector<uint8_t> reused;reused.reserve(data.size()+100);auto* storage=reused.data();
      for(int repeat=0;repeat<3;repeat++){
        reused.resize(data.size()+100);std::fill(reused.begin(),reused.end(),0xa5);
        uint64_t hash=pack_into(reused,unit.bytes.data(),unit.bytes.size(),unit.fid,unit.key,unit.eos,stream,codec);
        require(reused==data && reused.data()==storage && hash==vc_fnv1a64(unit.bytes.data(),unit.bytes.size()));
      }
      require(unpack(data,7,stream,codec,data.size(),restored));require(restored.bytes==unit.bytes);
      require(!unpack(data,8,stream,codec,data.size(),restored));
      auto bad=data;bad[28]=1;require(!unpack(bad,7,stream,codec,bad.size(),restored));
      bad=data;bad.back()^=1;require(!unpack(bad,7,stream,codec,bad.size(),restored));
      for(size_t payload:{1458u,8958u}){
        Assembler assembler(payload,data.size());std::vector<uint8_t> result;uint64_t first=0;
        const size_t count=(data.size()+payload-1)/payload;
        for(size_t n=count;n>0;n--){size_t seq=n-1,off=seq*payload,len=std::min(payload,data.size()-off);
          vc_pkt_hdr_t h={};h.magic=VC_MAGIC;h.frame_id=7;h.total=(uint16_t)count;h.seq=(uint16_t)seq;
          h.payload_len=(uint16_t)len;h.payload_crc=vc_crc32(data.data()+off,len);h.pattern=4;
          if(seq==count-1){auto broken=h;broken.payload_crc^=1;require(!assembler.accept(broken,data.data()+off,1,result,first));require(assembler.bad==1);}
          bool done=assembler.accept(h,data.data()+off,2,result,first);require(done==(seq==0));
          require(!assembler.accept(h,data.data()+off,3,result,first));
        }
        require(result==data && assembler.units==1);
      }
    }
    {
      const uint8_t bytes[5]={1,2,3,4,5};Assembler assembler(5,48);std::vector<uint8_t> output;uint64_t first=0;
      vc_pkt_hdr_t h={};h.magic=VC_MAGIC;h.total=2;h.seq=0;h.payload_len=5;h.payload_crc=vc_crc32(bytes,5);
      require(!assembler.accept(h,bytes,1,output,first));
      auto bad=h;bad.total=11;require(!assembler.accept(bad,bytes,2,output,first));
      auto conflict=std::vector<uint8_t>(bytes,bytes+5);conflict[0]^=1;bad=h;bad.payload_crc=vc_crc32(conflict.data(),5);
      require(!assembler.accept(bad,conflict.data(),3,output,first));require(assembler.dropped==1);
      h.frame_id=1;require(!assembler.accept(h,bytes,4,output,first));h.frame_id=2;require(!assembler.accept(h,bytes,5,output,first));
      h.frame_id=3;require(!assembler.accept(h,bytes,6,output,first));require(assembler.dropped==2);
      assembler.finish();require(assembler.dropped==4 && assembler.lost==4);
      require(random_access({0xff,0xd8,0xff,0xd9},3));
      require(!random_access({0xff,0xd8,0xff},3));
      require(!random_access({0xff,0xd8,0xff,0xd9},1));
    }
    puts("PASS: codec envelope/hash/limits, reversed fragments, CRC, duplicates, bounded memory, independent JPEG frame and late-frame rejection");return 0;
  }catch(const std::exception& e){fprintf(stderr,"%s\n",e.what());return 1;}
}
}
