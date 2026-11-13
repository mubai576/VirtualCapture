#pragma once
#include <array>
#include <memory>
#include <sstream>
#include "encoder_backend.h"
#include "../common/latency_window.h"

struct VCEncodedTimings {
  enum Stage { PrepareWait, Source, Submit, EncodeWait, Pack, RateWait, Transmit, Audit, Log, CaptureToSend, Count };
  std::deque<uint64_t> samples[Count];
  void add(Stage stage,uint64_t ns){
    auto& values=samples[stage];values.push_back(ns);if(values.size()>10000)values.pop_front();
  }
  std::string json() const {
    const char* names[]={"prepare_wait","source","sample_submit","encode_wait","au_pack","rate_wait","transmit","audit_hash","log_io","capture_to_send"};
    std::ostringstream out;out<<"{\"window_frames\":10000";
    for(int i=0;i<Count;i++){
      double avg,p99,mx;pc1_stats(samples[i],avg,p99,mx);
      out<<",\""<<names[i]<<"\":{\"samples\":"<<samples[i].size()<<",\"avg\":"<<avg<<",\"p99\":"<<p99<<",\"max\":"<<mx<<"}";
    }
    return out.str()+"}";
  }
};

struct VCEncodePreparation {
  std::deque<uint64_t> recent[2];
  void observe(bool key,uint64_t ns){
    auto& values=recent[key?1:0];values.push_back(ns);if(values.size()>64)values.pop_front();
  }
  uint64_t lead(bool key,uint64_t period) const {
    const auto& values=recent[key?1:0];
    if(values.empty())return 0;
    std::array<uint64_t,64> ordered;
    std::copy(values.begin(),values.end(),ordered.begin());
    // Nearest-rank p95 avoids retaining a single slow wakeup for 64 subsequent IDRs.
    const size_t rank=(values.size()*95+99)/100-1;
    std::nth_element(ordered.begin(),ordered.begin()+rank,ordered.begin()+values.size());
    const uint64_t estimate=ordered[rank];
    return estimate>=period || period-estimate<=100000ull?period:estimate+100000ull;
  }
  std::string json(uint64_t period) const {
    std::ostringstream out;
    out<<"{\"policy\":\"p95_recent64_by_frame_type\",\"window_samples\":64,\"margin_ns\":100000"
      <<",\"inter\":{\"samples\":"<<recent[0].size()<<",\"lead_ns\":"<<lead(false,period)<<"}"
      <<",\"idr\":{\"samples\":"<<recent[1].size()<<",\"lead_ns\":"<<lead(true,period)<<"}}";
    return out.str();
  }
};

static bool encoded_timing_selftest(){
  VCEncodedTimings timing;
  for(uint64_t i=0;i<10020;i++)timing.add(VCEncodedTimings::Source,i*1000000ull);
  double avg,p99,mx;pc1_stats(timing.samples[VCEncodedTimings::Source],avg,p99,mx);
  if(timing.samples[VCEncodedTimings::Source].size()!=10000 || avg!=5019.5 || p99!=9920 || mx!=10019)return false;
  VCEncodePreparation prep;
  if(prep.lead(false,4166667)!=0)return false;
  prep.observe(false,500000);prep.observe(true,6000000);
  if(prep.lead(false,4166667)!=600000 || prep.lead(true,4166667)!=4166667)return false;
  for(int i=0;i<64;i++)prep.observe(false,400000);
  if(prep.lead(false,4166667)!=500000 || prep.lead(false,100)!=100)return false;
  prep.observe(false,6000000);
  if(prep.lead(false,4166667)!=500000 || prep.lead(true,4166667)!=4166667)return false;
  for(int i=0;i<3;i++)prep.observe(false,6000000);
  if(prep.lead(false,4166667)!=4166667 || prep.recent[0].size()!=64)return false;
  for(int i=0;i<64;i++)prep.observe(false,400000);
  if(prep.lead(false,4166667)!=500000)return false;
  VCEncodePreparation cold;
  cold.observe(false,UINT64_MAX);
  for(int i=0;i<18;i++)cold.observe(false,400000);
  if(cold.lead(false,4166667)!=4166667)return false;
  cold.observe(false,400000);
  if(cold.lead(false,4166667)!=500000 || cold.lead(false,0)!=0)return false;
  VCFrameLimit cap(240);
  uint64_t previous=0;
  for(uint64_t work:{400000ull,6000000ull,400000ull,400000ull}){
    uint64_t acquire_at=cap.next>prep.lead(false,cap.period)?cap.next-prep.lead(false,cap.period):0;
    uint64_t sent=std::max(acquire_at+work,cap.next);
    if(previous && sent-previous<cap.period)return false;
    cap.sent(sent);previous=sent;
  }
  return true;
}

