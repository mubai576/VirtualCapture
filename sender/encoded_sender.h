#pragma once
#include <memory>

static int run_encoded_sender(const Args& a,DynPcap& d,pcap_t* handle,DDACap& dda){
  try{
    VCMFRuntime mf;
    vcmf::Transform encoder;
    std::vector<uint8_t> raw(a.stream.frame_bytes()),compressed;
    uint64_t started=qpc_ns_now(),last_stats=0,frames=0,packets=0,wire_bytes=0,encoded_bytes=0,keyframes=0,rate_wait_ns=0;
    uint32_t fid=0;uint64_t capture_ns=0,prepare_ns=0;bool clean=false;
    std::deque<std::pair<uint64_t,size_t>> rate_window;size_t window_bytes=0;
    VCFrameLimit limit(a.fps);
    ensure_parent(a.hashlog);std::ofstream hashlog(a.hashlog,std::ios::binary|std::ios::trunc);
    if(!hashlog)throw std::runtime_error("cannot open encoded sender hashlog");
    a.codec.open(encoder,a.stream,true,[&](IMFSample* sample){
      LONGLONG stamp=0;vcmf::check(sample->GetSampleTime(&stamp),"encoded timestamp");
      if(stamp!=(LONGLONG)fid*(10000000LL/a.fps) || !compressed.empty())throw std::runtime_error("encoder reordered/buffered more than one frame");
      UINT32 flag=0;sample->GetUINT32(MFSampleExtension_CleanPoint,&flag);clean=flag!=0;
      vcmf::ComPtr<IMFMediaBuffer> buffer;vcmf::check(sample->ConvertToContiguousBuffer(&buffer),"encoded bytes");
      BYTE* data=nullptr;DWORD size=0;vcmf::check(buffer->Lock(&data,nullptr,&size),"encoded Lock");
      if(!size || size+vcau::header_bytes>(size_t)a.codec.max_au){buffer->Unlock();throw std::runtime_error("access unit exceeds explicit max-au-bytes");}
      try{compressed.assign(data,data+size);}catch(...){buffer->Unlock();throw;}buffer->Unlock();
    });
    uint8_t source[6];vc_parse_mac(a.source_mac,source);
    const size_t payload=a.mtu-42;
    auto transmit=[&](const std::vector<uint8_t>& bytes,uint32_t frame,uint64_t stamp){
      size_t count=(bytes.size()+payload-1)/payload;
      std::vector<uint8_t> eth(42+payload);memcpy(eth.data(),a.dstmac,6);memcpy(eth.data()+6,source,6);
      eth[12]=(uint8_t)(a.ethertype>>8);eth[13]=(uint8_t)a.ethertype;
      void* queue=nullptr;int queued=0;
      if(a.batch>1){
        if(!d.sendqueue_alloc || !d.sendqueue_queue || !d.sendqueue_transmit || !d.sendqueue_destroy)throw std::runtime_error("missing sendqueue exports");
        queue=d.sendqueue_alloc((unsigned)(std::min(count,(size_t)a.batch)*(sizeof(PcapHdr)+42+payload)));
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
      }catch(...){if(queue)d.sendqueue_destroy(queue);throw;}
      if(queue)d.sendqueue_destroy(queue);
    };
    auto stats=[&](bool running){
      double seconds=std::max(.001,(qpc_ns_now()-started)/1e9);
      char text[2048];snprintf(text,sizeof(text),
        "{\"running\":%s,\"codec\":\"%s\",\"width\":%d,\"height\":%d,\"fps_limit\":%d,\"sent_frames\":%llu,\"sent_packets\":%llu,\"send_errors\":0,"
        "\"encoded_bytes\":%llu,\"keyframes\":%llu,\"bps\":%.1f,\"pps\":%.1f,\"fps_actual\":%.3f,\"uptime_s\":%.3f,\"rate_wait_ms\":%.3f,"
        "\"bitrate\":%d,\"max_bitrate\":%d,\"max_au_bytes\":%d,\"gop\":%d,\"source_skipped\":%llu,\"encoder_in_flight\":1,\"pixel_equality_gate\":false}",
        running?"true":"false",a.codec.codec.c_str(),a.stream.width,a.stream.height,a.fps,
        (unsigned long long)frames,(unsigned long long)packets,(unsigned long long)encoded_bytes,(unsigned long long)keyframes,
        wire_bytes*8/seconds,packets/seconds,frames/seconds,seconds,rate_wait_ns/1e6,a.codec.bitrate,a.codec.max_bitrate,a.codec.max_au,a.codec.gop,(unsigned long long)dda.source_skipped);
      atomic_write(a.stats,text);last_stats=qpc_ns_now();
    };
    stats(true);
    size_t file_index=0;
    std::ifstream file;if(a.mode=="file"){file.open(a.file,std::ios::binary);if(!file)throw std::runtime_error("encoded file open failed");}
    while(!g_stop && (a.frames==0 || frames<(uint64_t)a.frames)){
      uint64_t acquire_at=limit.next>prepare_ns?limit.next-prepare_ns:0;
      while(qpc_ns_now()<acquire_at && !g_stop)Sleep(1);
      if(g_stop)break;
      if(fid==UINT32_MAX)throw std::runtime_error("encoded session frame id exhausted; restart the session");
      capture_ns=qpc_ns_now();bool fresh=true;
      if(a.mode=="pattern")gen_full(pat_id(a.pattern),fid,raw.data(),raw.data()+a.stream.y_bytes(),a.stream.width,a.stream.height);
      else if(a.mode=="dda"){
        fresh=dda.capture(raw.data(),raw.data()+a.stream.y_bytes());
        if(!fresh && dda.err_code!=DDACap::E_TIMEOUT)throw std::runtime_error(dda.err());
      }else{
        file.read((char*)raw.data(),raw.size());
        if((size_t)file.gcount()!=raw.size()){
          if(!a.loop)break;file.clear();file.seekg(0);file.read((char*)raw.data(),raw.size());
          if((size_t)file.gcount()!=raw.size())throw std::runtime_error("encoded file contains no full NV12 frame");
        }
      }
      if(!fresh){if(qpc_ns_now()-last_stats>500000000)stats(true);continue;}
      uint64_t generated_ns=qpc_ns_now();
      compressed.clear();if(fid%a.codec.gop==0)encoder.force_keyframe();
      auto sample=vcmf::sample(raw.data(),raw.size(),(LONGLONG)fid*(10000000LL/a.fps),10000000LL/a.fps);
      encoder.submit(sample.Get());uint64_t deadline=qpc_ns_now()+5000000000ull;
      while(compressed.empty()){encoder.pump();if(qpc_ns_now()>deadline)throw std::runtime_error("one-in-flight encoder output timeout (5s)");Sleep(1);}
      prepare_ns=std::min(limit.period/2,(a.mode=="dda"?qpc_to_ns_raw(dda.last_convert_qpc):generated_ns-capture_ns)+qpc_ns_now()-generated_ns);
      if(clean && !vcau::random_access(compressed,a.codec.id()))throw std::runtime_error("clean point is missing Annex-B IDR/parameter sets");
      if(fid%a.codec.gop==0 && !clean)throw std::runtime_error("explicit IDR request not honored");
      vcau::Unit unit;unit.fid=fid;unit.key=clean;unit.bytes=std::move(compressed);
      auto bytes=vcau::pack(unit,a.stream,a.codec.id());
      // Strict rolling one-second compressed-payload cap; a giant IDR is rejected, never silently exempted.
      const size_t budget=(size_t)a.codec.max_bitrate/8;
      if(bytes.size()>budget)throw std::runtime_error("one access unit exceeds one-second max-bitrate budget");
      uint64_t wait_start=qpc_ns_now();
      for(;;){
        uint64_t now=qpc_ns_now();while(!rate_window.empty() && now-rate_window.front().first>=1000000000ull){window_bytes-=rate_window.front().second;rate_window.pop_front();}
        if(window_bytes+bytes.size()<=budget && limit.ready(now))break;
        if(g_stop)break;Sleep(1);
      }
      if(g_stop)break;
      rate_wait_ns+=qpc_ns_now()-wait_start;
      uint64_t send_ns=qpc_ns_now();limit.sent(send_ns);transmit(bytes,fid,capture_ns);
      rate_window.emplace_back(send_ns,bytes.size());window_bytes+=bytes.size();
      char source_hash[17],transport_hash[17];vc_fnv1a64_hex(vc_fnv1a64(raw.data(),raw.size()),source_hash);
      vc_fnv1a64_hex(vc_fnv1a64(unit.bytes.data(),unit.bytes.size()),transport_hash);
      hashlog<<"{\"frame_id\":"<<fid<<",\"source_hash\":\""<<source_hash<<"\",\"transport_hash\":\""<<transport_hash
        <<"\",\"encoded_bytes\":"<<unit.bytes.size()<<",\"keyframe\":"<<(clean?"true":"false")
        <<",\"fault_injected\":"<<(a.test_drop_frame==(long long)fid?"true":"false")<<",\"qpc_ns\":"<<capture_ns<<",\"send_qpc_ns\":"<<send_ns<<"}\n";
      hashlog.flush();encoded_bytes+=unit.bytes.size();keyframes+=clean;frames++;fid++;
      if(qpc_ns_now()-last_stats>500000000)stats(true);
    }
    encoder.drain();if(!compressed.empty())throw std::runtime_error("unexpected delayed encoder output");
    vcau::Unit eos;eos.fid=fid;eos.eos=true;transmit(vcau::pack(eos,a.stream,a.codec.id()),fid,qpc_ns_now());
    stats(false);printf("[encoded sender] codec=%s frames=%llu keyframes=%llu\n",a.codec.codec.c_str(),(unsigned long long)frames,(unsigned long long)keyframes);
    return 0;
  }catch(const std::exception& error){fprintf(stderr,"encoded sender failed: %s\n",error.what());return 1;}
}
