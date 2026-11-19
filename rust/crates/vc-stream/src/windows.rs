use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int, c_uchar, c_uint, c_void};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::mem::{MaybeUninit, size_of};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use clap::{Args, Parser, Subcommand};
use libloading::os::windows::{
    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LOAD_LIBRARY_SEARCH_SYSTEM32, Library,
};
use serde_json::{Value, json};
use vc_media::{AuSession, CodecConfig, Decoder, Encoder, OperationState, parse_au};
use vc_source::{DesktopSource, SourceConfig};
use vc_transport::checksum::{fnv1a64, fnv1a64_hex};
use vc_transport::encoded::EncodedReassembler;
use vc_transport::fragment::{FragmentConfig, fragments, write_ethernet};
use vc_transport::{
    Config, Geometry, PushOutcome, Reassembler, TimingSummary, payload_max_for_mtu,
};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use crate::pattern::Pattern;
use crate::shm::Publisher;

const MAX_CAPTURE: usize = 1024 * 1024;
static CLI_STOP: AtomicBool = AtomicBool::new(false);
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
pub(crate) struct CommonArgs {
    #[arg(long)]
    pub(crate) adapter: String,
    #[arg(long, default_value_t = 640)]
    pub(crate) width: u32,
    #[arg(long, default_value_t = 640)]
    pub(crate) height: u32,
    #[arg(long, default_value_t = 60)]
    pub(crate) fps: u32,
    #[arg(long, default_value_t = 9000)]
    pub(crate) mtu: usize,
    #[arg(long, default_value = "s-rust-stream")]
    pub(crate) session: String,
    #[arg(long, default_value = "0x88B5", value_parser = parse_ethertype)]
    pub(crate) ethertype: u16,
}

#[derive(Args)]
pub(crate) struct SendArgs {
    #[command(flatten)]
    pub(crate) common: CommonArgs,
    #[arg(long, default_value_t = 120)]
    pub(crate) frames: u64,
    #[arg(long)]
    pub(crate) batch: Option<usize>,
    #[arg(long, default_value_t = 0)]
    pub(crate) pacing_us: u64,
    #[arg(long, default_value = "motion", value_parser = parse_pattern)]
    pub(crate) pattern: Pattern,
    #[arg(long, default_value = "02:00:00:00:00:01", value_parser = parse_mac)]
    pub(crate) source_mac: [u8; 6],
    #[arg(long, default_value = "ff:ff:ff:ff:ff:ff", value_parser = parse_mac)]
    pub(crate) dst_mac: [u8; 6],
    #[arg(skip)]
    pub(crate) source: SourceConfig,
    #[arg(skip)]
    pub(crate) codec: CodecConfig,
    #[arg(long)]
    pub(crate) stats: Option<PathBuf>,
    #[arg(long)]
    pub(crate) hashlog: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub(crate) ready: Option<PathBuf>,
    #[arg(skip)]
    pub(crate) stop: Option<Arc<AtomicBool>>,
    #[arg(skip)]
    pub(crate) progress: Option<Arc<Progress>>,
    #[arg(skip)]
    pub(crate) hash_file: Option<File>,
    #[arg(skip)]
    pub(crate) stats_file: Option<File>,
    #[arg(skip)]
    pub(crate) operation: OperationState,
}

