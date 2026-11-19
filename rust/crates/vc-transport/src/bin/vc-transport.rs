//! `vc-transport` 诊断工具：离线检查/回放经典 pcap 抓包。
//!
//! 不打开网卡、不依赖 Npcap：读取现有 `.pcap`（以太网链路类型），用与 C++ 相同的
//! 冻结包头/CRC/重组逻辑统计完成帧和整帧哈希。适合把任一端 dump 的实包拿来做
//! 跨语言对账；pcapng 会明确拒绝。
//!
//! 例子：
//! `vc-transport inspect run/capture.pcap`
//! `vc-transport replay run/capture.pcap --width 640 --height 640 --mtu 9000 --framewise`

use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use vc_transport::pcap::{FileHeader, Packet, PcapReader};
use vc_transport::{
    Config, ETHERNET_HEADER_LEN, ETHERTYPE, Geometry, HEADER_LEN, Header, IgnoreReason,
    PushOutcome, Reassembler, Stats, TimingSummary,
};

const DEFAULT_MAX_PACKETS: u64 = 20_000_000;
const MAX_LISTED_MACS: usize = 16;
const MAX_LISTED_TOTALS: usize = 8;

const USAGE: &str = "\
vc-transport — VirtualCapture 抓包诊断（经典 pcap，Ethernet）

用法:
  vc-transport inspect <capture.pcap> [--json] [--max-packets N]
  vc-transport replay  <capture.pcap> --width W --height H --mtu N
                       [--source-mac aa:bb:cc:dd:ee:ff] [--framewise]
                       [--hash-out FILE] [--json] [--max-packets N]

inspect  只读统计：格式、包数、EtherType、源 MAC、VC 头概况
replay   按会话几何重组：完成帧/丢包/CRC/重复/迟到/帧哈希
         --framewise  逐帧输出（JSON 模式为 NDJSON）
         --hash-out   逐帧写 \"<frame_id> <fnv1a64-hex>\" 到文件

只支持经典 pcap（小端/大端、微秒/纳秒）和 LINKTYPE_ETHERNET(1)。
pcapng 需先转换。默认最多读取 20000000 包，单包捕获上限 1 MiB。
不打开网卡，不修改任何现有部署。";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("vc-transport: {message}");
            ExitCode::from(2)
        }
    }
}

#[derive(Default)]
struct Args {
    command: String,
    path: Option<PathBuf>,
    json: bool,
    framewise: bool,
    help: bool,
    hash_out: Option<PathBuf>,
    max_packets: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    mtu: Option<usize>,
    source_mac: Option<[u8; 6]>,
}

