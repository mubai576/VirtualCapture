use std::ffi::{CStr, c_char, c_int, c_uchar, c_uint, c_void};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::mem::{MaybeUninit, size_of};
use std::path::PathBuf;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use clap::{Args, Parser, Subcommand};
use libloading::os::windows::{
    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LOAD_LIBRARY_SEARCH_SYSTEM32, Library,
};
use serde_json::{Value, json};
use vc_transport::checksum::{fnv1a64, fnv1a64_hex};
use vc_transport::fragment::{FragmentConfig, fragments, write_ethernet};
use vc_transport::{
    Config, Geometry, PushOutcome, Reassembler, TimingSummary, payload_max_for_mtu,
};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use vc_stream::pattern::Pattern;
use vc_stream::shm::Publisher;

const MAX_CAPTURE: usize = 1024 * 1024;
static STOP: AtomicBool = AtomicBool::new(false);
static QPC_FREQ: OnceLock<u64> = OnceLock::new();
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Parser)]
#[command(
    name = "vc-stream",
    version,
    about = "Standalone Rust Raw Npcap data-plane diagnostic"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send deterministic integer-pattern Raw NV12 frames.
    Send(SendArgs),
    /// Receive and reassemble Raw NV12 frames; optional private SHM v3 publication.
    Receive(ReceiveArgs),
}

#[derive(Args)]
struct CommonArgs {
    #[arg(long)]
    adapter: String,
    #[arg(long, default_value_t = 640)]
    width: u32,
    #[arg(long, default_value_t = 640)]
    height: u32,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    #[arg(long, default_value_t = 9000)]
    mtu: usize,
    #[arg(long, default_value = "s-rust-stream")]
    session: String,
    #[arg(long, default_value = "0x88B5", value_parser = parse_ethertype)]
    ethertype: u16,
}

#[derive(Args)]
struct SendArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, default_value_t = 120)]
    frames: u64,
    #[arg(long, default_value_t = 1)]
    batch: usize,
    #[arg(long, default_value_t = 0)]
    pacing_us: u64,
    #[arg(long, default_value = "motion", value_parser = parse_pattern)]
    pattern: Pattern,
    #[arg(long, default_value = "02:00:00:00:00:01", value_parser = parse_mac)]
    source_mac: [u8; 6],
    #[arg(long, default_value = "ff:ff:ff:ff:ff:ff", value_parser = parse_mac)]
    dst_mac: [u8; 6],
    #[arg(long)]
    stats: Option<PathBuf>,
    #[arg(long)]
    hashlog: Option<PathBuf>,
}

#[derive(Args)]
struct ReceiveArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, value_parser = parse_mac)]
    source_mac: Option<[u8; 6]>,
    #[arg(long, default_value_t = 120)]
    expect_frames: u64,
    #[arg(long, default_value_t = 30)]
    timeout_s: u64,
    #[arg(long)]
    ready: Option<PathBuf>,
    #[arg(long)]
    stats: Option<PathBuf>,
    #[arg(long)]
    hashlog: Option<PathBuf>,
    /// Override the legacy DShow mapping name; diagnostics should use a unique Local\\ name.
    #[arg(long)]
    shm: Option<String>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcapIf {
    next: *mut PcapIf,
    name: *mut c_char,
    description: *mut c_char,
    _addresses: *mut c_void,
    _flags: c_uint,
}

#[repr(C)]
struct PcapHeader {
    ts_sec: u32,
    ts_usec: u32,
    caplen: u32,
    len: u32,
}

#[repr(C)]
struct PcapSendQueue {
    maxlen: c_uint,
    len: c_uint,
    buffer: *mut c_char,
}

type FindAllDevs = unsafe extern "C" fn(*mut *mut PcapIf, *mut c_char) -> c_int;
type FreeAllDevs = unsafe extern "C" fn(*mut PcapIf);
type OpenLive =
    unsafe extern "C" fn(*const c_char, c_int, c_int, c_int, *mut c_char) -> *mut c_void;
type SendPacket = unsafe extern "C" fn(*mut c_void, *const c_uchar, c_int) -> c_int;
type NextEx = unsafe extern "C" fn(*mut c_void, *mut *mut PcapHeader, *mut *const c_uchar) -> c_int;
type SetBuff = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type SetMinToCopy = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type DataLink = unsafe extern "C" fn(*mut c_void) -> c_int;
type SendQueueAlloc = unsafe extern "C" fn(c_uint) -> *mut PcapSendQueue;
type SendQueueDestroy = unsafe extern "C" fn(*mut PcapSendQueue);
type SendQueueQueue =
    unsafe extern "C" fn(*mut PcapSendQueue, *const PcapHeader, *const c_uchar) -> c_int;
