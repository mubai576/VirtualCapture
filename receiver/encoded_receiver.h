#pragma once
#include <memory>

static int run_encoded_receiver(const Args& a,DynPcapRx& d,void* handle){
  FramePool pool(a.stream);
  if(!a.shm.empty() && !pool.open_shm(a.shm)){fprintf(stderr,"encoded SHM unavailable (close old readers), err=%lu\n",GetLastError());return 1;}
  VstatReader vst;if(!a.shm.empty())vst.name=a.shm+".vcam";
  struct Work {vcau::Unit unit;bool discontinuity=false;};
  std::mutex mutex;std::condition_variable cv;std::deque<Work> queue;
  bool stopping=false,discontinuity=false;std::atomic<bool> failed{false},ready{false},waiting{true};
  std::atomic<uint64_t> decoded{0},resets{0},discarded{0},decode_errors{0},queue_drops{0},pending_count{0};
  std::string error,network_error;
  ensure_parent(a.hashlog);
  std::ofstream transport(a.hashlog+".transport.jsonl",std::ios::binary|std::ios::trunc);
  if(!transport){fprintf(stderr,"cannot open transport log\n");return 1;}
  std::thread worker([&]{
    try{
      VCMFRuntime mf;
      std::unique_ptr<vcmf::Transform> decoder;
      struct Metadata{uint64_t hash,first;};std::map<uint32_t,Metadata> pending;
      std::ofstream log(a.hashlog,std::ios::binary|std::ios::trunc);
      if(!log)throw std::runtime_error("cannot open decoded hashlog");
      auto reopen=[&]{
        decoder.reset();discarded+=pending.size();pending.clear();pending_count=0;
        decoder=std::make_unique<vcmf::Transform>();waiting=true;
        a.codec.open(*decoder,a.stream,false,[&](IMFSample* sample){
          LONGLONG timestamp=0;vcmf::check(sample->GetSampleTime(&timestamp),"decoded timestamp");
          const LONGLONG duration=10000000LL/a.stream.fps;
          if(timestamp<0 || timestamp%duration || (uint64_t)(timestamp/duration)>UINT32_MAX)throw std::runtime_error("invalid decoded timestamp");
          uint32_t fid=(uint32_t)(timestamp/duration);auto it=pending.find(fid);
          if(it==pending.end() || it!=pending.begin())throw std::runtime_error("decoded frame order/timestamp mismatch");
          auto bytes=decoder->packed_nv12(sample);int slot=pool.acquire();
          if(slot<0)throw std::runtime_error("decoded frame pool exhausted");
          memcpy(pool.data(slot),bytes.data(),bytes.size());uint64_t now=qpc_ns_now();
          pool.publish(slot,fid,it->second.first,now);
          uint64_t hash=vc_fnv1a64(bytes.data(),bytes.size());pool.hashed(slot,fid,hash);
          char hx[17],tx[17];vc_fnv1a64_hex(hash,hx);vc_fnv1a64_hex(it->second.hash,tx);
          log<<"{\"frame_id\":"<<fid<<",\"decoded_hash\":\""<<hx<<"\",\"transport_hash\":\""<<tx
            <<"\",\"qpc_ns\":"<<now<<",\"decode_pipeline_ms\":"<<(now-it->second.first)/1e6<<"}\n";log.flush();
          pending.erase(it);pending_count=pending.size();decoded++;
        });
      };
      reopen();ready=true;bool have_last=false;uint32_t last=0;
      for(;;){
        Work work;
        {std::unique_lock<std::mutex> lock(mutex);cv.wait_for(lock,std::chrono::milliseconds(2),[&]{return stopping || !queue.empty();});
          if(queue.empty()){if(stopping)break;lock.unlock();decoder->pump();continue;}
          work=std::move(queue.front());queue.pop_front();}
        auto& unit=work.unit;
        if(work.discontinuity || (have_last && unit.fid!=last+1)){reopen();resets++;}
        have_last=true;last=unit.fid;
        if(unit.eos){decoder->drain();if(!pending.empty())throw std::runtime_error("EOS drain left undecoded frames");continue;}
        if(waiting && !unit.key){discarded++;continue;}
        if(waiting)waiting=false;
        pending.emplace(unit.fid,Metadata{unit.hash,unit.first_ns});pending_count=pending.size();
        auto input=vcmf::sample(unit.bytes.data(),unit.bytes.size(),(LONGLONG)unit.fid*(10000000LL/a.stream.fps),10000000LL/a.stream.fps);
        if(unit.key)input->SetUINT32(MFSampleExtension_CleanPoint,TRUE);
        try{decoder->submit(input.Get());decoder->pump();}
        catch(const std::exception& failure){fprintf(stderr,"decode failure, waiting for next IDR: %s\n",failure.what());decode_errors++;reopen();resets++;continue;}
        uint64_t deadline=qpc_ns_now()+5000000000ull;
        while(pending.size()>=(size_t)a.codec.in_flight){
          decoder->pump();if(pending.size()<(size_t)a.codec.in_flight)break;
          if(qpc_ns_now()>deadline)throw std::runtime_error("decoder exceeded explicit decode-in-flight limit (5s); no queue-depth fallback");
          Sleep(1);
        }
      }
      decoder->drain();if(!pending.empty())throw std::runtime_error("final drain left undecoded frames");
    }catch(const std::exception& failure){error=failure.what();failed=true;ready=true;cv.notify_all();}
  });
  uint64_t started=qpc_ns_now(),last_stats=0;vcau::Assembler assembler(a.mtu-42,a.codec.max_au);
  uint64_t invalid_au=0,valid_units=0;std::vector<uint8_t> complete;bool received_eos=false;
  auto stats=[&](bool running){
    double seconds=std::max(.001,(qpc_ns_now()-started)/1e9);char text[2048];
    snprintf(text,sizeof(text),"{\"running\":%s,\"codec\":\"%s\",\"width\":%d,\"height\":%d,\"fps_limit\":%d,"
      "\"complete\":%llu,\"compressed_complete\":%llu,\"dropped\":%llu,\"packets_recv\":%llu,\"packets_lost\":%llu,\"transport_errors\":%llu,"
      "\"decode_errors\":%llu,\"decode_resets\":%llu,\"decode_discarded\":%llu,\"decode_queue_drops\":%llu,\"decode_pending\":%llu,\"decode_in_flight_limit\":%d,\"waiting_idr\":%s,"
      "\"bps\":%.1f,\"pps\":%.1f,\"uptime_s\":%.3f,\"shm_published\":%llu,\"pixel_equality_gate\":false,\"failed\":%s}",
      running?"true":"false",a.codec.codec.c_str(),a.stream.width,a.stream.height,a.stream.fps,(unsigned long long)decoded.load(),
      (unsigned long long)valid_units,(unsigned long long)assembler.dropped,(unsigned long long)assembler.packets,(unsigned long long)(assembler.lost+assembler.bad),
      (unsigned long long)(invalid_au+assembler.bad),(unsigned long long)decode_errors.load(),(unsigned long long)resets.load(),(unsigned long long)discarded.load(),
      (unsigned long long)queue_drops.load(),(unsigned long long)pending_count.load(),a.codec.in_flight,waiting?"true":"false",assembler.bytes*8/seconds,assembler.packets/seconds,seconds,
      (unsigned long long)(a.shm.empty()?0:decoded.load()),failed?"true":"false");
    vst.poll();std::string output=text;output.pop_back();output+=","+vseg_json(vst,0)+"}";
    atomic_write(a.stats,output);last_stats=qpc_ns_now();
  };
  uint8_t mac[6]={};vc_parse_mac(a.source_mac,mac);
  while(!ready && !failed)Sleep(1);
  stats(!failed);
  while(!g_stop && !failed){
    uint64_t now=qpc_ns_now();
    if(a.timeout_s>0 && now-started>=(uint64_t)a.timeout_s*1000000000ull)break;
    if(a.expect_frames>0 && decoded>=(uint64_t)a.expect_frames)break;
    void* ph=nullptr;void* pd=nullptr;int rc=d.nextex(handle,&ph,&pd);
    if(rc<0){network_error="pcap receive failed";failed=true;break;}
    if(rc==1 && ph && pd){
      auto* hdr=(PcapHdr*)ph;auto* eth=(uint8_t*)pd;
      if(hdr->caplen>=42 && ((eth[12]<<8)|eth[13])==a.ethertype && (a.source_mac.empty() || !memcmp(eth+6,mac,6))){
        vc_pkt_hdr_t h;memcpy(&h,eth+14,sizeof(h));uint64_t first=0;
        if(h.payload_len<=hdr->caplen-42 && assembler.accept(h,eth+42,now,complete,first)){
          vcau::Unit unit;
          if(!vcau::unpack(complete,h.frame_id,a.stream,a.codec.id(),a.codec.max_au,unit) || (unit.key && !vcau::random_access(unit.bytes,a.codec.id()))){invalid_au++;discontinuity=true;}
          else{
            unit.first_ns=first;char hash[17];vc_fnv1a64_hex(unit.hash,hash);
            if(received_eos){invalid_au++;discontinuity=true;continue;}
            if(!unit.eos){valid_units++;transport<<"{\"frame_id\":"<<unit.fid<<",\"transport_hash\":\""<<hash<<"\",\"encoded_bytes\":"<<unit.bytes.size()<<",\"keyframe\":"<<(unit.key?"true":"false")<<"}\n";transport.flush();}
            if(unit.eos)received_eos=true;
            {std::lock_guard<std::mutex> lock(mutex);
              if(queue.size()>=2){queue_drops+=queue.size();queue.clear();discontinuity=true;}
              queue.push_back(Work{std::move(unit),discontinuity});discontinuity=false;}
            cv.notify_one();
          }
        }
      }
    }
    if(now-last_stats>500000000)stats(true);
  }
  {std::lock_guard<std::mutex> lock(mutex);stopping=true;}cv.notify_all();worker.join();assembler.finish();stats(false);
  if(failed){fprintf(stderr,"encoded receiver failed: %s %s\n",network_error.c_str(),error.c_str());return 1;}
  printf("[encoded receiver] decoded=%llu resets=%llu discarded=%llu\n",(unsigned long long)decoded.load(),(unsigned long long)resets.load(),(unsigned long long)discarded.load());return 0;
}