static void encoded_wait_until(uint64_t deadline){
  for(uint64_t now=qpc_ns_now();now<deadline && !g_stop;now=qpc_ns_now()){
    uint64_t left=deadline-now;
    if(left>=2500000ull)Sleep((DWORD)std::min(10ull,(left-1500000ull)/1000000ull));
    else YieldProcessor();
  }
}

static int run_encoded_sender(const Args& a,DynPcap& d,pcap_t* handle,CaptureHandle& cap){
  try{
    VCEncoder encoder;
    std::vector<uint8_t> raw(a.stream.frame_bytes()),reused_au,reused_packet;
    const uint8_t* borrowed=nullptr;size_t borrowed_size=0,queue_capacity=0;
    std::unique_ptr<void,DynPcap::sendqueue_destroy_fn> reused_queue(nullptr,d.sendqueue_destroy);
    uint64_t started=qpc_ns_now(),last_stats=0,frames=0,packets=0,wire_bytes=0,encoded_bytes=0,keyframes=0,rate_wait_ns=0;
    uint32_t fid=0;uint64_t capture_ns=0;bool clean=false;
    std::deque<std::pair<uint64_t,size_t>> rate_window;size_t window_bytes=0;
    VCEncodedTimings timing;
    VCLatencyWindow send_segment;
    VCEncodePreparation preparation;
    VCFrameLimit limit(a.fps);
    ensure_parent(a.hashlog);std::ofstream hashlog(a.hashlog,std::ios::binary|std::ios::trunc);
    if(!hashlog)throw std::runtime_error("cannot open encoded sender hashlog");
    encoder.open(a.codec,a.stream,[&](const uint8_t* data,size_t size,LONGLONG stamp,bool key){
      if(stamp!=(LONGLONG)fid*(10000000LL/a.fps) || borrowed)throw std::runtime_error("encoder reordered/buffered more than one frame");
      clean=key;
      if(!size || size+vcau::header_bytes>(size_t)a.codec.max_au)throw std::runtime_error("access unit exceeds explicit max-au-bytes");
      // TurboJPEG owns a fixed buffer that remains valid until the next pump.
      borrowed=data;borrowed_size=size;
    });
    uint8_t source[6];vc_parse_mac(a.source_mac,source);
    const size_t payload=a.mtu-42;
    auto transmit=[&](const std::vector<uint8_t>& bytes,uint32_t frame,uint64_t stamp){
      size_t count=(bytes.size()+payload-1)/payload;
      auto& eth=reused_packet;
      eth.resize(42+payload);memcpy(eth.data(),a.dstmac,6);memcpy(eth.data()+6,source,6);
      eth[12]=(uint8_t)(a.ethertype>>8);eth[13]=(uint8_t)a.ethertype;
      void* queue=nullptr;int queued=0;
      if(a.batch>1){
        if(!d.sendqueue_alloc || !d.sendqueue_queue || !d.sendqueue_transmit || !d.sendqueue_destroy)throw std::runtime_error("missing sendqueue exports");
        unsigned capacity=(unsigned)(std::min(count,(size_t)a.batch)*(sizeof(PcapHdr)+42+payload));
        if(capacity>queue_capacity){
          reused_queue.reset(d.sendqueue_alloc(capacity));queue_capacity=capacity;
        }
        queue=reused_queue.get();
        if(!queue)throw std::runtime_error("encoded sendqueue allocation failed");
      }
      auto flush=[&](){if(queue && queued){
        unsigned expected=((unsigned*)queue)[1];int sent=d.sendqueue_transmit(handle,queue,0);
        if(sent<0 || (unsigned)sent!=expected){
          char message[192];snprintf(message,sizeof(message),"encoded sendqueue short transmit: %d of %u bytes; no batch fallback",sent,expected);
          throw std::runtime_error(message);
        }
        ((unsigned*)queue)[1]=0;queued=0;
      }};
      try{
        for(size_t seq=0;seq<count;seq++){
          // Explicit native diagnostic only; never exposed by product config/API.
          if(a.test_drop_frame>=0 && frame==(uint32_t)a.test_drop_frame && seq==0)continue;
          size_t off=seq*payload,n=std::min(payload,bytes.size()-off);memcpy(eth.data()+42,bytes.data()+off,n);
          vc_pkt_hdr_t h={};h.magic=VC_MAGIC;h.frame_id=frame;h.seq=(uint16_t)seq;h.total=(uint16_t)count;
          h.qpc_ns=stamp;h.payload_crc=vc_crc32(eth.data()+42,n);h.payload_len=(uint16_t)n;
          h.flags=frame==0?1:0;h.pattern=a.mode=="pattern"?(uint8_t)pat_id(a.pattern):255;memcpy(eth.data()+14,&h,sizeof(h));
          if(queue){PcapHdr p={};p.caplen=p.len=(unsigned)(42+n);if(d.sendqueue_queue(queue,&p,eth.data()))throw std::runtime_error("encoded sendqueue overflow");if(++queued>=a.batch)flush();}
          else if(d.sendpacket(handle,eth.data(),(int)(42+n))<0)throw std::runtime_error("encoded sendpacket failed");
          packets++;wire_bytes+=42+n;if(a.pacing_us)sleep_us(a.pacing_us);
        }
        flush();
      }catch(...){throw;}
    };
    auto stats=[&](bool running){
      double seconds=std::max(.001,(qpc_ns_now()-started)/1e9);
      char text[2048];snprintf(text,sizeof(text),
        "{\"running\":%s,\"codec\":\"%s\",\"width\":%d,\"height\":%d,\"fps_limit\":%d,\"sent_frames\":%llu,\"sent_packets\":%llu,\"send_errors\":0,"
        "\"encoded_bytes\":%llu,\"keyframes\":%llu,\"bps\":%.1f,\"pps\":%.1f,\"fps_actual\":%.3f,\"uptime_s\":%.3f,\"rate_wait_ms\":%.3f,"
        "\"bitrate\":%d,\"max_bitrate\":%d,\"max_au_bytes\":%d,\"gop\":%d,\"source_skipped\":%llu,\"encoder_in_flight\":1,\"pixel_equality_gate\":false",
        running?"true":"false",a.codec.codec.c_str(),a.stream.width,a.stream.height,a.fps,
        (unsigned long long)frames,(unsigned long long)packets,(unsigned long long)encoded_bytes,(unsigned long long)keyframes,
        wire_bytes*8/seconds,packets/seconds,frames/seconds,seconds,rate_wait_ns/1e6,a.codec.bitrate,a.codec.max_bitrate,a.codec.max_au,a.codec.gop,(unsigned long long)cap.source_skipped());
      atomic_write(a.stats,std::string(text)+","+send_segment.fields("pc1_seg","source_ready_to_transmit_complete")+",\"encoder_quality\":"+encoder.quality_json()+",\"encoder_configuration\":"+encoder.settings()+",\"preparation\":"+preparation.json(limit.period)+",\"timing_ms\":"+timing.json()+"}");last_stats=qpc_ns_now();
    };
    stats(true);
    size_t file_index=0;
    std::ifstream file;if(a.mode=="file"){file.open(a.file,std::ios::binary);if(!file)throw std::runtime_error("encoded file open failed");}
    while(!g_stop && (a.frames==0 || frames<(uint64_t)a.frames)){
      uint64_t begin_ns=qpc_ns_now();
      bool request_key=fid%a.codec.gop==0;
      uint64_t prepare_ns=preparation.lead(request_key,limit.period);
      uint64_t acquire_at=limit.next>prepare_ns?limit.next-prepare_ns:0;
      encoded_wait_until(acquire_at);
      if(g_stop)break;
      if(fid==UINT32_MAX)throw std::runtime_error("encoded session frame id exhausted; restart the session");
      capture_ns=qpc_ns_now();bool fresh=true;
      uint64_t prepare_wait_ns=capture_ns-begin_ns;
      if(a.mode=="pattern")gen_full(pat_id(a.pattern),fid,raw.data(),raw.data()+a.stream.y_bytes(),a.stream.width,a.stream.height);
      else if(a.mode=="dda" || a.mode=="wgc"){
        fresh=cap.capture(raw.data(),raw.data()+a.stream.y_bytes());
        if(!fresh && cap.err_code()!=DDACap::E_TIMEOUT)throw std::runtime_error(cap.err());
      }else{
        file.read((char*)raw.data(),raw.size());
        if((size_t)file.gcount()!=raw.size()){
          if(!a.loop)break;file.clear();file.seekg(0);file.read((char*)raw.data(),raw.size());
          if((size_t)file.gcount()!=raw.size())throw std::runtime_error("encoded file contains no full NV12 frame");
        }
      }
      if(!fresh){if(qpc_ns_now()-last_stats>500000000)stats(true);continue;}
      uint64_t generated_ns=qpc_ns_now();
      borrowed=nullptr;borrowed_size=0;
      encoder.submit(raw.data(),raw.size(),(LONGLONG)fid*(10000000LL/a.fps),10000000LL/a.fps);
      uint64_t submitted_ns=qpc_ns_now(),deadline=submitted_ns+5000000000ull;
      while(!borrowed){
        encoder.pump();if(borrowed)break;
        if(qpc_ns_now()>deadline)throw std::runtime_error("one-in-flight encoder output timeout (5s)");encoder.wait_for_event();
      }
      uint64_t encoded_ns=qpc_ns_now();
      const uint8_t* output=borrowed;size_t output_size=borrowed_size;
      if(clean && !vcau::random_access(output,output_size,a.codec.id()))throw std::runtime_error("clean point is missing a complete independent codec frame");
      if(fid%a.codec.gop==0 && !clean)throw std::runtime_error("explicit IDR request not honored");
      auto& bytes=reused_au;
      uint64_t packed_hash=vcau::pack_into(bytes,output,output_size,fid,clean,false,a.stream,a.codec.id());
      // Strict rolling one-second compressed-payload cap; a giant IDR is rejected, never silently exempted.
      const size_t budget=(size_t)a.codec.max_bitrate/8;
      if(bytes.size()>budget)throw std::runtime_error("one access unit exceeds one-second max-bitrate budget");
      uint64_t wait_start=qpc_ns_now();
      // Predict IDR and inter frames separately; never pre-capture more than one period or queue a second input.
      uint64_t prepare_work_ns=((a.mode=="dda"||a.mode=="wgc")?qpc_to_ns_raw(cap.last_convert_qpc()):generated_ns-capture_ns)+wait_start-generated_ns;
      preparation.observe(request_key,prepare_work_ns);
      for(;;){
        uint64_t now=qpc_ns_now();while(!rate_window.empty() && now-rate_window.front().first>=1000000000ull){window_bytes-=rate_window.front().second;rate_window.pop_front();}
        if(window_bytes+bytes.size()<=budget && limit.ready(now))break;
        if(g_stop)break;
        uint64_t ready_at=limit.next;
        if(window_bytes+bytes.size()>budget)ready_at=std::max(ready_at,rate_window.front().first+1000000000ull);
        encoded_wait_until(ready_at);
      }
      if(g_stop)break;
      rate_wait_ns+=qpc_ns_now()-wait_start;
      uint64_t send_ns=qpc_ns_now();limit.sent(send_ns);transmit(bytes,fid,capture_ns);
      uint64_t transmitted_ns=qpc_ns_now();
      send_segment.add(transmitted_ns-generated_ns);
      rate_window.emplace_back(send_ns,bytes.size());window_bytes+=bytes.size();
      char source_hash[17],transport_hash[17];vc_fnv1a64_hex(vc_fnv1a64(raw.data(),raw.size()),source_hash);
      vc_fnv1a64_hex(packed_hash,transport_hash);
      uint64_t audited_ns=qpc_ns_now();
      hashlog<<"{\"frame_id\":"<<fid<<",\"source_hash\":\""<<source_hash<<"\",\"transport_hash\":\""<<transport_hash
        <<"\",\"encoded_bytes\":"<<output_size<<",\"keyframe\":"<<(clean?"true":"false")
        <<",\"fault_injected\":"<<(a.test_drop_frame==(long long)fid?"true":"false")<<",\"qpc_ns\":"<<capture_ns<<",\"send_qpc_ns\":"<<send_ns
        <<",\"encode_wait_ns\":"<<encoded_ns-submitted_ns<<",\"prepare_lead_ns\":"<<prepare_ns<<",\"prepare_work_ns\":"<<prepare_work_ns<<"}\n";
      hashlog.flush();encoded_bytes+=output_size;keyframes+=clean;frames++;fid++;borrowed=nullptr;
      timing.add(VCEncodedTimings::PrepareWait,prepare_wait_ns);
      timing.add(VCEncodedTimings::Source,generated_ns-capture_ns);
      timing.add(VCEncodedTimings::Submit,submitted_ns-generated_ns);
      timing.add(VCEncodedTimings::EncodeWait,encoded_ns-submitted_ns);
      timing.add(VCEncodedTimings::Pack,wait_start-encoded_ns);
      timing.add(VCEncodedTimings::RateWait,send_ns-wait_start);
      timing.add(VCEncodedTimings::Transmit,transmitted_ns-send_ns);
      timing.add(VCEncodedTimings::Audit,audited_ns-transmitted_ns);
      timing.add(VCEncodedTimings::Log,qpc_ns_now()-audited_ns);
      timing.add(VCEncodedTimings::CaptureToSend,send_ns-capture_ns);
      if(qpc_ns_now()-last_stats>500000000)stats(true);
    }
    encoder.drain();
    vcau::Unit eos;eos.fid=fid;eos.eos=true;transmit(vcau::pack(eos,a.stream,a.codec.id()),fid,qpc_ns_now());
    stats(false);printf("[encoded sender] codec=%s frames=%llu keyframes=%llu\n",a.codec.codec.c_str(),(unsigned long long)frames,(unsigned long long)keyframes);
    return 0;
  }catch(const std::exception& error){fprintf(stderr,"encoded sender failed: %s\n",error.what());return 1;}
}