fn run() -> Result<(), String> {
    let mut args = parse_args(env::args().skip(1))?;
    if args.help || args.command.is_empty() {
        println!("{USAGE}");
        return Ok(());
    }
    let path = args
        .path
        .take()
        .ok_or("缺少 pcap 路径；用 --help 查看用法")?;
    args.max_packets = Some(args.max_packets.unwrap_or(DEFAULT_MAX_PACKETS));
    if args.max_packets == Some(0) {
        return Err("--max-packets 必须大于 0".into());
    }
    match args.command.as_str() {
        "inspect" => inspect(&path, &args),
        "replay" => replay(&path, &args),
        other => Err(format!("未知命令 {other}；用 --help 查看用法")),
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut positionals: Vec<String> = Vec::new();
    let mut command: Option<String> = None;
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => {
                (name.to_string(), Some(value.to_string()))
            }
            _ => (arg.clone(), None),
        };
        let mut value = |flag: &str| -> Result<String, String> {
            if let Some(value) = inline.clone() {
                if value.is_empty() {
                    return Err(format!("{flag} 需要非空值"));
                }
                return Ok(value);
            }
            args.next().ok_or_else(|| format!("{flag} 需要值"))
        };
        match name.as_str() {
            "-h" | "--help" => {
                require_no_value(&name, inline.as_deref())?;
                parsed.help = true;
            }
            "--json" => {
                require_no_value(&name, inline.as_deref())?;
                parsed.json = true;
            }
            "--framewise" => {
                require_no_value(&name, inline.as_deref())?;
                parsed.framewise = true;
            }
            "--width" => parsed.width = Some(parse_u32(&name, &value(&name)?)?),
            "--height" => parsed.height = Some(parse_u32(&name, &value(&name)?)?),
            "--mtu" => parsed.mtu = Some(parse_usize(&name, &value(&name)?)?),
            "--max-packets" => {
                parsed.max_packets = Some(parse_u64(&name, &value(&name)?)?);
            }
            "--source-mac" => parsed.source_mac = Some(parse_mac(&value(&name)?)?),
            "--hash-out" => parsed.hash_out = Some(PathBuf::from(value(&name)?)),
            other if other.starts_with('-') => {
                return Err(format!("未知选项 {other}；用 --help 查看用法"));
            }
            _ => {
                if command.is_none() {
                    command = Some(arg);
                } else {
                    positionals.push(arg);
                }
            }
        }
    }
    parsed.command = command.unwrap_or_default();
    if positionals.len() > 1 {
        return Err("只接受一个 pcap 路径".into());
    }
    parsed.path = positionals.pop().map(PathBuf::from);
    Ok(parsed)
}

fn require_no_value(flag: &str, inline: Option<&str>) -> Result<(), String> {
    if inline.is_some() {
        return Err(format!("{flag} 不接受值"));
    }
    Ok(())
}

fn parse_u32(flag: &str, text: &str) -> Result<u32, String> {
    text.parse()
        .map_err(|_| format!("{flag} 不是有效整数：{text}"))
}

fn parse_u64(flag: &str, text: &str) -> Result<u64, String> {
    text.parse()
        .map_err(|_| format!("{flag} 不是有效整数：{text}"))
}

fn parse_usize(flag: &str, text: &str) -> Result<usize, String> {
    text.parse()
        .map_err(|_| format!("{flag} 不是有效整数：{text}"))
}

fn parse_mac(text: &str) -> Result<[u8; 6], String> {
    let hex = |part: &str| u8::from_str_radix(part, 16);
    if text.len() == 12 && !text.contains([':', '-']) {
        let mut mac = [0u8; 6];
        for (i, slot) in mac.iter_mut().enumerate() {
            *slot = hex(&text[i * 2..i * 2 + 2]).map_err(|_| format!("无效 MAC：{text}"))?;
        }
        return Ok(mac);
    }
    let parts: Vec<&str> = text.split([':', '-']).collect();
    if parts.len() != 6 || parts.iter().any(|part| part.is_empty() || part.len() > 2) {
        return Err(format!("无效 MAC：{text}（示例 aa:bb:cc:dd:ee:ff）"));
    }
    let mut mac = [0u8; 6];
    for (slot, part) in mac.iter_mut().zip(parts) {
        *slot = hex(part).map_err(|_| format!("无效 MAC：{text}"))?;
    }
    Ok(mac)
}

fn open_reader(path: &Path) -> Result<PcapReader<BufReader<File>>, String> {
    let file = File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    PcapReader::new(BufReader::new(file)).map_err(|err| format!("{}: {err}", path.display()))
}

fn inspect(path: &Path, args: &Args) -> Result<(), String> {
    let max_packets = args.max_packets.unwrap_or(DEFAULT_MAX_PACKETS);
    let mut reader = open_reader(path)?;
    let header = *reader.header();
    let mut stats = InspectStats::default();
    while stats.packets < max_packets {
        match reader
            .next_packet()
            .map_err(|err| format!("{}: {err}", path.display()))?
        {
            Some(packet) => stats.observe(&packet),
            None => break,
        }
    }
    let limited = if stats.packets >= max_packets {
        reader
            .next_packet()
            .map_err(|err| format!("{}: {err}", path.display()))?
            .is_some()
    } else {
        false
    };
    if args.json {
        println!("{}", inspect_json(path, &header, &stats, limited));
    } else {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        write_inspect_text(&mut out, path, &header, &stats, limited)
            .map_err(|err| format!("写 stdout 失败：{err}"))?;
    }
    Ok(())
}

