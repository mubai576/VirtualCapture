#define main vc_sender_program_main
#include "../../../sender/vc_sender.cpp"
#undef main

// Exercise the actual sender with in-memory Npcap exports, never a real adapter.
namespace {
struct Queue {unsigned capacity=0,len=0;std::vector<std::vector<uint8_t>> packets;};
int allocated=0,freed=0,attempts=0,sent=0;
std::string fault;
std::vector<std::vector<uint8_t>> wire;
void require(bool good){if(!good)throw std::runtime_error("MJPEG buffer regression failed");}
void* allocate(unsigned capacity){
  attempts++;
  if(fault=="allocate")return nullptr;
  auto* q=new Queue;q->capacity=capacity;allocated++;return q;
}
void destroy(void* value){require(value!=nullptr);freed++;delete static_cast<Queue*>(value);}
int enqueue(void* value,const void* header,const unsigned char* data){
  if(fault=="enqueue")return -1;
  auto* q=static_cast<Queue*>(value);auto* h=static_cast<const PcapHdr*>(header);
  require(h->caplen==h->len && q->len+sizeof(*h)+h->caplen<=q->capacity);
  if(!q->len)q->packets.clear();
  q->packets.emplace_back(data,data+h->caplen);q->len+=(unsigned)sizeof(*h)+h->caplen;return 0;
}
int transmit(pcap_t*,void* value,int sync){
  auto* q=static_cast<Queue*>(value);require(sync==0 && q->len>0);
  if(fault=="short")return q->len-1;
  for(const auto& p:q->packets)wire.push_back(p);
  sent++;return q->len;
}
int packet(pcap_t*,const unsigned char* data,int size){
  if(fault=="packet")return -1;
  wire.emplace_back(data,data+size);sent++;return 0;
}
}

int main(int argc,char** argv){
  try{
    require(argc==2);qpc_init();const std::string root=argv[1];
    VCStream stream;const size_t size=stream.frame_bytes();
    std::vector<std::vector<uint8_t>> source(6,std::vector<uint8_t>(size,128)),expected(6);
    for(size_t fid=0;fid<source.size();fid++){
      if(fid%2)vc_gen_noise((uint32_t)fid,source[fid].data(),source[fid].data()+stream.y_bytes(),stream.width,stream.height);
    }
    vcjpeg::Codec encoder;encoder.open(stream,true,80,8*1024*1024);
    std::ofstream file(root+"/frames.nv12",std::ios::binary);
    for(size_t fid=0;fid<source.size();fid++){
      file.write((const char*)source[fid].data(),size);
      encoder.compress(source[fid].data(),size,[&](const uint8_t* data,size_t n){expected[fid].assign(data,data+n);});
    }
    file.close();require(expected[1].size()>expected[0].size()*2);
    for(const auto& mode:std::vector<std::pair<int,std::string>>{{1,""},{69,""},{69,"short"},{69,"allocate"},{69,"enqueue"},{1,"packet"}}){
      allocated=freed=attempts=sent=0;wire.clear();fault=mode.second;g_stop=false;
      DynPcap pcap;pcap.sendqueue_alloc=allocate;pcap.sendqueue_destroy=destroy;
      pcap.sendqueue_queue=enqueue;pcap.sendqueue_transmit=transmit;pcap.sendpacket=packet;
      Args args;args.mode="file";args.file=root+"/frames.nv12";args.frames=(int)source.size();args.fps=1000;
      args.stream=stream;args.stream.fps=args.fps;args.batch=mode.first;
      args.stats=root+"/stats-"+std::to_string(mode.first)+fault+".json";
      args.hashlog=root+"/hash-"+std::to_string(mode.first)+fault+".jsonl";
      args.source_mac="02:01:02:03:04:05";
      args.codec.codec="mjpeg";args.codec.backend="turbojpeg:cpu";args.codec.gop=1;args.codec.max_bitrate=2000000000;
      CaptureHandle cap;
      int result=run_encoded_sender(args,pcap,nullptr,cap);
      require(allocated==freed);
      if(!fault.empty()){
        require(result==1);
        if(fault=="allocate")require(attempts==1 && allocated==0);
        else if(mode.first>1)require(allocated==1);
        continue;
      }
      require(result==0);
      if(mode.first>1)require(allocated>=2 && allocated<(int)source.size());
      else require(allocated==0);
      std::ifstream audit(args.hashlog);std::string line;size_t logged=0;
      while(std::getline(audit,line)){
        require(logged<source.size());char raw_hash[17],jpeg_hash[17];
        vc_fnv1a64_hex(vc_fnv1a64(source[logged].data(),size),raw_hash);
        vc_fnv1a64_hex(vc_fnv1a64(expected[logged].data(),expected[logged].size()),jpeg_hash);
        require(line.find("\"source_hash\":\""+std::string(raw_hash)+"\"")!=std::string::npos &&
                line.find("\"transport_hash\":\""+std::string(jpeg_hash)+"\"")!=std::string::npos &&
                line.find("\"encoded_bytes\":"+std::to_string(expected[logged].size())+",")!=std::string::npos);
        logged++;
      }
      require(logged==source.size());
      vcau::Assembler assembler(args.mtu-42,args.codec.max_au);
      std::vector<uint8_t> complete;uint64_t first=0;size_t frames=0;
      for(const auto& eth:wire){
        require(eth.size()>=42);vc_pkt_hdr_t h;memcpy(&h,eth.data()+14,sizeof(h));
        require(eth.size()==42+h.payload_len && ((eth[12]<<8)|eth[13])==args.ethertype);
        if(!assembler.accept(h,eth.data()+42,qpc_ns_now(),complete,first))continue;
        vcau::Unit unit;require(vcau::unpack(complete,h.frame_id,args.stream,3,args.codec.max_au,unit));
        if(unit.eos){require(unit.fid==source.size());continue;}
        require(unit.fid==frames && unit.bytes==expected[frames]);frames++;
      }
      require(frames==source.size() && assembler.units==frames+1 && assembler.bad==0);
    }
    puts("PASS: real MJPEG sender, alternating AU sizes, batch1/69, reused queue growth/EOS, short send/allocation/enqueue/packet errors, exact JPEG and CRC, no leaked queues");
    return 0;
  }catch(const std::exception& e){fprintf(stderr,"%s\n",e.what());return 1;}
}