type SendQueueTransmit = unsafe extern "C" fn(*mut c_void, *mut PcapSendQueue, c_int) -> c_uint;
type GetErr = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type LibVersion = unsafe extern "C" fn() -> *const c_char;
type Close = unsafe extern "C" fn(*mut c_void);

struct Npcap {
    _library: Library,
    find_all_devs: FindAllDevs,
    free_all_devs: FreeAllDevs,
    open_live: OpenLive,
    send_packet: SendPacket,
    next_ex: NextEx,
    set_buff: SetBuff,
    set_min_to_copy: SetMinToCopy,
    datalink: DataLink,
    queue_alloc: SendQueueAlloc,
    queue_destroy: SendQueueDestroy,
    queue_packet: SendQueueQueue,
    queue_transmit: SendQueueTransmit,
    get_err: GetErr,
    lib_version: LibVersion,
    close: Close,
}

impl Npcap {
    fn load() -> anyhow::Result<Self> {
        let library = load_npcap()?;
        unsafe {
            macro_rules! symbol {
                ($name:literal, $kind:ty) => {{
                    *library
                        .get::<$kind>(concat!($name, "\0").as_bytes())
                        .with_context(|| format!("Npcap lacks {} export", $name))?
                }};
            }
            Ok(Self {
                find_all_devs: symbol!("pcap_findalldevs", FindAllDevs),
                free_all_devs: symbol!("pcap_freealldevs", FreeAllDevs),
                open_live: symbol!("pcap_open_live", OpenLive),
                send_packet: symbol!("pcap_sendpacket", SendPacket),
                next_ex: symbol!("pcap_next_ex", NextEx),
                set_buff: symbol!("pcap_setbuff", SetBuff),
                set_min_to_copy: symbol!("pcap_setmintocopy", SetMinToCopy),
                datalink: symbol!("pcap_datalink", DataLink),
                queue_alloc: symbol!("pcap_sendqueue_alloc", SendQueueAlloc),
                queue_destroy: symbol!("pcap_sendqueue_destroy", SendQueueDestroy),
                queue_packet: symbol!("pcap_sendqueue_queue", SendQueueQueue),
                queue_transmit: symbol!("pcap_sendqueue_transmit", SendQueueTransmit),
                get_err: symbol!("pcap_geterr", GetErr),
                lib_version: symbol!("pcap_lib_version", LibVersion),
                close: symbol!("pcap_close", Close),
                _library: library,
            })
        }
    }

    fn version(&self) -> String {
        let ptr = unsafe { (self.lib_version)() };
        if ptr.is_null() {
            return "unknown".into();
        }
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }

    fn adapters(&self) -> anyhow::Result<Vec<(String, String)>> {
        let mut list = ptr::null_mut();
        let mut err = [0i8; 256];
        ensure!(
            unsafe { (self.find_all_devs)(&mut list, err.as_mut_ptr()) } == 0,
            "pcap_findalldevs: {}",
            c_array(&err)
        );
        let mut adapters = Vec::new();
        let mut node = list;
        while !node.is_null() {
            let item = unsafe { &*node };
            if !item.name.is_null() {
                adapters.push((
                    unsafe { CStr::from_ptr(item.name) }
                        .to_string_lossy()
                        .into_owned(),
                    if item.description.is_null() {
                        String::new()
                    } else {
                        unsafe { CStr::from_ptr(item.description) }
                            .to_string_lossy()
                            .into_owned()
                    },
                ));
            }
            node = item.next;
        }
        if !list.is_null() {
            unsafe { (self.free_all_devs)(list) };
        }
        Ok(adapters)
    }

    fn select_adapter(&self, requested: &str) -> anyhow::Result<String> {
        let entries = self.adapters()?;
        if requested == r"\Device\NPF_Loopback" {
            ensure!(
                entries.iter().any(|entry| entry.0 == requested),
                "requested Npcap Loopback adapter is not installed"
            );
            return Ok(requested.to_owned());
        }
        let needle = requested.to_lowercase();
        let matches: Vec<_> = entries
            .into_iter()
            .filter(|(name, description)| {
                format!("{name} {description}")
                    .to_lowercase()
                    .contains(&needle)
            })
            .collect();
        match matches.as_slice() {
            [(name, _)] => Ok(name.clone()),
            [] => bail!("adapter selector '{requested}' matched no Npcap device"),
            _ => bail!(
                "adapter selector '{requested}' is ambiguous; use a complete \\Device\\NPF_ name"
            ),
        }
    }