#[derive(Default)]
struct InspectStats {
    packets: u64,
    captured_bytes: u64,
    original_bytes: u64,
    caplen_min: Option<u32>,
    caplen_max: u32,
    first_ts_ns: Option<u64>,
    last_ts_ns: Option<u64>,
    backwards_ts: u64,
    short_ethernet: u64,
    vc_packets: u64,
    non_vc_ethernet: u64,
    vc_short: u64,
    vc_bad_magic: u64,
    macs: Vec<[u8; 6]>,
    mac_overflow: u64,
    totals: Vec<u16>,
    total_overflow: u64,
}

impl InspectStats {
    fn observe(&mut self, packet: &Packet) {
        self.packets += 1;
        self.captured_bytes += packet.captured_len as u64;
        self.original_bytes += packet.original_len as u64;
        self.caplen_min = Some(
            self.caplen_min
                .map_or(packet.captured_len, |min| min.min(packet.captured_len)),
        );
        self.caplen_max = self.caplen_max.max(packet.captured_len);
        if let Some(last) = self.last_ts_ns
            && packet.ts_ns < last
        {
            self.backwards_ts += 1;
        }
        self.first_ts_ns.get_or_insert(packet.ts_ns);
        self.last_ts_ns = Some(packet.ts_ns);

        if packet.data.len() < ETHERNET_HEADER_LEN {
            self.short_ethernet += 1;
            return;
        }
        let ethertype = u16::from_be_bytes([packet.data[12], packet.data[13]]);
        if ethertype != ETHERTYPE {
            self.non_vc_ethernet += 1;
            return;
        }
        self.vc_packets += 1;
        let source = [
            packet.data[6],
            packet.data[7],
            packet.data[8],
            packet.data[9],
            packet.data[10],
            packet.data[11],
        ];
        if !self.macs.contains(&source) {
            if self.macs.len() < MAX_LISTED_MACS {
                self.macs.push(source);
            } else {
                self.mac_overflow += 1;
            }
        }
        if packet.data.len() < ETHERNET_HEADER_LEN + HEADER_LEN {
            self.vc_short += 1;
            return;
        }
        let header = match Header::parse(&packet.data[ETHERNET_HEADER_LEN..]) {
            Ok(header) => header,
            Err(_) => {
                self.vc_bad_magic += 1;
                return;
            }
        };
        if !self.totals.contains(&header.total) {
            if self.totals.len() < MAX_LISTED_TOTALS {
                self.totals.push(header.total);
            } else {
                self.total_overflow += 1;
            }
        }
    }

    fn avg_caplen(&self) -> f64 {
        if self.packets == 0 {
            0.0
        } else {
            self.captured_bytes as f64 / self.packets as f64
        }
    }
}

