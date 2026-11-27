//! 共享的进程内接收端。Npcap 线程只做捕获/重组/校验；由一个被 join 的
//! worker 完成全部解码、发布与解码后哈希。
use super::*;

use super::ThreadPriority;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Condvar, MutexGuard};
use std::thread::JoinHandle;

/// 最多允许两个完整 AU 等待这个单线程同步解码器。
const QUEUE_LIMIT: usize = 2;
const CAPTURE_BUFFER_BYTES: i32 = 128 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) fn receive(mut args: ReceiveArgs) -> anyhow::Result<()> {
    let requested = args.common.adapter.clone();
    let initial_session = args.common.session.clone();
    let initial_adapter = args.common.adapter.clone();
    let initial_codec = args.codec.codec.clone();
    let progress = args
        .progress
        .get_or_insert_with(|| {
            Arc::new(Progress::new(json!({
                "ready":false,"role":"receiver","session_id":initial_session,
                "adapter":initial_adapter,"codec":initial_codec,
            })))
        })
        .clone();
    if let Err(error) = validate_receiver(&args) {
        return fail_early(args, &progress, &requested, error);
    }
    let geometry = match geometry(&args.common) {
        Ok(geometry) => geometry,
        Err(error) => return fail_early(args, &progress, &requested, error),
    };
    let api = match Npcap::load() {
        Ok(api) => api,
        Err(error) => return fail_early(args, &progress, &requested, error),
    };
    let adapter = match api.select_adapter(&args.common.adapter) {
        Ok(adapter) => adapter,
        Err(error) => return fail_early(args, &progress, &requested, error),
    };
    if args.codec.codec == "raw" {
        receive_raw(args, progress, api, adapter, geometry)
    } else {
        receive_encoded(args, progress, api, adapter, geometry)
    }
}

// ---------------------------------------------------------------------------
// Raw：单线程、零拷贝的解码/发布/哈希路径。
// ---------------------------------------------------------------------------

use crate::shm::PublishToken;

const RX_AUDIT_QUEUE_CAP: usize = 4;

struct RawCounters {
    hashes: u64,
    hash_rows: u64,
    shm_published: u64,
    hash_mismatch: u64,
    bytes: u64,
    last_frame_id: Option<u32>,
    first_completed_ns: u64,
    last_completed_ns: u64,
    cancelled: bool,
    timed_out: bool,
}

impl RawCounters {
    fn new() -> Self {
        Self {
            hashes: 0,
            hash_rows: 0,
            shm_published: 0,
            hash_mismatch: 0,
            bytes: 0,
            last_frame_id: None,
            first_completed_ns: 0,
            last_completed_ns: 0,
            cancelled: false,
            timed_out: false,
        }
    }
}

struct RxAuditJob {
    frame_id: u32,
    pattern: u8,
    first_packet_ns: u64,
    completed_ns: u64,
    token: Option<PublishToken>,
    data: Vec<u8>,
}

struct RxAuditDone {
    token: Option<PublishToken>,
    hash: u64,
    mismatch: bool,
    logged: bool,
    buffer: Vec<u8>,
}

struct RxAuditWorker {
    tx: Option<mpsc::SyncSender<RxAuditJob>>,
    done_rx: mpsc::Receiver<RxAuditDone>,
    join: Option<JoinHandle<anyhow::Result<()>>>,
}

impl RxAuditWorker {
    fn spawn(
        geometry: Geometry,
        frame_bytes: usize,
        shm_enabled: bool,
        mut hashlog: Option<BufferedLog>,
        session: &str,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<RxAuditJob>(RX_AUDIT_QUEUE_CAP);
        let (done_tx, done_rx) = mpsc::channel::<RxAuditDone>();
        let need_hash = shm_enabled || hashlog.is_some();
        let join = thread::Builder::new()
            .name(format!("vc-stream-rx-audit-{session}"))
            .spawn(move || -> anyhow::Result<()> {
                let _priority = ThreadPriority::below_normal();
                let mut expected_frame = Vec::new();
                while let Ok(job) = rx.recv() {
                    let hash = if need_hash { fnv1a64(&job.data) } else { 0 };
                    let mismatch = if let Some(pattern) = Pattern::from_id(job.pattern) {
                        if expected_frame.len() != frame_bytes {
                            expected_frame.resize(frame_bytes, 0);
                        }
                        pattern.fill(geometry, job.frame_id, &mut expected_frame);
                        expected_frame != job.data
                    } else {
                        false
                    };
                    let logged = if let Some(writer) = hashlog.as_mut() {
                        writer.write_line(|w| {
                            writeln!(
                                w,
                                "{{\"frame_id\":{},\"hash\":\"{hash:016x}\",\"qpc_ns\":{},\"first_packet_qpc_ns\":{}}}",
                                job.frame_id, job.completed_ns, job.first_packet_ns
                            )
                        })?;
                        true
                    } else {
                        false
                    };
                    let _ = done_tx.send(RxAuditDone {
                        token: job.token,
                        hash,
                        mismatch,
                        logged,
                        buffer: job.data,
                    });
                }
                if let Some(writer) = hashlog.as_mut() {
                    writer.flush()?;
                }
                Ok(())
            })
            .context("spawn receiver audit worker")?;
        Ok(Self {
            tx: Some(tx),
            done_rx,
            join: Some(join),
        })
    }

    fn drain_done(
        &self,
        publisher: &mut Option<Publisher>,
        reassembler: Option<&mut Reassembler>,
        counters: &mut RawCounters,
    ) {
        let mut reassembler = reassembler;
        while let Ok(done) = self.done_rx.try_recv() {
            if let (Some(shm), Some(token)) = (publisher.as_mut(), done.token) {
                shm.mark_hash(token, done.hash);
                counters.shm_published += 1;
            }
            if done.mismatch {
                counters.hash_mismatch += 1;
            }
            if done.logged {
                counters.hash_rows += 1;
            }
            if let Some(r) = reassembler.as_deref_mut() {
                r.recycle(done.buffer);
            }
        }
    }

    fn submit(&self, job: RxAuditJob) -> anyhow::Result<()> {
        self.tx
            .as_ref()
            .context("receiver audit channel closed")?
            .send(job)
            .map_err(|_| anyhow::anyhow!("receiver audit worker exited unexpectedly"))
    }

    fn finish(
        mut self,
        publisher: &mut Option<Publisher>,
        counters: &mut RawCounters,
    ) -> anyhow::Result<()> {
        self.tx.take();
        let outcome = if let Some(join) = self.join.take() {
            match join.join() {
                Ok(res) => res,
                Err(_) => Err(anyhow::anyhow!("receiver audit worker panicked")),
            }
        } else {
            Ok(())
        };
        self.drain_done(publisher, None, counters);
        outcome
    }
}