#[derive(Args)]
pub(crate) struct ReceiveArgs {
    #[command(flatten)]
    pub(crate) common: CommonArgs,
    #[arg(long, value_parser = parse_mac)]
    pub(crate) source_mac: Option<[u8; 6]>,
    #[arg(long, default_value_t = 120)]
    pub(crate) expect_frames: u64,
    #[arg(long, default_value_t = 30)]
    pub(crate) timeout_s: u64,
    #[arg(long)]
    pub(crate) ready: Option<PathBuf>,
    #[arg(long)]
    pub(crate) stats: Option<PathBuf>,
    #[arg(long)]
    pub(crate) hashlog: Option<PathBuf>,
    /// Override the legacy DShow mapping name; diagnostics should use a unique Local\\ name.
    #[arg(long)]
    pub(crate) shm: Option<String>,
    #[arg(skip)]
    pub(crate) codec: CodecConfig,
    #[arg(skip)]
    pub(crate) transport_file: Option<File>,
    #[arg(skip)]
    pub(crate) stop: Option<Arc<AtomicBool>>,
    #[arg(skip)]
    pub(crate) progress: Option<Arc<Progress>>,
    #[arg(skip)]
    pub(crate) hash_file: Option<File>,
    #[arg(skip)]
    pub(crate) stats_file: Option<File>,
    #[arg(skip)]
    pub(crate) operation: OperationState,
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

pub(crate) struct Npcap {
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
    pub(crate) fn load() -> anyhow::Result<Self> {
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

    pub(crate) fn adapters(&self) -> anyhow::Result<Vec<(String, String)>> {
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

    pub(crate) fn select_adapter(&self, requested: &str) -> anyhow::Result<String> {
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

pub(crate) struct Capture(*mut c_void, Close);
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

pub fn run() -> anyhow::Result<()> {
    CLI_STOP.store(false, Ordering::Relaxed);
    install_ctrl_handler()?;
    let frequency = query_frequency()?;
    let _ = QPC_FREQ.set(frequency);
    let cli = Cli::parse();
    match cli.command {
        Command::Send(args) => send(args),
        Command::Receive(args) => receive(args),
    }
}

pub(crate) struct Progress(Mutex<Value>);

impl Progress {
    pub(crate) fn new(initial: Value) -> Self {
        Self(Mutex::new(initial))
    }

    fn update(&self, value: &Value) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let (Some(current), Some(fields)) = (state.as_object_mut(), value.as_object()) {
            current.extend(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
    }

    pub(crate) fn snapshot(&self) -> Value {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

fn publish_progress(
    progress: Option<&Progress>,
    path: Option<&Path>,
    value: &Value,
) -> anyhow::Result<()> {
    if let Some(path) = path {
        atomic_write_json(path, value)?;
    }
    if let Some(progress) = progress {
        progress.update(value);
    }
    Ok(())
}

pub(crate) fn validate_sender(args: &SendArgs) -> anyhow::Result<()> {
    validate_common(&args.common)?;
    vc_source::validate(&args.source)?;
    validate_codec(&args.codec, &args.common)?;
    if args.source.mode == "pattern" {
        ensure!(
            args.source.transform == "crop",
            "pattern source does not support desktop transforms"
        );
    }
    if args.codec.codec != "raw" {
        ensure!(
            args.batch.is_none(),
            "encoded streams do not accept a fixed Raw batch"
        );
    }
    Ok(())
}

pub(crate) fn validate_receiver(args: &ReceiveArgs) -> anyhow::Result<()> {
    validate_common(&args.common)?;
    Config::new(geometry(&args.common)?, args.common.mtu)?;
    validate_codec(&args.codec, &args.common)?;
    if let Some(name) = args.shm.as_deref() {
        ensure!(
            !name.is_empty() && !name.contains('\0'),
            "SHM name must not be empty or contain NUL"
        );
    }
    Ok(())
}

fn validate_codec(codec: &CodecConfig, common: &CommonArgs) -> anyhow::Result<()> {
    vc_media::validate(codec, common.width, common.height, common.fps)
}

pub(crate) fn transport_path(hashlog: &Path) -> PathBuf {
    hashlog.with_file_name(format!(
        "{}.transport.jsonl",
        hashlog
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("hashlog")
    ))
}

pub(crate) fn capabilities() -> Value {
    json!({
        "stream_revision": 2,
        "variable_resolution": true,
        "shm_version": 3,
        "pixel_format": "nv12",
        "codecs": ["raw", "mjpeg", "pyrowave"],
        "sources": ["pattern", "dda", "wgc"],
        "sendqueue": "frame",
        "encoded_batch": null,
        "no_automatic_fallback": true,
    })
}

fn stopped(stop: Option<&AtomicBool>) -> bool {
    stop.unwrap_or(&CLI_STOP).load(Ordering::Acquire)
}

fn loop_sleep(target_ns: u64, stop: Option<&AtomicBool>) -> bool {
    loop {
        if stopped(stop) {
            return false;
        }
        let remaining = target_ns.saturating_sub(qpc_ns());
        if remaining == 0 {
            return true;
        }
        // High-resolution timer delivery can still arrive about half a
        // millisecond late. Wake ahead of that window, then use QPC for the
        // precision tail: relative send ceilings cannot recover oversleep by
        // sending early on the next frame. Keep the tail below one millisecond
        // so waiting does not consume most of the MMCSS CPU budget.
        if remaining > 800_000 {
            scheduling::wait_for((remaining - 700_000).min(5_000_000));
        } else {
            std::hint::spin_loop();
        }
    }
}

#[path = "windows/receiver.rs"]
mod receiver;
#[path = "windows/scheduling.rs"]
mod scheduling;
#[path = "windows/sender.rs"]
mod sender;
pub(crate) use receiver::receive;
use scheduling::ThreadPriority;
pub(crate) use sender::send;

fn timing_value(timing: TimingSummary) -> Value {
    json!({"samples":timing.samples,"avg_ms":timing.avg_ms,"p50_ms":timing.p50_ms,"p99_ms":timing.p99_ms,"max_ms":timing.max_ms})
}

fn atomic_write_json(path: &Path, value: &Value) -> anyhow::Result<()> {
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

fn take_writer(file: Option<File>, path: Option<&Path>) -> anyhow::Result<Option<BufWriter<File>>> {
    if let Some(file) = file {
        return Ok(Some(BufWriter::new(file)));
    }
    path.map(|path| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .with_context(|| {
                format!(
                    "create evidence {} (existing evidence is never overwritten)",
                    path.display()
                )
            })?;
        Ok(BufWriter::new(file))
    })
    .transpose()
}

fn write_stats_file(path: Option<&Path>, file: Option<File>, value: &Value) -> anyhow::Result<()> {
    let Some(mut writer) = take_writer(file, path)? else {
        return Ok(());
    };
    writer.seek(SeekFrom::Start(0))?;
    writer.get_ref().set_len(0)?;
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn au_session(codec: &str, width: u32, height: u32, fps: u32) -> anyhow::Result<AuSession> {
    match codec {
        "mjpeg" => Ok(AuSession::mjpeg(width, height, fps)),
        "pyrowave" => Ok(AuSession::pyrowave(width, height, fps)),
        _ => bail!("codec {codec} has no encoded access unit session"),
    }
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

pub(crate) fn initialize_timebase() -> anyhow::Result<()> {
    let frequency = query_frequency()?;
    let _ = QPC_FREQ.set(frequency);
    Ok(())
}

fn qpc_ns() -> u64 {
    let mut counter = MaybeUninit::<i64>::uninit();
    unsafe {
        QueryPerformanceCounter(counter.as_mut_ptr());
    }
    let ticks = unsafe { counter.assume_init() } as u64;
    let freq = *QPC_FREQ.get_or_init(|| {
        query_frequency().expect("QPC frequency must be initialized before data-plane use")
    });
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
        // The caller records capture start, before source/encoding/transmit work.
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

fn install_ctrl_handler() -> anyhow::Result<()> {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, PHANDLER_ROUTINE, SetConsoleCtrlHandler,
    };
    unsafe extern "system" fn handle(event: u32) -> i32 {
        if event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT {
            CLI_STOP.store(true, Ordering::Relaxed);
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
    fn frame_pacer_keeps_encoding_work_inside_the_frame_period() {
        let base = 100;
        let mut pacer = FramePacer::new(240, base);
        for _ in 0..240 {
            let capture_start = pacer.next_ns;
            pacer.advance(capture_start);
            // Representative codec work exceeds the wake-up jitter tolerance,
            // but finishes within one period and must not reduce the frame rate.
            let send_start = capture_start + 1_300_000;
            assert!(send_start < pacer.next_ns);
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