fn write_inspect_text(
    out: &mut impl Write,
    path: &Path,
    header: &FileHeader,
    stats: &InspectStats,
    limited: bool,
) -> io::Result<()> {
    writeln!(out, "pcap: {}", path.display())?;
    writeln!(out, "format: {}", format_description(header))?;
    writeln!(
        out,
        "version: {}.{}",
        header.version_major, header.version_minor
    )?;
    writeln!(out, "snaplen: {}", header.snaplen)?;
    writeln!(out, "linktype: {} (Ethernet)", header.linktype)?;
    writeln!(out, "packets: {}", stats.packets)?;
    writeln!(out, "captured_bytes: {}", stats.captured_bytes)?;
    writeln!(out, "original_bytes: {}", stats.original_bytes)?;
    writeln!(
        out,
        "captured_len: min={} max={} avg={:.1}",
        stats.caplen_min.unwrap_or(0),
        stats.caplen_max,
        stats.avg_caplen()
    )?;
    writeln!(
        out,
        "timestamps: first_ns={} last_ns={} backwards={}",
        stats.first_ts_ns.unwrap_or(0),
        stats.last_ts_ns.unwrap_or(0),
        stats.backwards_ts
    )?;
    writeln!(out, "vc_packets: {}", stats.vc_packets)?;
    writeln!(out, "non_vc_ethernet: {}", stats.non_vc_ethernet)?;
    writeln!(out, "short_ethernet: {}", stats.short_ethernet)?;
    writeln!(out, "vc_short: {}", stats.vc_short)?;
    writeln!(out, "vc_bad_magic: {}", stats.vc_bad_magic)?;
    writeln!(
        out,
        "source_macs: {}{}",
        stats
            .macs
            .iter()
            .map(|mac| format_mac(*mac))
            .collect::<Vec<_>>()
            .join(", "),
        if stats.mac_overflow > 0 {
            format!(" (+{} more)", stats.mac_overflow)
        } else {
            String::new()
        }
    )?;
    writeln!(
        out,
        "vc_totals: {}{}",
        stats
            .totals
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        if stats.total_overflow > 0 {
            format!(" (+{} more)", stats.total_overflow)
        } else {
            String::new()
        }
    )?;
    if limited {
        writeln!(out, "note: 达到 --max-packets 上限，未读完全部记录")?;
    }
    Ok(())
}

fn inspect_json(path: &Path, header: &FileHeader, stats: &InspectStats, limited: bool) -> String {
    let macs = stats
        .macs
        .iter()
        .map(|mac| format!("\"{}\"", format_mac(*mac)))
        .collect::<Vec<_>>()
        .join(",");
    let totals = stats
        .totals
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        concat!(
            "{{\"file\":{},\"format\":{{\"kind\":\"classic-pcap\",\"endian\":\"{}\",",
            "\"timestamp\":\"{}\",\"version\":\"{}.{}\",\"snaplen\":{},",
            "\"linktype\":{},\"linktype_name\":\"Ethernet\"}},",
            "\"packets\":{},\"captured_bytes\":{},\"original_bytes\":{},",
            "\"captured_len\":{{\"min\":{},\"max\":{},\"avg\":{:.1}}},",
            "\"timestamps\":{{\"first_ns\":{},\"last_ns\":{},\"backwards\":{}}},",
            "\"vc_packets\":{},\"non_vc_ethernet\":{},\"short_ethernet\":{},",
            "\"vc_short\":{},\"vc_bad_magic\":{},",
            "\"source_macs\":[{}],\"source_mac_overflow\":{},",
            "\"vc_totals\":[{}],\"vc_total_overflow\":{},\"limit_reached\":{}}}"
        ),
        json_string(&path.display().to_string()),
        header.endian.name(),
        if header.nanosecond {
            "nanosecond"
        } else {
            "microsecond"
        },
        header.version_major,
        header.version_minor,
        header.snaplen,
        header.linktype,
        stats.packets,
        stats.captured_bytes,
        stats.original_bytes,
        stats.caplen_min.unwrap_or(0),
        stats.caplen_max,
        stats.avg_caplen(),
        stats.first_ts_ns.unwrap_or(0),
        stats.last_ts_ns.unwrap_or(0),
        stats.backwards_ts,
        stats.vc_packets,
        stats.non_vc_ethernet,
        stats.short_ethernet,
        stats.vc_short,
        stats.vc_bad_magic,
        macs,
        stats.mac_overflow,
        totals,
        stats.total_overflow,
        limited
    )
}