    fn open(
        &self,
        adapter: &str,
        snaplen: c_int,
        timeout_ms: c_int,
    ) -> anyhow::Result<*mut c_void> {
        let c_adapter = cstring(adapter, "adapter")?;
        let mut err = [0i8; 256];
        let handle = unsafe {
            (self.open_live)(c_adapter.as_ptr(), snaplen, 1, timeout_ms, err.as_mut_ptr())
        };
        ensure!(
            !handle.is_null(),
            "pcap_open_live({adapter}): {}",
            c_array(&err)
        );
        Ok(handle)
    }

    fn handle_error(&self, handle: *mut c_void, operation: &str) -> anyhow::Error {
        let msg = unsafe { (self.get_err)(handle) };
        let detail = if msg.is_null() {
            "unknown Npcap error".to_owned()
        } else {
            unsafe { CStr::from_ptr(msg) }
                .to_string_lossy()
                .into_owned()
        };
        anyhow::anyhow!("{operation}: {detail}")
    }
}

struct Capture(*mut c_void, Close);
impl Drop for Capture {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (self.1)(self.0) };
            self.0 = ptr::null_mut();
        }
    }
}

struct SendQueue(*mut PcapSendQueue, SendQueueDestroy);
impl Drop for SendQueue {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (self.1)(self.0) };
            self.0 = ptr::null_mut();
        }
    }
}

pub(super) fn run() -> anyhow::Result<()> {
    STOP.store(false, Ordering::Relaxed);
    install_ctrl_handler()?;
    let frequency = query_frequency()?;
    let _ = QPC_FREQ.set(frequency);
    let cli = Cli::parse();
    match cli.command {
        Command::Send(args) => send(args),
        Command::Receive(args) => receive(args),
    }
}