impl Drop for RxAuditWorker {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn receive_raw(
    mut args: ReceiveArgs,
    progress: Arc<Progress>,
    api: Npcap,
    adapter: String,
    geometry: Geometry,
) -> anyhow::Result<()> {
    let _priority = ThreadPriority::capture();
    let frame_bytes = geometry.frame_bytes();
    let mut hashlog = match take_log_writer(args.hash_file.take(), args.hashlog.as_deref()) {
        Ok(writer) => writer,
        Err(error) => {
            return fail_early(args, &progress, &adapter, error.context("open Raw hashlog"));
        }
    };
    let handle = match api.open(&adapter, (MAX_CAPTURE + 14) as c_int, 1) {
        Ok(handle) => handle,
        Err(error) => return fail_early(args, &progress, &adapter, error),
    };
    let _capture = Capture(handle, api.close);
    let linktype = unsafe { (api.datalink)(handle) };
    if linktype == 1 {
        let _ = api.set_ethertype_filter(handle, args.common.ethertype);
    }
    unsafe {
        let _ = (api.set_buff)(handle, CAPTURE_BUFFER_BYTES);
        let _ = (api.set_min_to_copy)(handle, 0);
    }
    let shm_name = args.shm.clone();
    let mut publisher = match shm_name.as_deref() {
        Some(name) => match Publisher::new(name, geometry, args.common.fps) {
            Ok(publisher) => Some(publisher),
            Err(error) => {
                return fail_early(
                    args,
                    &progress,
                    &adapter,
                    anyhow::Error::new(error)
                        .context(format!("create requested SHM v3 mapping {name}")),
                );
            }
        },
        None => None,
    };
    let config = match Config::new(geometry, args.common.mtu) {
        Ok(config) => config.with_source_mac(args.source_mac),
        Err(error) => {
            return fail_early(
                args,
                &progress,
                &adapter,
                anyhow::Error::new(error).context("create Raw transport config"),
            );
        }
    };
    let mut reassembler = match Reassembler::new(config) {
        Ok(reassembler) => reassembler,
        Err(error) => {
            return fail_early(
                args,
                &progress,
                &adapter,
                anyhow::Error::new(error).context("create Raw reassembler"),
            );
        }
    };

    let shm_enabled = publisher.is_some();
    let started = Instant::now();
    let deadline = if args.timeout_s > 0 {
        started.checked_add(Duration::from_secs(args.timeout_s))
    } else {
        None
    };
    let ready = json!({
        "ready":true,"running":true,"role":"receiver","session_id":args.common.session,
        "scheduling":_priority.mode(),
        "adapter":adapter,"geometry":{"width":args.common.width,"height":args.common.height,"fps":args.common.fps},
        "width":args.common.width,"height":args.common.height,"fps":args.common.fps,
        "fps_limit":args.common.fps,"frame_bytes":frame_bytes,"mtu":args.common.mtu,
        "ethertype":format!("0x{:04x}",args.common.ethertype),"codec":"raw","batch":null,
        "decode_in_flight":args.codec.decode_in_flight,
        "decode_in_flight_limit":args.codec.decode_in_flight,
        "shm_enabled":shm_enabled,"shm_version":if shm_enabled { 3 } else { 0 },
        "datalink":linktype,"npcap":api.version(),
        "capture_buffer_bytes":CAPTURE_BUFFER_BYTES,"mintocopy":0,
        "expect_frames":args.expect_frames,"timeout_s":args.timeout_s,
    });
    let mut ready_published = false;
    let mut outcome = match publish_progress(Some(progress.as_ref()), args.ready.as_deref(), &ready)
    {
        Ok(()) => {
            ready_published = true;
            Ok(())
        }
        Err(error) => Err(error.context("publish receiver readiness")),
    };
    let cpu_start = process_cpu_ns().ok();
    let mut counters = RawCounters::new();
    let mut vstat = VstatReader::new(shm_name.as_deref());
    let mut audit_worker = match RxAuditWorker::spawn(
        geometry,
        frame_bytes,
        shm_enabled,
        hashlog.take(),
        &args.common.session,
    ) {
        Ok(worker) => Some(worker),
        Err(error) => {
            outcome = merge_error(outcome, error);
            None
        }
    };
    if outcome.is_ok()
        && let Some(audit) = audit_worker.as_ref()
    {
        let loop_result = catch_unwind(AssertUnwindSafe(|| {
            raw_capture_loop(
                &args,
                &api,
                handle,
                geometry,
                &mut reassembler,
                &mut publisher,
                audit,
                &mut counters,
                &mut vstat,
                started,
                deadline,
            )
        }));
        outcome = match loop_result {
            Ok(result) => result,
            Err(panic) => Err(anyhow::anyhow!(
                "Raw receiver loop panicked: {}",
                panic_text(panic.as_ref())
            )),
        };
    }
    reassembler.finish();
    if let Some(audit) = audit_worker.take()
        && let Err(error) = audit.finish(&mut publisher, &mut counters)
    {
        outcome = merge_error(outcome, error.context("finish Raw audit worker"));
    }
    if let Some(writer) = hashlog.as_mut()
        && let Err(error) = writer.flush()
    {
        outcome = merge_error(outcome, error.context("flush Raw hashlog"));
    }
    let stats = reassembler.stats();
    if outcome.is_ok() && !counters.cancelled && args.expect_frames > 0 {
        if counters.hashes != args.expect_frames {
            outcome = Err(anyhow::anyhow!(
                "receiver got {}/{} frames before {}",
                counters.hashes,
                args.expect_frames,
                if counters.timed_out {
                    "timeout"
                } else {
                    "stop"
                }
            ));
        } else if stats.dropped > 0 || stats.packets_lost > 0 || counters.hash_mismatch > 0 {
            outcome = Err(anyhow::anyhow!(
                "receiver finished with drops={}, packets_lost={}, hash_mismatch={}",
                stats.dropped,
                stats.packets_lost,
                counters.hash_mismatch
            ));
        }
    }
    let elapsed = started.elapsed().as_secs_f64().max(0.000_001);
    let cpu_pct = match (cpu_start, process_cpu_ns().ok()) {
        (Some(start), Some(end)) => {
            Some((end.saturating_sub(start) as f64 / 1_000_000_000.0) / elapsed * 100.0)
        }
        _ => None,
    };
    let mut value = json!({
        "ready":ready_published,"running":false,"role":"receiver","session_id":args.common.session,
        "adapter":adapter,"geometry":{"width":args.common.width,"height":args.common.height,"fps":args.common.fps},
        "width":args.common.width,"height":args.common.height,"fps":args.common.fps,
        "fps_limit":args.common.fps,"frame_bytes":frame_bytes,"mtu":args.common.mtu,
        "ethertype":format!("0x{:04x}",args.common.ethertype),"codec":"raw","batch":null,
        "complete":stats.complete,"hash_rows":counters.hash_rows,
    });
    add_json_fields(
        &mut value,
        json!({
            "dropped":stats.dropped,"packets_recv":stats.packets_recv,
            "packets_lost":stats.packets_lost,"transport_errors":stats.packets_lost,
            "crc_errors":stats.crc_errors,"duplicate":stats.duplicate,"gaps":stats.gaps,
            "late":stats.late,"ignored_other_source":stats.ignored_other_source,
            "hash_mismatch":counters.hash_mismatch,"shm_published":counters.shm_published,
            "shm_enabled":shm_enabled,"shm_version":if shm_enabled { 3 } else { 0 },
            "bytes_captured":counters.bytes,"bytes_recv":stats.bytes_recv,
        }),
    );
    let (vcam_delivered, vseg_p99_ms, vseg) = vstat.final_fields();
    add_json_fields(
        &mut value,
        json!({
            "pps":stats.packets_recv as f64/elapsed,"bps":counters.bytes as f64*8.0/elapsed,
            "fps_actual":counters.hashes as f64/elapsed,"cpu_pct":cpu_pct,"uptime_s":elapsed,
            "inter_arrival":timing_value(reassembler.inter_arrival_summary()),
            "rseg":timing_value(reassembler.rseg_summary()),
            "vcam_delivered":vcam_delivered,
            "vseg_p99_ms":vseg_p99_ms,
            "vseg":vseg,
            "last_frame_id":counters.last_frame_id,
            "first_completed_qpc_ns":counters.first_completed_ns,
            "last_completed_qpc_ns":counters.last_completed_ns,
            "capture_datalink":linktype,"inflight_frames":reassembler.inflight_frames(),
            "inflight_bytes":reassembler.inflight_bytes(),
        }),
    );
    add_json_fields(
        &mut value,
        json!({
            "decode_errors":0,"decode_resets":0,"decode_discarded":0,"decode_queue_drops":0,
            "decode_pending":0,"decode_in_flight_limit":args.codec.decode_in_flight,
            "waiting_idr":false,"pixel_equality_gate":true,
            "failed":outcome.is_err(),"error":outcome.as_ref().err().map(|error|format!("{error:#}")),
            "cancelled":counters.cancelled,"timed_out":counters.timed_out,
            "capture_buffer_bytes":CAPTURE_BUFFER_BYTES,"mintocopy":0,
            "expect_frames":args.expect_frames,"timeout_s":args.timeout_s,
        }),
    );

    println!(
        "[vc-stream] receive done adapter={adapter} session={} complete={} drops={} gaps={} hash_mismatch={}",
        args.common.session, stats.complete, stats.dropped, stats.gaps, counters.hash_mismatch
    );
    finish_receiver(
        &progress,
        args.stats.take(),
        args.stats_file.take(),
        &value,
        outcome,
    )
}

struct RawDispatchCtx<'a> {
    args: &'a ReceiveArgs,
    api: &'a Npcap,
    handle: *mut c_void,
    frame_bytes: usize,
    reassembler: &'a mut Reassembler,
    publisher: &'a mut Option<Publisher>,
    audit: &'a RxAuditWorker,
    counters: &'a mut RawCounters,
    vstat: &'a mut VstatReader,
    started: Instant,
    deadline: Option<Instant>,
    rate_window: RateWindow,
    live_rseg: TimedWindow,
    live_inter_arrival: TimedWindow,
    last_progress: Instant,
    pkt_tick: u32,
    stop_loop: bool,
    error: Option<anyhow::Error>,
}