fn replay(path: &Path, args: &Args) -> Result<(), String> {
    let width = args.width.ok_or("replay 需要 --width")?;
    let height = args.height.ok_or("replay 需要 --height")?;
    let mtu = args.mtu.ok_or("replay 需要 --mtu")?;
    let geometry = Geometry::new(width, height).map_err(|err| err.to_string())?;
    let mut config = Config::new(geometry, mtu).map_err(|err| err.to_string())?;
    if let Some(mac) = args.source_mac {
        config = config.with_source_mac(Some(mac));
    }
    let mut reassembler = Reassembler::new(config).map_err(|err| err.to_string())?;
    let mut reader = open_reader(path)?;
    let max_packets = args.max_packets.unwrap_or(DEFAULT_MAX_PACKETS);

    let mut hash_out = match &args.hash_out {
        Some(path) => {
            let file = File::create(path).map_err(|err| format!("{}: {err}", path.display()))?;
            Some(BufWriter::new(file))
        }
        None => None,
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let io_error = |err: io::Error| format!("写输出失败：{err}");

    let mut packets_read = 0u64;
    let mut bytes_read = 0u64;
    let mut ignored: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut base_ts: Option<u64> = None;
    let mut limited = false;
    while packets_read < max_packets {
        let packet = match reader
            .next_packet()
            .map_err(|err| format!("{}: {err}", path.display()))?
        {
            Some(packet) => packet,
            None => break,
        };
        packets_read += 1;
        bytes_read += packet.captured_len as u64;
        let base = *base_ts.get_or_insert(packet.ts_ns);
        // 相对首个包的时间轴；+1µs 保证 0 不被当成“尚未完成过帧”。
        let now_ns = 1_000u64.saturating_add(packet.ts_ns.saturating_sub(base));
        match reassembler.push_ethernet_at(&packet.data, now_ns) {
            PushOutcome::Completed(frame) => {
                if args.framewise {
                    if args.json {
                        writeln!(
                            out,
                            "{{\"frame\":{},\"hash\":\"{}\",\"bytes\":{},\"rseg_ms\":{:.3}}}",
                            frame.frame_id,
                            frame.hash_hex(),
                            frame.data.len(),
                            frame.completed_ns.saturating_sub(frame.first_packet_ns) as f64 / 1e6
                        )
                        .map_err(io_error)?;
                    } else {
                        writeln!(
                            out,
                            "frame {} {} {} {:.3}",
                            frame.frame_id,
                            frame.hash_hex(),
                            frame.data.len(),
                            frame.completed_ns.saturating_sub(frame.first_packet_ns) as f64 / 1e6
                        )
                        .map_err(io_error)?;
                    }
                }
                if let Some(writer) = hash_out.as_mut() {
                    writeln!(writer, "{} {}", frame.frame_id, frame.hash_hex())
                        .map_err(|err| format!("写 {} 失败：{err}", hash_path(args)))?;
                }
            }
            PushOutcome::Accepted => {}
            PushOutcome::Ignored(reason) => {
                *ignored.entry(reason_name(reason)).or_default() += 1;
            }
        }
    }
    if packets_read >= max_packets {
        limited = reader
            .next_packet()
            .map_err(|err| format!("{}: {err}", path.display()))?
            .is_some();
    }
    if let Some(mut writer) = hash_out {
        writer
            .flush()
            .map_err(|err| format!("写 {} 失败：{err}", hash_path(args)))?;
    }

    let inflight_frames = reassembler.inflight_frames();
    let inflight_bytes = reassembler.inflight_bytes();
    let rseg = reassembler.rseg_summary();
    let inter_arrival = reassembler.inter_arrival_summary();
    reassembler.finish();
    let stats = reassembler.stats();

    if args.json {
        let ignored_json = ignored
            .iter()
            .map(|(name, count)| format!("\"{name}\":{count}"))
            .collect::<Vec<_>>()
            .join(",");
        let mac = args
            .source_mac
            .map(|mac| format!("\"{}\"", format_mac(mac)))
            .unwrap_or_else(|| "null".to_string());
        writeln!(
            out,
            concat!(
                "{{\"file\":{},\"packets_read\":{},\"bytes_read\":{},\"limit_reached\":{},",
                "\"config\":{{\"width\":{},\"height\":{},\"mtu\":{},\"payload_max\":{},",
                "\"total\":{},\"source_mac\":{}}},",
                "\"stats\":{},\"ignored\":{{{}}},",
                "\"inflight_at_end\":{},\"inflight_bytes_at_end\":{},",
                "\"rseg\":{},\"inter_arrival\":{}}}"
            ),
            json_string(&path.display().to_string()),
            packets_read,
            bytes_read,
            limited,
            config.geometry.width(),
            config.geometry.height(),
            mtu,
            config.payload_max,
            config.total,
            mac,
            stats_json(&stats),
            ignored_json,
            inflight_frames,
            inflight_bytes,
            timing_json(&rseg),
            timing_json(&inter_arrival)
        )
        .map_err(io_error)?;
    } else {
        writeln!(out, "pcap: {}", path.display()).map_err(io_error)?;
        writeln!(
            out,
            "session: {}x{} NV12, mtu {} (payload {}, {} packets/frame)",
            config.geometry.width(),
            config.geometry.height(),
            mtu,
            config.payload_max,
            config.total
        )
        .map_err(io_error)?;
        if let Some(mac) = args.source_mac {
            writeln!(out, "source_mac_filter: {}", format_mac(mac)).map_err(io_error)?;
        }
        writeln!(out, "packets_read: {packets_read}").map_err(io_error)?;
        writeln!(out, "bytes_read: {bytes_read}").map_err(io_error)?;
        if limited {
            writeln!(out, "note: 达到 --max-packets 上限，未读完全部记录").map_err(io_error)?;
        }
        write_stats_text(&mut out, &stats).map_err(io_error)?;
        writeln!(
            out,
            "inflight_at_end: {inflight_frames} frames / {inflight_bytes} bytes"
        )
        .map_err(io_error)?;
        writeln!(out, "ignored:{}", ignore_summary(&ignored)).map_err(io_error)?;
        writeln!(out, "rseg: {}", timing_text(&rseg)).map_err(io_error)?;
        writeln!(out, "inter_arrival: {}", timing_text(&inter_arrival)).map_err(io_error)?;
    }
    Ok(())
}

fn hash_path(args: &Args) -> String {
    args.hash_out
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<hash-out>".to_string())
}

fn reason_name(reason: IgnoreReason) -> &'static str {
    match reason {
        IgnoreReason::ShortFrame => "ShortFrame",
        IgnoreReason::WrongEtherType => "WrongEtherType",
        IgnoreReason::OtherSource => "OtherSource",
        IgnoreReason::BadMagic => "BadMagic",
        IgnoreReason::BadGeometry => "BadGeometry",
        IgnoreReason::LengthMismatch => "LengthMismatch",
        IgnoreReason::TruncatedPayload => "TruncatedPayload",
        IgnoreReason::BadCrc => "BadCrc",
        IgnoreReason::Duplicate => "Duplicate",
        IgnoreReason::Late => "Late",
        IgnoreReason::SlotConflict => "SlotConflict",
        IgnoreReason::Capacity => "Capacity",
    }
}