fn send(args: SendArgs) -> anyhow::Result<()> {
    validate_common(&args.common)?;
    ensure!(args.frames > 0, "--frames must be positive");
    ensure!(
        (1..=10000).contains(&args.batch),
        "--batch must be in 1..=10000"
    );
    let pattern_id = args.pattern.id();
    let geometry = geometry(&args.common)?;
    let frame_bytes = geometry.frame_bytes();
    let payload_max = payload_max_for_mtu(args.common.mtu)?;
    let total = vc_transport::packet_count(frame_bytes, payload_max)? as usize;
    let api = Npcap::load()?;
    let adapter = api.select_adapter(&args.common.adapter)?;
    let handle = api.open(&adapter, 65536, 1000)?;
    let _capture = Capture(handle, api.close);
    println!(
        "[vc-stream] role=send adapter={adapter} session={} mode=pattern pattern={} geometry={}x{}@{} mtu={} payload={} packets_per_frame={} batch={} npcap={}",
        args.common.session,
        args.pattern,
        args.common.width,
        args.common.height,
        args.common.fps,
        args.common.mtu,
        payload_max,
        total,
        args.batch,
        api.version()
    );
    let mut hashlog = create_writer(args.hashlog.as_ref())?;
    let mut frame = vec![0u8; frame_bytes];
    let mut packet = vec![0u8; 14 + 28 + payload_max];
    let mut queue = None;
    let mut queue_count = 0usize;
    if args.batch > 1 {
        let capacity = args
            .batch
            .min(total)
            .saturating_mul(14 + 28 + payload_max + size_of::<PcapHeader>())
            .max(2 * 1024 * 1024)
            .min(u32::MAX as usize);
        let q = unsafe { (api.queue_alloc)(capacity as u32) };
        ensure!(!q.is_null(), "pcap_sendqueue_alloc({capacity}) failed");
        queue = Some(SendQueue(q, api.queue_destroy));
    }
    let cpu_start = process_cpu_ns()?;
    let started = Instant::now();
    let mut pacer = FramePacer::new(args.common.fps, qpc_ns());
    let mut sent_packets = 0u64;
    let mut sent_bytes = 0u64;
    let mut first_qpc = 0u64;
    let mut last_qpc = 0u64;
    let mut total_frame_elapsed = 0u64;

    for frame_id in 0..args.frames {
        sleep_until(pacer.next_ns);
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        let frame_start_ns = qpc_ns();
        pacer.advance(frame_start_ns);
        args.pattern.fill(geometry, frame_id as u32, &mut frame);
        let send_ns = qpc_ns();
        let generation_ns = send_ns.saturating_sub(frame_start_ns);
        let config = FragmentConfig {
            payload_max,
            qpc_ns: frame_start_ns,
            pattern: pattern_id,
        };
        for fragment in fragments(&frame, frame_id as u32, config)? {
            let len = write_ethernet(args.dst_mac, args.source_mac, &fragment, &mut packet)
                .map_err(|needed| anyhow::anyhow!("packet buffer requires {needed} bytes"))?;
            if let Some(send_queue) = queue.as_ref() {
                let header = PcapHeader {
                    ts_sec: 0,
                    ts_usec: 0,
                    caplen: len as u32,
                    len: len as u32,
                };
                let rc = unsafe { (api.queue_packet)(send_queue.0, &header, packet.as_ptr()) };
                ensure!(
                    rc == 0,
                    "pcap_sendqueue_queue failed before frame {frame_id} packet {sent_packets}"
                );
                queue_count += 1;
                if queue_count >= args.batch {
                    transmit_queue(&api, handle, send_queue.0, queue_count)?;
                    queue_count = 0;
                }
            } else {
                let rc = unsafe { (api.send_packet)(handle, packet.as_ptr(), len as c_int) };
                if rc != 0 {
                    bail!(
                        "pcap_sendpacket failed: {}",
                        api.handle_error(handle, "send packet")
                    );
                }
            }
            sent_packets += 1;
            sent_bytes += len as u64;
            if args.pacing_us > 0 {
                let delay_ns = args.pacing_us.saturating_mul(1000);
                sleep_until(qpc_ns().saturating_add(delay_ns));
            }
        }
        if let (Some(send_queue), n) = (queue.as_ref(), queue_count)
            && n > 0
        {
            transmit_queue(&api, handle, send_queue.0, n)?;
            queue_count = 0;
        }
        let end_ns = qpc_ns();
        if frame_id == 0 {
            first_qpc = send_ns;
        }
        last_qpc = end_ns;
        total_frame_elapsed =
            total_frame_elapsed.saturating_add(generation_ns + end_ns.saturating_sub(send_ns));
        let hash = vc_transport::checksum::fnv1a64_hex(vc_transport::checksum::fnv1a64(&frame));
        if let Some(writer) = hashlog.as_mut() {
            writeln!(
                writer,
                "{{\"frame_id\":{},\"hash\":\"{}\",\"qpc_ns\":{},\"send_qpc_ns\":{}}}",
                frame_id, hash, frame_start_ns, send_ns
            )?;
        }
    }
    if let Some(writer) = hashlog.as_mut() {
        writer.flush()?;
    }
    let elapsed = started.elapsed().as_secs_f64().max(0.000_001);
    let sent_frames = sent_packets / total as u64;
    let cpu_pct =
        (process_cpu_ns()?.saturating_sub(cpu_start) as f64 / 1_000_000_000.0) / elapsed * 100.0;
    let stats = json!({
        "session_id": args.common.session,
        "adapter": adapter,
        "running": false,
        "frames": sent_frames,
        "sent_frames": sent_frames,
        "sent_packets": sent_packets,
        "send_errors": 0,
        "bytes": sent_bytes,
        "pps": sent_packets as f64 / elapsed,
        "bps": sent_bytes as f64 * 8.0 / elapsed,
        "fps_actual": sent_frames as f64 / elapsed,
        "fps_target": args.common.fps,
        "first_send_qpc_ns": first_qpc,
        "last_send_qpc_ns": last_qpc,
        "generation_send_avg_ms": total_frame_elapsed as f64 / sent_frames.max(1) as f64 / 1_000_000.0,
        "cpu_pct": cpu_pct,
        "uptime_s": elapsed,
        "pattern": args.pattern.name(),
        "width": args.common.width,
        "height": args.common.height,
        "frame_bytes": frame_bytes,
        "mtu": args.common.mtu,
        "ethertype": format!("0x{:04x}", args.common.ethertype),
        "batch": args.batch,
        "pacing_us": args.pacing_us,
        "transform": "pattern",
        "codec": "raw",
        "source_skipped": 0,
        "gpu_ms": 0.0,
    });
    write_stats(args.stats.as_ref(), &stats)?;
    println!("{}", stats);
    ensure!(
        sent_frames == args.frames || STOP.load(Ordering::Relaxed),
        "sender stopped after {sent_frames}/{} complete frames",
        args.frames
    );
    Ok(())
}

fn transmit_queue(
    api: &Npcap,
    handle: *mut c_void,
    queue: *mut PcapSendQueue,
    packets: usize,
) -> anyhow::Result<()> {
    let expected = unsafe { (*queue).len };
    let actual = unsafe { (api.queue_transmit)(handle, queue, 0) };
    ensure!(
        actual == expected,
        "pcap_sendqueue_transmit short send: {actual}/{expected} bytes for {packets} packets; no batch fallback"
    );
    unsafe {
        (*queue).len = 0;
    }
    Ok(())
}