impl RawDispatchCtx<'_> {
    fn on_packet(&mut self, header: *const PcapHeader, data: *const c_uchar) -> anyhow::Result<()> {
        if self.stop_loop {
            return Ok(());
        }
        if self.pkt_tick & 15 == 0 && stopped(self.args.stop.as_deref()) {
            self.counters.cancelled = true;
            self.stop_loop = true;
            unsafe { (self.api.breakloop)(self.handle) };
            return Ok(());
        }
        if self.pkt_tick & 63 == 0 {
            let now = Instant::now();
            if self.deadline.is_some_and(|deadline| now >= deadline) {
                self.counters.timed_out = true;
                self.stop_loop = true;
                unsafe { (self.api.breakloop)(self.handle) };
                return Ok(());
            }
            if now.duration_since(self.last_progress) >= PROGRESS_INTERVAL {
                self.audit
                    .drain_done(self.publisher, Some(self.reassembler), self.counters);
                live_raw_progress(
                    self.args,
                    self.reassembler,
                    self.counters,
                    self.vstat,
                    self.started,
                    &mut self.rate_window,
                    &mut self.live_rseg,
                    &mut self.live_inter_arrival,
                );
                self.last_progress = now;
            }
        }
        self.pkt_tick = self.pkt_tick.wrapping_add(1);
        if header.is_null() || data.is_null() {
            return Ok(());
        }
        let caplen = unsafe { (*header).caplen as usize };
        if !(14 + 28..=MAX_CAPTURE).contains(&caplen) {
            return Ok(());
        }
        let packet = unsafe { std::slice::from_raw_parts(data, caplen) };
        self.counters.bytes = self.counters.bytes.saturating_add(caplen as u64);
        if let PushOutcome::Completed(frame) = self
            .reassembler
            .push_ethernet_timed_lazy(packet, qpc_ns, qpc_ns)
        {
            ensure!(
                frame.data.len() == self.frame_bytes,
                "completed frame {} has {} bytes, expected {}",
                frame.frame_id,
                frame.data.len(),
                self.frame_bytes
            );
            // 先发布到 SHM，再把 FNV-1a64、图案比对与 hashlog 落盘交给有界审计线程。
            let token = if let Some(shm) = self.publisher.as_mut() {
                Some(shm.publish(frame.frame_id, frame.first_packet_ns, &frame.data, qpc_ns)?)
            } else {
                None
            };
            self.audit
                .drain_done(self.publisher, Some(self.reassembler), self.counters);
            let frame_id = frame.frame_id;
            let first_packet_ns = frame.first_packet_ns;
            let completed_ns = frame.completed_ns;
            let pattern = frame.pattern;
            self.audit.submit(RxAuditJob {
                frame_id,
                pattern,
                first_packet_ns,
                completed_ns,
                token,
                data: frame.data,
            })?;
            self.live_rseg
                .push(completed_ns, completed_ns.saturating_sub(first_packet_ns));
            if self.counters.hashes > 0 && self.counters.last_completed_ns > 0 {
                self.live_inter_arrival.push(
                    completed_ns,
                    completed_ns.saturating_sub(self.counters.last_completed_ns),
                );
            }
            self.counters.hashes += 1;
            if self.counters.hashes == 1 {
                self.counters.first_completed_ns = completed_ns;
            }
            self.counters.last_completed_ns = completed_ns;
            self.counters.last_frame_id = Some(frame_id);
            self.rate_window.maybe_record(
                completed_ns,
                self.counters.hashes,
                self.reassembler.stats().packets_recv,
                self.counters.bytes,
            );
            self.pkt_tick = 0;
            if self.args.expect_frames > 0 && self.counters.hashes >= self.args.expect_frames {
                self.stop_loop = true;
                unsafe { (self.api.breakloop)(self.handle) };
            }
        }
        Ok(())
    }
}