fn stats_json(stats: &Stats) -> String {
    format!(
        concat!(
            "{{\"packets_recv\":{},\"bytes_recv\":{},\"packets_lost\":{},",
            "\"dropped\":{},\"complete\":{},\"gaps\":{},\"duplicate\":{},",
            "\"crc_errors\":{},\"ignored_other_source\":{},\"late\":{}}}"
        ),
        stats.packets_recv,
        stats.bytes_recv,
        stats.packets_lost,
        stats.dropped,
        stats.complete,
        stats.gaps,
        stats.duplicate,
        stats.crc_errors,
        stats.ignored_other_source,
        stats.late
    )
}

fn write_stats_text(out: &mut impl Write, stats: &Stats) -> io::Result<()> {
    writeln!(out, "complete: {}", stats.complete)?;
    writeln!(out, "packets_recv: {}", stats.packets_recv)?;
    writeln!(out, "bytes_recv: {}", stats.bytes_recv)?;
    writeln!(out, "packets_lost: {}", stats.packets_lost)?;
    writeln!(out, "dropped_frames: {}", stats.dropped)?;
    writeln!(out, "gaps: {}", stats.gaps)?;
    writeln!(out, "duplicate: {}", stats.duplicate)?;
    writeln!(out, "crc_errors: {}", stats.crc_errors)?;
    writeln!(out, "ignored_other_source: {}", stats.ignored_other_source)?;
    writeln!(out, "late: {}", stats.late)
}