fn receive(args: ReceiveArgs) -> anyhow::Result<()> {
    validate_common(&args.common)?;
    ensure!(args.timeout_s > 0, "--timeout-s must be positive");
    let geometry = geometry(&args.common)?;
    let payload_max = payload_max_for_mtu(args.common.mtu)?;
    let config = Config::new(geometry, args.common.mtu)?.with_source_mac(args.source_mac);
    let mut reassembler = Reassembler::new(config)?;
    let api = Npcap::load()?;
    let adapter = api.select_adapter(&args.common.adapter)?;
    let handle = api.open(&adapter, (MAX_CAPTURE + 14) as c_int, 1)?;
    let _capture = Capture(handle, api.close);
    unsafe {
        (api.set_buff)(handle, 128 * 1024 * 1024);
    }
    unsafe {
        (api.set_min_to_copy)(handle, 0);
    }
    let linktype = unsafe { (api.datalink)(handle) };
    let mut publisher = match args.shm.as_deref() {
        Some(name) => Some(
            Publisher::new(name, geometry, args.common.fps)
                .with_context(|| format!("create requested SHM v3 mapping {name}"))?,
        ),
        None => None,
    };
    let mut hashlog = create_writer(args.hashlog.as_ref())?;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(args.timeout_s);
    let mut hashes = 0u64;
    let mut shm_published = 0u64;
    let mut hash_mismatch = 0u64;
    let mut last_frame_id = None;
    let mut first_completed_ns = 0u64;
    let mut final_completed_ns = 0u64;
    let mut bytes = 0u64;
    let expected_frame_bytes = geometry.frame_bytes();

    let readiness = json!({ "session_id": args.common.session, "adapter": adapter, "running": true, "geometry": {"width":args.common.width,"height":args.common.height,"fps":args.common.fps}, "mtu":args.common.mtu, "ethertype":format!("0x{:04x}",args.common.ethertype), "datalink":linktype, "shm_enabled":publisher.is_some(), "shm_version":if publisher.is_some(){3}else{0}, "capture_buffer_bytes":128*1024*1024, "mintocopy":0, "npcap":api.version() });
    write_ready(args.ready.as_ref(), &readiness)?;
    println!(
        "[vc-stream] role=receive adapter={adapter} session={} datalink={} width={} height={} fps={} mtu={} payload={} expect_frames={} shm={}",
        args.common.session,
        linktype,
        args.common.width,
        args.common.height,
        args.common.fps,
        args.common.mtu,
        payload_max,
        args.expect_frames,
        publisher.is_some()
    );
    let cpu_start = process_cpu_ns()?;
    let mut expected_frame = vec![0u8; expected_frame_bytes];

    while !STOP.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(args.timeout_s) {
        if args.expect_frames > 0 && hashes >= args.expect_frames {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        let mut header = ptr::null_mut();
        let mut data = ptr::null();
        let rc = unsafe { (api.next_ex)(handle, &mut header, &mut data) };
        if rc == 0 {
            continue;
        }
        if rc < 0 {
            bail!(
                "pcap_next_ex returned {rc}: {}",
                api.handle_error(handle, "capture")
            );
        }
        if header.is_null() || data.is_null() {
            continue;
        }
        let caplen = unsafe { (*header).caplen as usize };
        if !(14 + 28..=MAX_CAPTURE).contains(&caplen) {
            continue;
        }
        let packet = unsafe { std::slice::from_raw_parts(data, caplen) };
        let arrived_ns = qpc_ns();
        bytes = bytes.saturating_add(caplen as u64);
        if let PushOutcome::Completed(frame) =
            reassembler.push_ethernet_timed(packet, arrived_ns, qpc_ns)
        {
            ensure!(
                frame.data.len() == expected_frame_bytes,
                "completed frame {} has {} bytes, expected {expected_frame_bytes}",
                frame.frame_id,
                frame.data.len()
            );
            let hash = fnv1a64(&frame.data);
            let hash_text = fnv1a64_hex(hash);
            let pattern = Pattern::from_id(frame.pattern);
            if let Some(pattern) = pattern {
                pattern.fill(geometry, frame.frame_id, &mut expected_frame);
                if expected_frame != frame.data {
                    hash_mismatch += 1;
                }
            }
            let token = if let Some(shm) = publisher.as_mut() {
                Some(shm.publish(frame.frame_id, frame.first_packet_ns, &frame.data, qpc_ns)?)
            } else {
                None
            };
            if let (Some(shm), Some(token)) = (publisher.as_mut(), token) {
                shm.mark_hash(token, hash);
                shm_published += 1;
            }
            if let Some(writer) = hashlog.as_mut() {
                writeln!(
                    writer,
                    "{{\"frame_id\":{},\"hash\":\"{}\",\"qpc_ns\":{},\"first_packet_qpc_ns\":{}}}",
                    frame.frame_id, hash_text, frame.completed_ns, frame.first_packet_ns
                )?;
                writer.flush()?;
            }
            hashes += 1;
            if hashes == 1 {
                first_completed_ns = frame.completed_ns;
            }
            final_completed_ns = frame.completed_ns;
            last_frame_id = Some(frame.frame_id);
        }
    }
    reassembler.finish();
    let cpu_pct = (process_cpu_ns()?.saturating_sub(cpu_start) as f64 / 1_000_000_000.0)
        / start.elapsed().as_secs_f64().max(0.000_001)
        * 100.0;
    if let Some(writer) = hashlog.as_mut() {
        writer.flush()?;
    }
    if let Some(stats_path) = args.stats.as_ref() {
        write_stats(
            Some(stats_path),
            &json!({"session_id":args.common.session,"adapter":adapter,"running":false,"complete":reassembler.stats().complete,"hash_rows":hashes,"dropped":reassembler.stats().dropped,"packets_recv":reassembler.stats().packets_recv,"packets_lost":reassembler.stats().packets_lost,"crc_errors":reassembler.stats().crc_errors,"duplicate":reassembler.stats().duplicate,"gaps":reassembler.stats().gaps,"hash_mismatch":hash_mismatch,"shm_published":shm_published,"bytes_captured":bytes,"pps":reassembler.stats().packets_recv as f64/start.elapsed().as_secs_f64().max(0.000001),"bps":bytes as f64*8.0/start.elapsed().as_secs_f64().max(0.000001),"fps_actual":hashes as f64/start.elapsed().as_secs_f64().max(0.000001),"cpu_pct":cpu_pct,"inter_arrival":timing_value(reassembler.inter_arrival_summary()),"rseg":timing_value(reassembler.rseg_summary()),"uptime_s":start.elapsed().as_secs_f64(),"width":args.common.width,"height":args.common.height,"frame_bytes":geometry.frame_bytes(),"fps_limit":args.common.fps,"mtu":args.common.mtu,"ethertype":format!("0x{:04x}",args.common.ethertype),"codec":"raw","last_frame_id":last_frame_id,"first_completed_qpc_ns":first_completed_ns,"last_completed_qpc_ns":final_completed_ns,"capture_datalink":linktype,"inflight_frames":reassembler.inflight_frames(),"inflight_bytes":reassembler.inflight_bytes()}),
        )?;
    }
    let stats = reassembler.stats();
    println!(
        "[vc-stream] receive done adapter={adapter} session={} complete={} drops={} gaps={} hash_mismatch={}",
        args.common.session, stats.complete, stats.dropped, stats.gaps, hash_mismatch
    );
    if args.expect_frames > 0 {
        ensure!(
            hashes == args.expect_frames,
            "receiver got {hashes}/{} frames before timeout",
            args.expect_frames
        );
    }
    ensure!(
        stats.dropped == 0 && stats.packets_lost == 0 && hash_mismatch == 0,
        "receiver finished with drops={}, packets_lost={}, hash_mismatch={hash_mismatch}",
        stats.dropped,
        stats.packets_lost
    );
    Ok(())
}

fn timing_value(timing: TimingSummary) -> Value {
    json!({"samples":timing.samples,"avg_ms":timing.avg_ms,"p50_ms":timing.p50_ms,"p99_ms":timing.p99_ms,"max_ms":timing.max_ms})
}

fn write_ready(path: Option<&PathBuf>, value: &Value) -> anyhow::Result<()> {
    if let Some(path) = path {
        atomic_write_json(path, value)?;
    }
    Ok(())
}

fn write_stats(path: Option<&PathBuf>, value: &Value) -> anyhow::Result<()> {
    if let Some(path) = path {
        atomic_write_json(path, value)?;
    }
    Ok(())
}

fn atomic_write_json(path: &PathBuf, value: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create stats directory {}", parent.display()))?;
    }
    let serial = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!(
        "{}.{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("json"),
        serial
    ));
    {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .with_context(|| format!("create temporary JSON {}", temp.display()))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error).with_context(|| format!("replace JSON {}", path.display()));
    }
    Ok(())
}