unsafe extern "C" fn raw_dispatch_cb(
    user: *mut c_uchar,
    header: *const PcapHeader,
    data: *const c_uchar,
) {
    let ctx = unsafe { &mut *user.cast::<RawDispatchCtx<'_>>() };
    if ctx.stop_loop {
        return;
    }
    match catch_unwind(AssertUnwindSafe(|| ctx.on_packet(header, data))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            ctx.error = Some(error);
            ctx.stop_loop = true;
            unsafe { (ctx.api.breakloop)(ctx.handle) };
        }
        Err(panic) => {
            ctx.error = Some(anyhow::anyhow!(
                "Raw dispatch callback panicked: {}",
                panic_text(panic.as_ref())
            ));
            ctx.stop_loop = true;
            unsafe { (ctx.api.breakloop)(ctx.handle) };
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn raw_capture_loop(
    args: &ReceiveArgs,
    api: &Npcap,
    handle: *mut c_void,
    geometry: Geometry,
    reassembler: &mut Reassembler,
    publisher: &mut Option<Publisher>,
    audit: &RxAuditWorker,
    counters: &mut RawCounters,
    vstat: &mut VstatReader,
    started: Instant,
    deadline: Option<Instant>,
) -> anyhow::Result<()> {
    let mut ctx = RawDispatchCtx {
        args,
        api,
        handle,
        frame_bytes: geometry.frame_bytes(),
        reassembler,
        publisher,
        audit,
        counters,
        vstat,
        started,
        deadline,
        rate_window: RateWindow::new(qpc_ns()),
        live_rseg: TimedWindow::default(),
        live_inter_arrival: TimedWindow::default(),
        last_progress: Instant::now(),
        pkt_tick: 0,
        stop_loop: false,
        error: None,
    };
    loop {
        if stopped(ctx.args.stop.as_deref()) {
            ctx.counters.cancelled = true;
            break;
        }
        if ctx.args.expect_frames > 0 && ctx.counters.hashes >= ctx.args.expect_frames {
            break;
        }
        let now = Instant::now();
        if ctx.deadline.is_some_and(|deadline| now >= deadline) {
            ctx.counters.timed_out = true;
            break;
        }
        // 空闲超时、无关包或畸形包也不能冻结实际帧率；最近 1 秒滑窗会在空闲后归零。
        if now.duration_since(ctx.last_progress) >= PROGRESS_INTERVAL {
            ctx.audit
                .drain_done(ctx.publisher, Some(ctx.reassembler), ctx.counters);
            live_raw_progress(
                ctx.args,
                ctx.reassembler,
                ctx.counters,
                ctx.vstat,
                ctx.started,
                &mut ctx.rate_window,
                &mut ctx.live_rseg,
                &mut ctx.live_inter_arrival,
            );
            ctx.last_progress = now;
        }
        let rc = unsafe {
            (ctx.api.dispatch)(
                ctx.handle,
                256,
                raw_dispatch_cb,
                (&raw mut ctx).cast::<c_uchar>(),
            )
        };
        if let Some(error) = ctx.error.take() {
            return Err(error);
        }
        if ctx.stop_loop {
            break;
        }
        if rc == -1 {
            return Err(ctx
                .api
                .handle_error(ctx.handle, &format!("pcap_dispatch returned {rc}")));
        }
        if rc == -2 {
            break;
        }
        if rc == 0 {
            ctx.pkt_tick = 0;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn live_raw_progress(
    args: &ReceiveArgs,
    reassembler: &Reassembler,
    counters: &RawCounters,
    vstat: &mut VstatReader,
    started: Instant,
    rate_window: &mut RateWindow,
    live_rseg: &mut TimedWindow,
    live_inter_arrival: &mut TimedWindow,
) {
    let now_ns = qpc_ns();
    let (vcam_delivered, vseg_p99_ms, vseg) = vstat.live_fields(now_ns);
    if let Some(progress) = args.progress.as_deref() {
        let stats = reassembler.stats();
        let rate = rate_window.sample(now_ns, counters.hashes, stats.packets_recv, counters.bytes);
        let rseg = live_rseg.summary_at(now_ns);
        let inter_arrival = live_inter_arrival.summary_at(now_ns);
        let uptime = started.elapsed().as_secs_f64().max(0.000_001);
        progress.update(&json!({
            "running":true,"complete":stats.complete,"hash_rows":counters.hash_rows,
            "dropped":stats.dropped,"packets_recv":stats.packets_recv,
            "packets_lost":stats.packets_lost,"crc_errors":stats.crc_errors,
            "duplicate":stats.duplicate,"gaps":stats.gaps,
            "hash_mismatch":counters.hash_mismatch,
            "shm_published":counters.shm_published,
            "vcam_delivered":vcam_delivered,
            "vseg_p99_ms":vseg_p99_ms,
            "vseg":vseg,
            "bytes_captured":counters.bytes,
            "pps":rate.pps,
            "bps":rate.bps,
            "fps_actual":rate.fps,"uptime_s":uptime,
            "rseg":rseg,
            "inter_arrival":inter_arrival,
            "last_frame_id":counters.last_frame_id,
            "first_completed_qpc_ns":counters.first_completed_ns,
            "last_completed_qpc_ns":counters.last_completed_ns,
        }));
    }
}

// ---------------------------------------------------------------------------
// 编码：捕获线程负责重组/校验，另有一个解码 worker。
// ---------------------------------------------------------------------------

struct Work {
    au: Vec<u8>,
    frame_id: u32,
    first_ns: u64,
    queued_ns: u64,
    transport_hash: u64,
    key_frame: bool,
    discontinuity: bool,
    eos: bool,
}

struct Queue {
    jobs: VecDeque<Work>,
    stopping: bool,
}

struct SharedDecoder {
    queue: Mutex<Queue>,
    cv: Condvar,
    queue_drops: AtomicU64,
    queue_backpressure_events: AtomicU64,
    queue_backpressure_ns: AtomicU64,
    queue_backpressure_max_ns: AtomicU64,
    resets: AtomicU64,
    discarded: AtomicU64,
    decode_errors: AtomicU64,
    pending: AtomicU64,
    waiting_idr: AtomicBool,
    decoded: AtomicU64,
    shm_published: AtomicU64,
    last_frame_id_plus_one: AtomicU64,
    first_completed_ns: AtomicU64,
    last_completed_ns: AtomicU64,
    failed: AtomicBool,
    error: Mutex<Option<String>>,
}

impl SharedDecoder {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Queue {
                jobs: VecDeque::new(),
                stopping: false,
            }),
            cv: Condvar::new(),
            queue_drops: AtomicU64::new(0),
            queue_backpressure_events: AtomicU64::new(0),
            queue_backpressure_ns: AtomicU64::new(0),
            queue_backpressure_max_ns: AtomicU64::new(0),
            resets: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
            pending: AtomicU64::new(0),
            waiting_idr: AtomicBool::new(false),
            decoded: AtomicU64::new(0),
            shm_published: AtomicU64::new(0),
            last_frame_id_plus_one: AtomicU64::new(0),
            first_completed_ns: AtomicU64::new(0),
            last_completed_ns: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
        }
    }

    fn fail(&self, text: String) {
        {
            let mut error = lock(&self.error);
            if error.is_none() {
                *error = Some(text);
            }
        }
        // 先发布文本，再设置观察者用作 acquire 侧的标志。
        self.failed.store(true, Ordering::Release);
    }

    fn error(&self) -> Option<String> {
        lock(&self.error).clone()
    }

    fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    fn decoded(&self) -> u64 {
        self.decoded.load(Ordering::Acquire)
    }
}

struct PendingGuard<'a>(&'a AtomicU64);
impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

struct WorkerHandle {
    shared: Arc<SharedDecoder>,
    worker: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    /// 请求关闭队列，并只 join 这一个 worker。若某次原生 decode/Drop 永不返回，
    /// 该 join 会一直挂起；这会有意让所属 ABI 会话与 DLL 映像继续保持存活。
    fn shutdown(&mut self) -> Option<String> {
        {
            let mut queue = lock(&self.shared.queue);
            queue.stopping = true;
            self.shared.cv.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.shared.error()
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

enum WorkerInit {
    Ready { settings: Value, shm_enabled: bool },
    Failed(String),
}

#[derive(Default)]
struct EncodedCounters {
    valid_units: u64,
    invalid_au: u64,
    received_eos: bool,
    cancelled: bool,
    timed_out: bool,
}

fn receive_encoded(
    mut args: ReceiveArgs,
    progress: Arc<Progress>,
    api: Npcap,
    adapter: String,
    geometry: Geometry,
) -> anyhow::Result<()> {
    let _priority = ThreadPriority::capture();
    let frame_bytes = geometry.frame_bytes();
    let transport_path = args.hashlog.as_deref().map(transport_path);
    let mut transport = match take_log_writer(args.transport_file.take(), transport_path.as_deref())
    {
        Ok(writer) => writer,
        Err(error) => {
            return fail_early(
                args,
                &progress,
                &adapter,
                error.context("open transport log"),
            );
        }
    };
    let mut reassembler =
        match EncodedReassembler::new(args.common.mtu, args.codec.max_au_bytes, args.source_mac) {
            Ok(reassembler) => reassembler,
            Err(error) => {
                return fail_early(
                    args,
                    &progress,
                    &adapter,
                    anyhow::Error::new(error).context("create encoded reassembler"),
                );
            }
        };
    let session = match au_session(
        &args.codec.codec,
        args.common.width,
        args.common.height,
        args.common.fps,
    ) {
        Ok(session) => session,
        Err(error) => return fail_early(args, &progress, &adapter, error),
    };
    let handle = match api.open(&adapter, (MAX_CAPTURE + 14) as c_int, 1) {
        Ok(handle) => handle,
        Err(error) => return fail_early(args, &progress, &adapter, error),
    };
    let _capture = Capture(handle, api.close);
    let linktype = unsafe { (api.datalink)(handle) };
    if linktype == 1 {
        let _ = api.set_ethertype_filter(handle, args.common.ethertype);
    }
    unsafe {
        let _ = (api.set_buff)(handle, CAPTURE_BUFFER_BYTES);
        let _ = (api.set_min_to_copy)(handle, 0);
    }

    let shared = Arc::new(SharedDecoder::new());
    let (init_tx, init_rx) = mpsc::channel();
    let (recycle_tx, recycle_rx) = mpsc::channel::<Vec<u8>>();
    let need_transport_hash = transport.is_some() || args.hashlog.is_some();
    let worker_codec = args.codec.clone();
    let worker_shm = args.shm.clone();
    let worker_hashlog = args.hashlog.clone();
    let worker_hash_file = args.hash_file.take();
    let worker_operation = args.operation.clone();
    let worker_shared = shared.clone();
    let fail_shared = shared.clone();
    let fps = args.common.fps;
    let worker_name = format!("vc-stream-receiver-{}", args.common.session);
    let worker = match thread::Builder::new().name(worker_name).spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| {
            worker_main(
                worker_codec,
                geometry,
                fps,
                worker_shm,
                worker_hash_file,
                worker_hashlog,
                worker_operation,
                worker_shared,
                init_tx,
                recycle_tx,
            )
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => fail_shared.fail(format!("{error:#}")),
            Err(panic) => fail_shared.fail(format!(
                "decoder worker panicked: {}",
                panic_text(panic.as_ref())
            )),
        }
        fail_shared.cv.notify_all();
    }) {
        Ok(worker) => worker,
        Err(error) => {
            return fail_early(
                args,
                &progress,
                &adapter,
                anyhow::Error::new(error).context("start decoder worker"),
            );
        }
    };
    let mut worker = WorkerHandle {
        shared: shared.clone(),
        worker: Some(worker),
    };

    let started = Instant::now();
    let deadline = if args.timeout_s > 0 {
        started.checked_add(Duration::from_secs(args.timeout_s))
    } else {
        None
    };
    let cpu_start = process_cpu_ns().ok();
    let mut counters = EncodedCounters::default();
    let mut settings = Value::Null;
    let mut shm_enabled = false;
    let mut cancelled = false;
    let mut outcome: anyhow::Result<()> = Ok(());
    loop {
        if stopped(args.stop.as_deref()) {
            cancelled = true;
            break;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            counters.timed_out = true;
            outcome = Err(anyhow::anyhow!(
                "receiver timed out after {} s during initialization",
                args.timeout_s
            ));
            break;
        }
        match init_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(WorkerInit::Ready {
                settings: worker_settings,
                shm_enabled: worker_shm_enabled,
            }) => {
                settings = worker_settings;
                shm_enabled = worker_shm_enabled;
                break;
            }
            Ok(WorkerInit::Failed(text)) => {
                outcome = Err(anyhow::anyhow!(text));
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if args.operation.timed_out() {
                    let message = operation_timeout_message(&args.operation);
                    if let Some(progress) = args.progress.as_deref() {
                        progress.update(&json!({
                            "failed":true,"cleanup_pending":true,
                            "operation_timeout":args.operation.timed_out_operation(),
                            "error":message,
                        }));
                    }
                    outcome = Err(anyhow::anyhow!(operation_timeout_message(&args.operation)));
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                outcome = Err(anyhow::anyhow!(
                    "decoder worker ended during initialization before reporting readiness"
                ));
                break;
            }
        }
    }
    if outcome.is_ok() && stopped(args.stop.as_deref()) {
        cancelled = true;
        counters.cancelled = true;
    }
    if outcome.is_ok() && !cancelled {
        let ready = json!({
            "ready":true,"running":true,"role":"receiver","session_id":args.common.session,
            "adapter":adapter,"geometry":{"width":args.common.width,"height":args.common.height,"fps":args.common.fps},
            "width":args.common.width,"height":args.common.height,"fps":args.common.fps,
            "fps_limit":args.common.fps,"frame_bytes":frame_bytes,"mtu":args.common.mtu,
            "ethertype":format!("0x{:04x}",args.common.ethertype),"codec":args.codec.codec,
            "batch":null,"decode_in_flight":args.codec.decode_in_flight,
            "decode_in_flight_limit":args.codec.decode_in_flight,
            "shm_enabled":shm_enabled,"shm_version":if shm_enabled { 3 } else { 0 },
            "datalink":linktype,"npcap":api.version(),"decoder_configuration":settings,
            "decoder_backend":args.codec.decoder_backend,"jpeg_quality":args.codec.jpeg_quality,
            "max_au_bytes":args.codec.max_au_bytes,
            "capture_buffer_bytes":CAPTURE_BUFFER_BYTES,"mintocopy":0,
            "scheduling":_priority.mode(),
            "expect_frames":args.expect_frames,"timeout_s":args.timeout_s,
        });
        match publish_progress(Some(progress.as_ref()), args.ready.as_deref(), &ready) {
            Ok(()) => {}
            Err(error) => outcome = Err(error.context("publish receiver readiness")),
        }
    }
    let ready_published = outcome.is_ok() && !cancelled;
    let mut vstat = VstatReader::new(args.shm.as_deref());
    if ready_published {
        let loop_result = catch_unwind(AssertUnwindSafe(|| {
            encoded_capture_loop(
                &args,
                &api,
                handle,
                session,
                &mut reassembler,
                &mut transport,
                &shared,
                &recycle_rx,
                need_transport_hash,
                &mut counters,
                &mut vstat,
                deadline,
            )
        }));
        outcome = match loop_result {
            Ok(result) => result,
            Err(panic) => Err(anyhow::anyhow!(
                "encoded receiver loop panicked: {}",
                panic_text(panic.as_ref())
            )),
        };
    }
    let worker_error = worker.shutdown();
    reassembler.finish();
    if let Some(writer) = transport.as_mut()
        && let Err(error) = writer.flush()
    {
        outcome = merge_error(outcome, error.context("flush transport log"));
    }

    let decoded = shared.decoded();
    let resets = shared.resets.load(Ordering::Acquire);
    let discarded = shared.discarded.load(Ordering::Acquire);
    let queue_drops = shared.queue_drops.load(Ordering::Acquire);
    let queue_backpressure_events = shared.queue_backpressure_events.load(Ordering::Acquire);
    let queue_backpressure_ns = shared.queue_backpressure_ns.load(Ordering::Acquire);
    let queue_backpressure_max_ns = shared.queue_backpressure_max_ns.load(Ordering::Acquire);
    let decode_errors = shared.decode_errors.load(Ordering::Acquire);
    let pending = shared.pending.load(Ordering::Acquire);
    let waiting_idr = shared.waiting_idr.load(Ordering::Acquire);
    let shm_published = shared.shm_published.load(Ordering::Acquire);
    let last_frame_id = shared
        .last_frame_id_plus_one
        .load(Ordering::Acquire)
        .checked_sub(1)
        .map(|value| value as u32);
    let first_completed_ns = shared.first_completed_ns.load(Ordering::Acquire);
    let last_completed_ns = shared.last_completed_ns.load(Ordering::Acquire);
    let timed_out = counters.timed_out || args.operation.timed_out();
    outcome = combine_worker_error(outcome, worker_error);
    if outcome.is_ok()
        && !counters.cancelled
        && args.expect_frames > 0
        && decoded != args.expect_frames
    {
        outcome = Err(anyhow::anyhow!(
            "receiver decoded {decoded}/{} frames before {}",
            args.expect_frames,
            if counters.timed_out {
                "timeout"
            } else {
                "stop"
            }
        ));
    }
    if outcome.is_ok() && args.operation.timed_out() {
        outcome = Err(anyhow::anyhow!(operation_timeout_message(&args.operation)));
    }

    let stats = reassembler.stats();
    let elapsed = started.elapsed().as_secs_f64().max(0.000_001);
    let cpu_pct = match (cpu_start, process_cpu_ns().ok()) {
        (Some(start), Some(end)) => {
            Some((end.saturating_sub(start) as f64 / 1_000_000_000.0) / elapsed * 100.0)
        }
        _ => None,
    };
    let mut rseg = timing_value(reassembler.rseg_summary());
    rseg["scope"] = json!("first_fragment_to_complete_au");
    let transport_errors = counters.invalid_au.saturating_add(stats.packets_lost);
    let mut value = json!({
        "ready":ready_published,"running":false,"role":"receiver","session_id":args.common.session,
        "adapter":adapter,"geometry":{"width":args.common.width,"height":args.common.height,"fps":args.common.fps},
        "width":args.common.width,"height":args.common.height,"fps":args.common.fps,
        "fps_limit":args.common.fps,"frame_bytes":frame_bytes,"mtu":args.common.mtu,
        "ethertype":format!("0x{:04x}",args.common.ethertype),"codec":args.codec.codec,
        "batch":null,"complete":decoded,"compressed_complete":counters.valid_units,
    });
    add_json_fields(
        &mut value,
        json!({
            "transport_frames":counters.valid_units,"hash_rows":decoded,
            "dropped":stats.dropped,"packets_recv":stats.packets_recv,
            "packets_lost":stats.packets_lost,"transport_errors":transport_errors,
            "crc_errors":stats.crc_errors,"duplicate":stats.duplicate,"gaps":stats.gaps,
            "late":stats.late,"ignored_other_source":stats.ignored_other_source,
            "hash_mismatch":0,"pixel_equality_gate":false,
        }),
    );
    add_json_fields(
        &mut value,
        json!({
            "decode_errors":decode_errors,"resets":resets,"decode_resets":resets,
            "discarded":discarded,"decode_discarded":discarded,
            "queue_drops":queue_drops,"decode_queue_drops":queue_drops,
            "decode_queue_backpressure_events":queue_backpressure_events,
            "decode_queue_backpressure_ms":queue_backpressure_ns as f64/1e6,
            "decode_queue_backpressure_max_ms":queue_backpressure_max_ns as f64/1e6,
            "decode_pending":pending,"decode_in_flight":args.codec.decode_in_flight,
            "decode_in_flight_limit":args.codec.decode_in_flight,
            "waiting_idr":waiting_idr,"shm_published":shm_published,
            "shm_enabled":shm_enabled,"shm_version":if shm_enabled { 3 } else { 0 },
            "bytes_captured":stats.bytes_recv,"bytes_recv":stats.bytes_recv,
        }),
    );
    let (vcam_delivered, vseg_p99_ms, vseg) = vstat.final_fields();
    add_json_fields(
        &mut value,
        json!({
            "pps":stats.packets_recv as f64/elapsed,"bps":stats.bytes_recv as f64*8.0/elapsed,
            "fps_actual":decoded as f64/elapsed,"cpu_pct":cpu_pct,"uptime_s":elapsed,
            "inter_arrival":timing_value(reassembler.inter_arrival_summary()),
            "rseg":rseg,
            "vcam_delivered":vcam_delivered,
            "vseg_p99_ms":vseg_p99_ms,
            "vseg":vseg,
            "last_frame_id":last_frame_id,
            "first_completed_qpc_ns":first_completed_ns,
            "last_completed_qpc_ns":last_completed_ns,
            "capture_datalink":linktype,"inflight_frames":reassembler.inflight_frames(),
            "inflight_bytes":reassembler.inflight_bytes(),
        }),
    );
    add_json_fields(
        &mut value,
        json!({
            "decoder_configuration":settings,"decoder_backend":args.codec.decoder_backend,
            "jpeg_quality":args.codec.jpeg_quality,"max_au_bytes":args.codec.max_au_bytes,
            "failed":outcome.is_err(),"error":outcome.as_ref().err().map(|error|format!("{error:#}")),
            "cancelled":counters.cancelled,"timed_out":timed_out,
            "capture_buffer_bytes":CAPTURE_BUFFER_BYTES,"mintocopy":0,
            "expect_frames":args.expect_frames,"timeout_s":args.timeout_s,
        }),
    );

    println!(
        "[vc-stream] receive done adapter={adapter} session={} codec={} decoded={} transport_errors={} resets={} drops={}",
        args.common.session, args.codec.codec, decoded, transport_errors, resets, stats.dropped
    );
    finish_receiver(
        &progress,
        args.stats.take(),
        args.stats_file.take(),
        &value,
        outcome,
    )
}

struct EncodedDispatchCtx<'a> {
    args: &'a ReceiveArgs,
    api: &'a Npcap,
    handle: *mut c_void,
    session: AuSession,
    reassembler: &'a mut EncodedReassembler,
    transport: &'a mut Option<BufferedLog>,
    shared: &'a SharedDecoder,
    recycle_rx: &'a mpsc::Receiver<Vec<u8>>,
    need_transport_hash: bool,
    counters: &'a mut EncodedCounters,
    vstat: &'a mut VstatReader,
    deadline: Option<Instant>,
    discontinuity: bool,
    rate_window: RateWindow,
    live_rseg: TimedWindow,
    live_inter_arrival: TimedWindow,
    prev_completed_ns: u64,
    last_progress: Instant,
    started: Instant,
    pkt_tick: u32,
    stop_loop: bool,
    error: Option<anyhow::Error>,
}

impl EncodedDispatchCtx<'_> {
    fn break_now(&mut self) {
        self.stop_loop = true;
        unsafe { (self.api.breakloop)(self.handle) };
    }

    fn drain_recycled(&mut self) {
        while let Ok(buf) = self.recycle_rx.try_recv() {
            self.reassembler.recycle(buf);
        }
    }

    fn on_packet(&mut self, header: *const PcapHeader, data: *const c_uchar) -> anyhow::Result<()> {
        if self.stop_loop {
            return Ok(());
        }
        if self.pkt_tick & 15 == 0 {
            if stopped(self.args.stop.as_deref()) {
                self.counters.cancelled = true;
                self.break_now();
                return Ok(());
            }
            if self.shared.failed() {
                bail!(
                    "decoder worker failed: {}",
                    self.shared
                        .error()
                        .unwrap_or_else(|| "unknown decoder worker failure".into())
                );
            }
            if self.args.operation.timed_out() {
                bail!("{}", operation_timeout_message(&self.args.operation));
            }
            if self.args.expect_frames > 0 && self.shared.decoded() >= self.args.expect_frames {
                self.break_now();
                return Ok(());
            }
        }
        if self.pkt_tick & 63 == 0 {
            let now = Instant::now();
            if self.deadline.is_some_and(|deadline| now >= deadline) {
                self.counters.timed_out = true;
                self.break_now();
                return Ok(());
            }
            if now.duration_since(self.last_progress) >= PROGRESS_INTERVAL {
                live_encoded_progress(
                    self.args,
                    self.reassembler,
                    self.shared,
                    self.counters,
                    self.vstat,
                    self.started,
                    &mut self.rate_window,
                    &mut self.live_rseg,
                    &mut self.live_inter_arrival,
                );
                self.last_progress = now;
            }
        }
        self.pkt_tick = self.pkt_tick.wrapping_add(1);
        if header.is_null() || data.is_null() {
            return Ok(());
        }
        let caplen = unsafe { (*header).caplen as usize };
        if !(14 + 28..=MAX_CAPTURE).contains(&caplen) {
            return Ok(());
        }
        let packet = unsafe { std::slice::from_raw_parts(data, caplen) };
        let PushOutcome::Completed(completed) = self
            .reassembler
            .push_ethernet_timed_lazy(packet, qpc_ns, qpc_ns)
        else {
            return Ok(());
        };
        self.live_rseg.push(
            completed.completed_ns,
            completed
                .completed_ns
                .saturating_sub(completed.first_packet_ns),
        );
        if self.prev_completed_ns > 0 {
            self.live_inter_arrival.push(
                completed.completed_ns,
                completed
                    .completed_ns
                    .saturating_sub(self.prev_completed_ns),
            );
        }
        self.prev_completed_ns = completed.completed_ns;
        let rx_stats = self.reassembler.stats();
        self.rate_window.maybe_record(
            completed.completed_ns,
            self.shared.decoded(),
            rx_stats.packets_recv,
            rx_stats.bytes_recv,
        );
        self.pkt_tick = 0;
        self.drain_recycled();
        let transport_hash = if self.need_transport_hash {
            completed.hash()
        } else {
            0
        };
        match parse_au(
            &completed.data,
            self.session,
            completed.frame_id,
            self.args.codec.max_au_bytes,
        ) {
            Ok(unit) => {
                let eos = unit.is_eos();
                if self.counters.received_eos {
                    self.counters.invalid_au = self.counters.invalid_au.saturating_add(1);
                    self.discontinuity = true;
                    if self.counters.invalid_au <= 5 || self.counters.invalid_au.is_multiple_of(100)
                    {
                        eprintln!(
                            "[vc-stream] receiver rejected AU frame={} after EOS",
                            completed.frame_id
                        );
                    }
                } else {
                    if !eos {
                        self.counters.valid_units = self.counters.valid_units.saturating_add(1);
                        if let Some(writer) = self.transport.as_mut() {
                            writer.write_line(|w| {
                                writeln!(
                                    w,
                                    "{{\"frame_id\":{},\"encoded_bytes\":{},\"keyframe\":true,\"transport_hash\":\"{transport_hash:016x}\"}}",
                                    completed.frame_id,
                                    completed.data.len()
                                )
                            })?;
                        }
                    } else {
                        self.counters.received_eos = true;
                    }
                    let work = Work {
                        au: completed.data,
                        frame_id: completed.frame_id,
                        first_ns: completed.first_packet_ns,
                        queued_ns: qpc_ns(),
                        transport_hash,
                        key_frame: true,
                        discontinuity: self.discontinuity,
                        eos,
                    };
                    let mut queue = lock(&self.shared.queue);
                    let wait_started = qpc_ns();
                    let mut waited = false;
                    while queue.jobs.len() >= QUEUE_LIMIT && !queue.stopping {
                        if self.shared.failed() {
                            bail!(
                                "decoder worker failed: {}",
                                self.shared
                                    .error()
                                    .unwrap_or_else(|| "unknown decoder worker failure".into())
                            );
                        }
                        if stopped(self.args.stop.as_deref()) {
                            self.counters.cancelled = true;
                            self.break_now();
                            return Ok(());
                        }
                        if self.args.operation.timed_out() {
                            bail!("{}", operation_timeout_message(&self.args.operation));
                        }
                        if self
                            .deadline
                            .is_some_and(|deadline| Instant::now() >= deadline)
                        {
                            self.counters.timed_out = true;
                            self.break_now();
                            return Ok(());
                        }
                        if !waited {
                            self.shared
                                .queue_backpressure_events
                                .fetch_add(1, Ordering::AcqRel);
                            waited = true;
                        }
                        let (next_queue, _) = self
                            .shared
                            .cv
                            .wait_timeout(queue, Duration::from_millis(1))
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        queue = next_queue;
                    }
                    if waited {
                        let wait_ns = qpc_ns().saturating_sub(wait_started);
                        self.shared
                            .queue_backpressure_ns
                            .fetch_add(wait_ns, Ordering::AcqRel);
                        self.shared
                            .queue_backpressure_max_ns
                            .fetch_max(wait_ns, Ordering::AcqRel);
                    }
                    if queue.stopping {
                        self.break_now();
                        return Ok(());
                    }
                    queue.jobs.push_back(work);
                    drop(queue);
                    self.discontinuity = false;
                    self.shared.cv.notify_one();
                }
            }
            Err(error) => {
                self.counters.invalid_au = self.counters.invalid_au.saturating_add(1);
                self.discontinuity = true;
                if self.counters.invalid_au <= 5 || self.counters.invalid_au.is_multiple_of(100) {
                    eprintln!(
                        "[vc-stream] receiver rejected AU frame={}: {error}",
                        completed.frame_id
                    );
                }
            }
        }
        Ok(())
    }
}

unsafe extern "C" fn encoded_dispatch_cb(
    user: *mut c_uchar,
    header: *const PcapHeader,
    data: *const c_uchar,
) {
    let ctx = unsafe { &mut *user.cast::<EncodedDispatchCtx<'_>>() };
    if ctx.stop_loop {
        return;
    }
    match catch_unwind(AssertUnwindSafe(|| ctx.on_packet(header, data))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            ctx.error = Some(error);
            ctx.break_now();
        }
        Err(panic) => {
            ctx.error = Some(anyhow::anyhow!(
                "encoded dispatch callback panicked: {}",
                panic_text(panic.as_ref())
            ));
            ctx.break_now();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn encoded_capture_loop(
    args: &ReceiveArgs,
    api: &Npcap,
    handle: *mut c_void,
    session: AuSession,
    reassembler: &mut EncodedReassembler,
    transport: &mut Option<BufferedLog>,
    shared: &SharedDecoder,
    recycle_rx: &mpsc::Receiver<Vec<u8>>,
    need_transport_hash: bool,
    counters: &mut EncodedCounters,
    vstat: &mut VstatReader,
    deadline: Option<Instant>,
) -> anyhow::Result<()> {
    let mut ctx = EncodedDispatchCtx {
        args,
        api,
        handle,
        session,
        reassembler,
        transport,
        shared,
        recycle_rx,
        need_transport_hash,
        counters,
        vstat,
        deadline,
        discontinuity: false,
        rate_window: RateWindow::new(qpc_ns()),
        live_rseg: TimedWindow::default(),
        live_inter_arrival: TimedWindow::default(),
        prev_completed_ns: 0,
        last_progress: Instant::now(),
        started: Instant::now(),
        pkt_tick: 0,
        stop_loop: false,
        error: None,
    };
    loop {
        if stopped(ctx.args.stop.as_deref()) {
            ctx.counters.cancelled = true;
            break;
        }
        if ctx.shared.failed() {
            bail!(
                "decoder worker failed: {}",
                ctx.shared
                    .error()
                    .unwrap_or_else(|| "unknown decoder worker failure".into())
            );
        }
        if ctx.args.operation.timed_out() {
            bail!("{}", operation_timeout_message(&ctx.args.operation));
        }
        if ctx.args.expect_frames > 0 && ctx.shared.decoded() >= ctx.args.expect_frames {
            break;
        }
        let now = Instant::now();
        if ctx.deadline.is_some_and(|deadline| now >= deadline) {
            ctx.counters.timed_out = true;
            break;
        }
        // 空闲超时或无关/畸形包也不能冻结实际帧率；最近 1 秒滑窗会在空闲后归零。
        if now.duration_since(ctx.last_progress) >= PROGRESS_INTERVAL {
            live_encoded_progress(
                ctx.args,
                ctx.reassembler,
                ctx.shared,
                ctx.counters,
                ctx.vstat,
                ctx.started,
                &mut ctx.rate_window,
                &mut ctx.live_rseg,
                &mut ctx.live_inter_arrival,
            );
            ctx.last_progress = now;
        }
        let rc = unsafe {
            (ctx.api.dispatch)(
                ctx.handle,
                256,
                encoded_dispatch_cb,
                (&raw mut ctx).cast::<c_uchar>(),
            )
        };
        if let Some(error) = ctx.error.take() {
            return Err(error);
        }
        if ctx.stop_loop {
            break;
        }
        if rc == -1 {
            return Err(ctx
                .api
                .handle_error(ctx.handle, &format!("pcap_dispatch returned {rc}")));
        }
        if rc == -2 {
            break;
        }
        if rc == 0 {
            ctx.pkt_tick = 0;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn live_encoded_progress(
    args: &ReceiveArgs,
    reassembler: &EncodedReassembler,
    shared: &SharedDecoder,
    counters: &EncodedCounters,
    vstat: &mut VstatReader,
    started: Instant,
    rate_window: &mut RateWindow,
    live_rseg: &mut TimedWindow,
    live_inter_arrival: &mut TimedWindow,
) {
    let now_ns = qpc_ns();
    let (vcam_delivered, vseg_p99_ms, vseg) = vstat.live_fields(now_ns);
    let Some(progress) = args.progress.as_deref() else {
        return;
    };
    let stats = reassembler.stats();
    let uptime = started.elapsed().as_secs_f64().max(0.000_001);
    let decoded = shared.decoded();
    let rate = rate_window.sample(now_ns, decoded, stats.packets_recv, stats.bytes_recv);
    let rseg = live_rseg.summary_at(now_ns);
    let inter_arrival = live_inter_arrival.summary_at(now_ns);
    progress.update(&json!({
        "running":true,"complete":decoded,"compressed_complete":counters.valid_units,
        "transport_frames":counters.valid_units,"hash_rows":decoded,
        "dropped":stats.dropped,"packets_recv":stats.packets_recv,
        "packets_lost":stats.packets_lost,
        "transport_errors":counters.invalid_au.saturating_add(stats.packets_lost),
        "crc_errors":stats.crc_errors,"duplicate":stats.duplicate,"gaps":stats.gaps,
        "decode_errors":shared.decode_errors.load(Ordering::Acquire),
        "resets":shared.resets.load(Ordering::Acquire),
        "discarded":shared.discarded.load(Ordering::Acquire),
        "queue_drops":shared.queue_drops.load(Ordering::Acquire),
        "decode_queue_backpressure_events":shared.queue_backpressure_events.load(Ordering::Acquire),
        "decode_queue_backpressure_ms":shared.queue_backpressure_ns.load(Ordering::Acquire) as f64/1e6,
        "decode_queue_backpressure_max_ms":shared.queue_backpressure_max_ns.load(Ordering::Acquire) as f64/1e6,
        "decode_pending":shared.pending.load(Ordering::Acquire),
        "decode_in_flight_limit":args.codec.decode_in_flight,
        "waiting_idr":shared.waiting_idr.load(Ordering::Acquire),
        "shm_published":shared.shm_published.load(Ordering::Acquire),
        "vcam_delivered":vcam_delivered,
        "vseg_p99_ms":vseg_p99_ms,
        "vseg":vseg,
        "bytes_captured":stats.bytes_recv,
        "pps":rate.pps,
        "bps":rate.bps,
        "fps_actual":rate.fps,"uptime_s":uptime,
        "rseg":rseg,
        "inter_arrival":inter_arrival,
        "last_frame_id":shared.last_frame_id_plus_one
            .load(Ordering::Acquire).checked_sub(1).map(|value|value as u32),
    }));
}

// ---------------------------------------------------------------------------
// 解码 worker：创建、有界排空、重置/重建、哈希、SHM。
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn worker_main(
    codec: CodecConfig,
    geometry: Geometry,
    fps: u32,
    shm: Option<String>,
    hash_file: Option<File>,
    hashlog: Option<PathBuf>,
    operation: OperationState,
    shared: Arc<SharedDecoder>,
    init: mpsc::Sender<WorkerInit>,
    recycle_tx: mpsc::Sender<Vec<u8>>,
) -> anyhow::Result<()> {
    let _priority = ThreadPriority::capture();
    let mut log = match take_log_writer(hash_file, hashlog.as_deref()) {
        Ok(log) => log,
        Err(error) => {
            return Err(init_failure(
                &init,
                &shared,
                error.context("open decoded hashlog"),
            ));
        }
    };
    let mut publisher = match shm.as_deref() {
        Some(name) => match Publisher::new(name, geometry, fps) {
            Ok(publisher) => Some(publisher),
            Err(error) => {
                return Err(init_failure(
                    &init,
                    &shared,
                    anyhow::Error::new(error)
                        .context(format!("create requested SHM v3 mapping {name}")),
                ));
            }
        },
        None => None,
    };
    let decoder = match Decoder::open_observed(
        &codec,
        geometry.width(),
        geometry.height(),
        fps,
        &operation,
    ) {
        Ok(decoder) => decoder,
        Err(error) => {
            return Err(init_failure(&init, &shared, error.context("open decoder")));
        }
    };
    let settings = decoder.settings();
    let shm_enabled = publisher.is_some();
    if init
        .send(WorkerInit::Ready {
            settings,
            shm_enabled,
        })
        .is_err()
    {
        // 所属接收端已消失，不再有就绪观察者。
        return Ok(());
    }
    drop(init);
    let mut decoder = Some(decoder);
    let loop_result = worker_loop(
        &codec,
        geometry,
        fps,
        &operation,
        &shared,
        &recycle_tx,
        &mut decoder,
        &mut publisher,
        &mut log,
    );
    let flush = match log.as_mut() {
        Some(writer) => writer
            .flush()
            .map_err(|error| error.context("flush decoded hashlog")),
        None => Ok(()),
    };
    match (loop_result, flush) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(flush)) => Err(error.context(format!("flush decoded hashlog: {flush:#}"))),
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    codec: &CodecConfig,
    geometry: Geometry,
    fps: u32,
    operation: &OperationState,
    shared: &Arc<SharedDecoder>,
    recycle_tx: &mpsc::Sender<Vec<u8>>,
    decoder: &mut Option<Decoder>,
    publisher: &mut Option<Publisher>,
    log: &mut Option<BufferedLog>,
) -> anyhow::Result<()> {
    let frame_bytes = geometry.frame_bytes();
    let mut decoded = vec![0u8; frame_bytes];
    let mut have_last = false;
    let mut last_frame_id = 0u32;
    while let Some(work) = next_work(shared) {
        if work.discontinuity || (have_last && work.frame_id != last_frame_id.wrapping_add(1)) {
            if let Err(error) = decoder
                .as_mut()
                .context("decoder disappeared before reset")?
                .reset()
            {
                let _ = recycle_tx.send(work.au);
                return Err(error).context("reset decoder after stream discontinuity");
            }
            shared.resets.fetch_add(1, Ordering::AcqRel);
            // 每个编码 AU 都独立，但下面这个标志保留历史 C++ 接收端显式的
            // “等待关键帧/随机访问帧”可见状态。
            shared.waiting_idr.store(true, Ordering::Release);
        }
        have_last = true;
        last_frame_id = work.frame_id;
        if work.eos {
            let _ = recycle_tx.send(work.au);
            // EOS 不携带画面。本 worker 同步解码，不存在待处理的解码器输出，
            // 因此接受 EOS 本身就已算“排空”。
            ensure!(
                shared.pending.load(Ordering::Acquire) == 0,
                "EOS arrived while a decoder frame was still pending"
            );
            continue;
        }
        if shared.waiting_idr.load(Ordering::Acquire) && !work.key_frame {
            let _ = recycle_tx.send(work.au);
            shared.discarded.fetch_add(1, Ordering::AcqRel);
            continue;
        }
        shared.waiting_idr.store(false, Ordering::Release);
        shared.pending.store(1, Ordering::Release);
        let pending = PendingGuard(&shared.pending);
        let decode_started_ns = qpc_ns();
        let au_len = work.au.len();
        let payload = work
            .au
            .get(vc_media::AU_HEADER_BYTES..)
            .context("encoded AU shorter than 48B header")?;
        let result = decoder
            .as_mut()
            .context("decoder disappeared before decode")?
            .decode_verified_payload_into(payload, &mut decoded);
        let _ = recycle_tx.send(work.au);
        let now = qpc_ns();
        if let Err(error) = result {
            let count = shared.decode_errors.fetch_add(1, Ordering::AcqRel) + 1;
            if count <= 5 || count.is_multiple_of(100) {
                eprintln!(
                    "[vc-stream] decode failure frame={}: {error:#}",
                    work.frame_id
                );
            }
            drop(pending);
            recover_decoder(decoder, codec, geometry, fps, operation)
                .context("recreate decoder after decode failure")?;
            shared.resets.fetch_add(1, Ordering::AcqRel);
            shared.waiting_idr.store(true, Ordering::Release);
            continue;
        }
        // 先发布 SHM 并唤醒虚拟卡，再在关键路径之外计算 FNV-1a64 与写入日志。
        let token = if let Some(shm) = publisher.as_mut() {
            Some(shm.publish(work.frame_id, work.first_ns, &decoded, qpc_ns)?)
        } else {
            None
        };
        let need_hash = token.is_some() || log.is_some();
        let hash = if need_hash { fnv1a64(&decoded) } else { 0 };
        if let (Some(shm), Some(token)) = (publisher.as_mut(), token) {
            shm.mark_hash(token, hash);
            shared.shm_published.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(writer) = log.as_mut() {
            let decode_pipeline_ms = now.saturating_sub(work.first_ns) as f64 / 1e6;
            let queue_wait_ms = decode_started_ns.saturating_sub(work.queued_ns) as f64 / 1e6;
            let decode_ms = now.saturating_sub(decode_started_ns) as f64 / 1e6;
            let transport_hash = work.transport_hash;
            writer.write_line(|w| {
                writeln!(
                    w,
                    "{{\"frame_id\":{},\"decoded_hash\":\"{hash:016x}\",\"transport_hash\":\"{transport_hash:016x}\",\"qpc_ns\":{now},\"decode_pipeline_ms\":{decode_pipeline_ms},\"queued_qpc_ns\":{},\"decode_started_qpc_ns\":{decode_started_ns},\"queue_wait_ms\":{queue_wait_ms},\"decode_ms\":{decode_ms},\"encoded_bytes\":{au_len}}}",
                    work.frame_id,
                    work.queued_ns,
                )
            })?;
        }
        let count = shared.decoded.fetch_add(1, Ordering::AcqRel) + 1;
        if count == 1 {
            shared.first_completed_ns.store(now, Ordering::Release);
        }
        shared.last_frame_id_plus_one.store(
            u64::from(work.frame_id).saturating_add(1),
            Ordering::Release,
        );
        shared.last_completed_ns.store(now, Ordering::Release);
        shared.waiting_idr.store(false, Ordering::Release);
        drop(pending);
    }
    Ok(())
}

fn next_work(shared: &SharedDecoder) -> Option<Work> {
    let mut queue = lock(&shared.queue);
    loop {
        if let Some(work) = queue.jobs.pop_front() {
            shared.cv.notify_one();
            return Some(work);
        }
        if queue.stopping {
            return None;
        }
        queue = shared
            .cv
            .wait(queue)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

fn recover_decoder(
    slot: &mut Option<Decoder>,
    codec: &CodecConfig,
    geometry: Geometry,
    fps: u32,
    operation: &OperationState,
) -> anyhow::Result<()> {
    // 先销毁失败的解码器，再构造其替代者。PyroWave 的 reset 保留 Vulkan
    // 设备；重建相当于历史 C++ 的 `reopen(true)` 路径。
    *slot = None;
    let decoder =
        Decoder::open_observed(codec, geometry.width(), geometry.height(), fps, operation)?;
    *slot = Some(decoder);
    Ok(())
}

// ---------------------------------------------------------------------------
// 证据与错误辅助函数。
// ---------------------------------------------------------------------------

/// 把 `extra` 的字段合并进 `target`，保留无关的实时指标。
fn add_json_fields(target: &mut Value, extra: Value) {
    let (Some(target), Value::Object(extra)) = (target.as_object_mut(), extra) else {
        return;
    };
    target.extend(extra);
}

fn finish_receiver(
    progress: &Progress,
    stats_path: Option<PathBuf>,
    stats_file: Option<File>,
    value: &Value,
    outcome: anyhow::Result<()>,
) -> anyhow::Result<()> {
    progress.update(value);
    let evidence = write_stats_file(stats_path.as_deref(), stats_file, value);
    match (outcome, evidence) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(evidence)) => Err(evidence.context("write receiver stats")),
        (Err(error), Err(evidence)) => {
            Err(error.context(format!("write receiver stats: {evidence:#}")))
        }
    }
}

fn fail_early(
    mut args: ReceiveArgs,
    progress: &Progress,
    adapter: &str,
    error: anyhow::Error,
) -> anyhow::Result<()> {
    let text = format!("{error:#}");
    let value = json!({
        "ready":false,"running":false,"role":"receiver","session_id":args.common.session,
        "adapter":adapter,"width":args.common.width,"height":args.common.height,
        "fps":args.common.fps,"fps_limit":args.common.fps,"mtu":args.common.mtu,
        "ethertype":format!("0x{:04x}",args.common.ethertype),"codec":args.codec.codec,
        "geometry":{"width":args.common.width,"height":args.common.height,"fps":args.common.fps},
        "batch":null,"failed":true,"error":text,
        "decode_errors":0,"decode_resets":0,"decode_discarded":0,
        "decode_queue_drops":0,"decode_pending":0,
        "expect_frames":args.expect_frames,"timeout_s":args.timeout_s,
    });
    eprintln!("[vc-stream] receiver failed: {text}");
    finish_receiver(
        progress,
        args.stats.take(),
        args.stats_file.take(),
        &value,
        Err(error),
    )
}

fn merge_error(outcome: anyhow::Result<()>, error: anyhow::Error) -> anyhow::Result<()> {
    match outcome {
        Ok(()) => Err(error),
        Err(primary) => Err(primary.context(format!("{error:#}"))),
    }
}

fn combine_worker_error(
    outcome: anyhow::Result<()>,
    worker_error: Option<String>,
) -> anyhow::Result<()> {
    let Some(worker_error) = worker_error else {
        return outcome;
    };
    match outcome {
        Ok(()) => Err(anyhow::anyhow!("{worker_error}")),
        Err(primary) => {
            let text = format!("{primary:#}");
            if text == worker_error || text.contains(&worker_error) {
                Err(primary)
            } else {
                Err(primary.context(format!("decoder worker: {worker_error}")))
            }
        }
    }
}

fn init_failure(
    init: &mpsc::Sender<WorkerInit>,
    shared: &SharedDecoder,
    error: anyhow::Error,
) -> anyhow::Error {
    let text = format!("{error:#}");
    shared.fail(text.clone());
    // 在调用方返回并丢弃部分创建的 Publisher / Decoder 之前发送，使等待就绪
    // 的观察者能看到创建失败的原因。
    let _ = init.send(WorkerInit::Failed(text));
    error
}

fn operation_timeout_message(operation: &OperationState) -> String {
    format!(
        "PyroWave {}操作超过5秒；保留活动worker与DLL等待清理，未终止宿主",
        operation.timed_out_operation().unwrap_or("原生")
    )
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic payload".into()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