fn timing_json(summary: &TimingSummary) -> String {
    format!(
        "{{\"samples\":{},\"avg_ms\":{:.3},\"p50_ms\":{:.3},\"p99_ms\":{:.3},\"max_ms\":{:.3}}}",
        summary.samples, summary.avg_ms, summary.p50_ms, summary.p99_ms, summary.max_ms
    )
}

fn timing_text(summary: &TimingSummary) -> String {
    format!(
        "samples={} avg={:.3}ms p50={:.3}ms p99={:.3}ms max={:.3}ms",
        summary.samples, summary.avg_ms, summary.p50_ms, summary.p99_ms, summary.max_ms
    )
}

fn ignore_summary(ignored: &BTreeMap<&'static str, u64>) -> String {
    if ignored.is_empty() {
        return " (none)".to_string();
    }
    ignored
        .iter()
        .map(|(name, count)| format!(" {name}={count}"))
        .collect::<Vec<_>>()
        .join("")
}

fn format_description(header: &FileHeader) -> String {
    format!(
        "classic pcap ({}-endian, {} timestamps)",
        header.endian.name(),
        if header.nanosecond {
            "nanosecond"
        } else {
            "microsecond"
        }
    )
}

fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &[&str]) -> Result<Args, String> {
        parse_args(text.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_replay_options_and_inline_values() {
        let parsed = args(&[
            "replay",
            "capture.pcap",
            "--width=640",
            "--height",
            "640",
            "--mtu",
            "9000",
            "--source-mac",
            "02:00:00:00:00:09",
            "--framewise",
            "--json",
            "--max-packets",
            "100",
        ])
        .unwrap();
        assert_eq!(parsed.command, "replay");
        assert_eq!(parsed.path.unwrap().to_string_lossy(), "capture.pcap");
        assert_eq!(parsed.width, Some(640));
        assert_eq!(parsed.height, Some(640));
        assert_eq!(parsed.mtu, Some(9000));
        assert_eq!(parsed.source_mac, Some([2, 0, 0, 0, 0, 9]));
        assert!(parsed.framewise && parsed.json);
        assert_eq!(parsed.max_packets, Some(100));
    }

    #[test]
    fn rejects_bad_usage() {
        assert!(args(&["inspect"]).unwrap().path.is_none());
        assert!(args(&["inspect", "a.pcap", "b.pcap"]).is_err());
        assert!(args(&["inspect", "--json=1", "a.pcap"]).is_err());
        assert!(args(&["replay", "a.pcap", "--width", "x"]).is_err());
        assert!(args(&["replay", "a.pcap", "--source-mac", "zz:00:00:00:00:00"]).is_err());
        assert!(args(&["replay", "a.pcap", "--bogus"]).is_err());
    }

    #[test]
    fn parses_mac_forms() {
        assert_eq!(
            parse_mac("aa:bb:cc:dd:ee:ff").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
        assert_eq!(
            parse_mac("AA-BB-CC-DD-EE-FF").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
        assert_eq!(
            parse_mac("aabbccddeeff").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
        assert!(parse_mac("aa:bb:cc:dd:ee").is_err());
        assert!(parse_mac("aa:bb:cc:dd:ee:ffff").is_err());
    }

    #[test]
    fn json_string_escapes() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }
}