fn create_writer(path: Option<&PathBuf>) -> anyhow::Result<Option<BufWriter<File>>> {
    path.map(|path| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open hashlog {}", path.display()))?;
        Ok(BufWriter::new(file))
    })
    .transpose()
}

fn parse_mac(value: &str) -> Result<[u8; 6], String> {
    let mut output = [0u8; 6];
    let parts: Vec<_> = value.split(':').collect();
    if parts.len() != 6 {
        return Err("MAC must contain six colon-separated octets".into());
    }
    for (index, part) in parts.iter().enumerate() {
        if part.len() != 2 {
            return Err("MAC octets must be two hex digits".into());
        }
        output[index] = u8::from_str_radix(part, 16).map_err(|_| "MAC contains a non-hex octet")?;
    }
    Ok(output)
}

fn parse_ethertype(value: &str) -> Result<u16, String> {
    let parsed = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map(|value| u16::from_str_radix(value, 16))
        .unwrap_or_else(|| value.parse::<u16>());
    parsed.map_err(|_| format!("invalid EtherType '{value}'"))
}

fn parse_pattern(value: &str) -> Result<Pattern, String> {
    value.parse()
}

fn validate_common(args: &CommonArgs) -> anyhow::Result<()> {
    ensure!(
        !args.adapter.trim().is_empty(),
        "--adapter must not be empty"
    );
    ensure!(
        !args.session.trim().is_empty() && !args.session.contains('\0'),
        "--session must not be empty or contain NUL"
    );
    ensure!(
        args.ethertype == vc_transport::wire::ETHERTYPE,
        "--ethertype is frozen at 0x88B5"
    );
    ensure!((1..=1000).contains(&args.fps), "--fps must be in 1..=1000");
    geometry(args)?;
    payload_max_for_mtu(args.mtu)?;
    Ok(())
}

