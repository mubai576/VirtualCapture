#define main vc_receiver_program_main
#include "../../../receiver/vc_receiver.cpp"
#undef main

namespace {
std::vector<std::vector<uint8_t>> wire;
size_t next_packet=0;
void require(bool good){if(!good)throw std::runtime_error("MJPEG receiver buffer regression failed");}
int next(void*,void** header,void* packet){
  if(next_packet==wire.size()){Sleep(1);return 0;}
  auto& eth=wire[next_packet++];vc_pkt_hdr_t h;memcpy(&h,eth.data()+14,sizeof(h));
  if(h.seq==0)Sleep(20);
  static PcapHdr capture={};capture.caplen=capture.len=(unsigned)eth.size();
  *header=&capture;*static_cast<void**>(packet)=eth.data();return 1;
}
void append(vcau::Unit& unit,const VCStream& stream){
  auto bytes=vcau::pack(unit,stream,3);size_t payload=8958,count=(bytes.size()+payload-1)/payload;
  for(size_t seq=0;seq<count;seq++){
    size_t offset=seq*payload,size=std::min(payload,bytes.size()-offset);
    std::vector<uint8_t> eth(42+size);eth[12]=0x88;eth[13]=0xb5;
    vc_pkt_hdr_t h={};h.magic=VC_MAGIC;h.frame_id=unit.fid;h.seq=(uint16_t)seq;h.total=(uint16_t)count;
    h.payload_len=(uint16_t)size;h.payload_crc=vc_crc32(bytes.data()+offset,size);h.pattern=4;
    memcpy(eth.data()+14,&h,sizeof(h));memcpy(eth.data()+42,bytes.data()+offset,size);wire.push_back(std::move(eth));
  }
}
}

int main(int argc,char** argv){
  try{
    require(argc==2);qpc_init();const std::string root=argv[1];VCStream stream;
    std::vector<uint8_t> raw(stream.frame_bytes()),jpeg;
    vc_gen_motion(0,raw.data(),raw.data()+stream.y_bytes(),stream.width,stream.height);
    vcjpeg::Codec encoder,decoder;encoder.open(stream,true,80,8*1024*1024);decoder.open(stream,false,80,8*1024*1024);
    encoder.compress(raw.data(),raw.size(),[&](const uint8_t* data,size_t size){jpeg.assign(data,data+size);});
    auto valid=decoder.decompress(jpeg.data(),jpeg.size());char expected_hash[17];
    vc_fnv1a64_hex(vc_fnv1a64(valid.data(),valid.size()),expected_hash);
    size_t sos=0;
    for(size_t i=0;i+4<jpeg.size();i++)if(jpeg[i]==0xff && jpeg[i+1]==0xda){sos=i;break;}
    require(sos>0);size_t cut=sos+2+((size_t)jpeg[sos+2]<<8)+jpeg[sos+3]+128;
    require(cut+2<jpeg.size());auto broken=jpeg;broken.resize(cut);broken.push_back(0xff);broken.push_back(0xd9);
    bool rejected=false;try{decoder.decompress_into(broken.data(),broken.size(),raw.data(),raw.size());}
    catch(const std::exception&){rejected=true;}require(rejected);
    const uint32_t failures=VC_SHM_SLOTS+5;
    vcau::Unit unit;unit.key=true;unit.bytes=jpeg;append(unit,stream);
    for(uint32_t i=1;i<=failures;i++){unit.fid=i;unit.bytes=broken;append(unit,stream);}
    unit.fid=failures+1;unit.bytes=jpeg;append(unit,stream);
    unit.fid++;unit.bytes.clear();unit.key=false;unit.eos=true;append(unit,stream);
    Args args;args.codec.codec="mjpeg";args.codec.backend="turbojpeg:cpu";args.codec.gop=1;args.codec.in_flight=1;
    args.stats=root+"/receiver-stats.json";args.hashlog=root+"/receiver-hash.jsonl";args.timeout_s=4;args.expect_frames=2;
    DynPcapRx pcap;pcap.nextex=next;require(run_encoded_receiver(args,pcap,nullptr)==0);
    std::ifstream stats_file(args.stats);std::string stats((std::istreambuf_iterator<char>(stats_file)),{});
    for(const auto& value:std::vector<std::string>{"\"complete\":2,","\"decode_errors\":"+std::to_string(failures)+",",
        "\"decode_resets\":"+std::to_string(failures)+",","\"decode_queue_drops\":0,","\"decode_pending\":0,","\"failed\":false"})
      require(stats.find(value)!=std::string::npos);
    std::ifstream audit(args.hashlog);std::string line;size_t logged=0;
    while(std::getline(audit,line)){
      require(logged<2);uint32_t fid=logged?failures+1:0;
      require(line.find("\"frame_id\":"+std::to_string(fid)+",")!=std::string::npos &&
              line.find("\"decoded_hash\":\""+std::string(expected_hash)+"\"")!=std::string::npos);logged++;
    }
    require(logged==2);
    puts("PASS: actual receiver worker, valid CRC/envelope with truncated JPEG entropy, abandoned direct slots beyond pool capacity, no partial output, next JPEG recovers exact pixels");
    return 0;
  }catch(const std::exception& e){fprintf(stderr,"%s\n",e.what());return 1;}
}