fn geometry(args: &CommonArgs) -> anyhow::Result<Geometry> {
    Ok(Geometry::new(args.width, args.height)?)
}

fn qpc_ns() -> u64 {
    let mut counter = MaybeUninit::<i64>::uninit();
    unsafe {
        QueryPerformanceCounter(counter.as_mut_ptr());
    }
    let ticks = unsafe { counter.assume_init() } as u64;
    let freq = *QPC_FREQ.get_or_init(|| query_frequency().unwrap_or(1));
    let frequency = freq.max(1);
    ticks / frequency * 1_000_000_000 + ticks % frequency * 1_000_000_000 / frequency
}

fn query_frequency() -> anyhow::Result<u64> {
    let mut frequency = MaybeUninit::<i64>::uninit();
    ensure!(
        unsafe { QueryPerformanceFrequency(frequency.as_mut_ptr()) } != 0,
        "QueryPerformanceFrequency failed"
    );
    let frequency = unsafe { frequency.assume_init() };
    ensure!(frequency > 0, "invalid QPC frequency {frequency}");
    Ok(frequency as u64)
}

struct FramePacer {
    next_ns: u64,
    base_ns: u64,
    frame_index: u64,
    fps: u32,
}

impl FramePacer {
    fn new(fps: u32, base_ns: u64) -> Self {
        Self {
            next_ns: base_ns,
            base_ns,
            frame_index: 0,
            fps: fps.max(1),
        }
    }

    fn advance(&mut self, frame_start_ns: u64) {
        // Absorb minor wake-up jitter without drifting. Rebase after a larger
        // stall instead of trying to catch up by sending back-to-back frames.
        let phase_tolerance_ns = self.offset_ns(1) / 8;
        if frame_start_ns.saturating_sub(self.next_ns) > phase_tolerance_ns {
            self.base_ns = frame_start_ns;
            self.frame_index = 0;
        }
        self.frame_index = self.frame_index.saturating_add(1);
        self.next_ns = self
            .base_ns
            .saturating_add(self.offset_ns(self.frame_index));
    }

    fn offset_ns(&self, frame_index: u64) -> u64 {
        ((u128::from(frame_index) * 1_000_000_000) / u128::from(self.fps)).min(u128::from(u64::MAX))
            as u64
    }
}

fn process_cpu_ns() -> anyhow::Result<u64> {
    fn filetime_value(value: FILETIME) -> u64 {
        (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
    }
    let mut creation = MaybeUninit::<FILETIME>::uninit();
    let mut exit = MaybeUninit::<FILETIME>::uninit();
    let mut kernel = MaybeUninit::<FILETIME>::uninit();
    let mut user = MaybeUninit::<FILETIME>::uninit();
    ensure!(
        unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                creation.as_mut_ptr(),
                exit.as_mut_ptr(),
                kernel.as_mut_ptr(),
                user.as_mut_ptr(),
            )
        } != 0,
        "GetProcessTimes failed"
    );
    Ok(filetime_value(unsafe { kernel.assume_init() })
        .saturating_add(filetime_value(unsafe { user.assume_init() }))
        * 100)
}

fn sleep_until(target_ns: u64) {
    loop {
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        let remaining = target_ns.saturating_sub(qpc_ns());
        if remaining == 0 {
            break;
        }
        if remaining > 2_000_000 {
            thread::sleep(Duration::from_nanos(remaining - 1_000_000));
        } else {
            std::hint::spin_loop();
        }
    }
}

fn install_ctrl_handler() -> anyhow::Result<()> {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, PHANDLER_ROUTINE, SetConsoleCtrlHandler,
    };
    unsafe extern "system" fn handle(event: u32) -> i32 {
        if event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT {
            STOP.store(true, Ordering::Relaxed);
            1
        } else {
            0
        }
    }
    let handler: PHANDLER_ROUTINE = Some(handle);
    ensure!(
        unsafe { SetConsoleCtrlHandler(handler, 1) } != 0,
        "SetConsoleCtrlHandler failed"
    );
    Ok(())
}

fn load_npcap() -> anyhow::Result<Library> {
    let mut system = vec![0u16; 32_768];
    let length = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) };
    ensure!(
        length > 0 && (length as usize) < system.len(),
        "GetSystemDirectoryW failed"
    );
    let system_dir = String::from_utf16(&system[..length as usize])?;
    let candidates = [
        PathBuf::from(&system_dir).join("Npcap\\wpcap.dll"),
        PathBuf::from(&system_dir).join("wpcap.dll"),
    ];
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        let library = unsafe {
            Library::load_with_flags(
                path,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
                    | LOAD_LIBRARY_SEARCH_SYSTEM32
                    | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if let Ok(library) = library {
            return Ok(library);
        }
    }
    bail!(
        "wpcap.dll was not found under the Windows System32 Npcap or System32 directory; install Npcap separately"
    )
}

fn cstring(value: &str, label: &str) -> anyhow::Result<std::ffi::CString> {
    std::ffi::CString::new(value).with_context(|| format!("{label} contains NUL"))
}

fn c_array(value: &[i8]) -> String {
    let bytes: Vec<u8> = value
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_pacer_distributes_fractional_nanoseconds_without_drift() {
        let mut pacer = FramePacer::new(240, 17);
        let mut frame_start = 17;
        for _ in 0..240 {
            pacer.advance(frame_start);
            frame_start = pacer.next_ns;
        }
        assert_eq!(frame_start, 1_000_000_017);
        pacer.advance(frame_start);
        assert_eq!(pacer.next_ns - frame_start, 4_166_666);
    }

    #[test]
    fn frame_pacer_absorbs_small_lateness_without_drift() {
        let base = 100;
        let mut pacer = FramePacer::new(240, base);
        for _ in 0..240 {
            pacer.advance(pacer.next_ns + 5_000);
        }
        assert_eq!(pacer.next_ns, base + 1_000_000_000);
    }

    #[test]
    fn frame_pacer_rebases_after_stall_without_catching_up_in_a_burst() {
        let mut pacer = FramePacer::new(240, 100);
        pacer.advance(100);
        pacer.advance(9_000_000);
        assert_eq!(pacer.next_ns, 13_166_666);
    }

    #[test]
    fn mac_parser_accepts_unicast_and_rejects_bad_values() {
        assert_eq!(
            parse_mac("02:11:22:33:44:55").unwrap(),
            [2, 17, 34, 51, 68, 85]
        );
        assert!(parse_mac("ff:ff").is_err());
        assert!(parse_mac("01:11:22:33:44:55").is_ok());
    }

    #[test]
    fn ethertype_parser_preserves_the_frozen_value() {
        assert_eq!(parse_ethertype("0x88B5").unwrap(), 0x88b5);
        assert_eq!(parse_ethertype("34997").unwrap(), 0x88b5);
        assert!(parse_ethertype("65536").is_err());
    }

    #[test]
    fn common_args_reject_non_frozen_ethertype() {
        let mut args = CommonArgs {
            adapter: r"\\Device\\NPF_Loopback".into(),
            width: 640,
            height: 640,
            fps: 60,
            mtu: 9000,
            session: "test".into(),
            ethertype: 0x0800,
        };
        assert!(validate_common(&args).is_err());
        args.ethertype = vc_transport::wire::ETHERTYPE;
        assert!(validate_common(&args).is_ok());
    }
}
