//! 共享的进程内发送端。物理网卡以一次 queue 提交完整帧。
use super::*;
use std::sync::{Condvar, mpsc};

const TX_AUDIT_QUEUE_CAP: usize = 8;
const CAPTURE_READY_LIMIT: usize = 2;
const CAPTURE_POOL_BUFFERS: usize = 3;

pub(crate) fn send(mut args: SendArgs) -> anyhow::Result<()> {
    if args.progress.is_none() {
        args.source.pattern = args.pattern.name().to_owned();
    }
    // 在进入任何原生库之前先预约证据文件。只重写本次拥有的文件；早前会话的
    // stats/hashlog 绝不会被截断。
    if args.stats_file.is_none() {
        args.stats_file = take_writer(None, args.stats.as_deref())?
            .map(|writer| writer.into_inner().map_err(|error| error.into_error()))
            .transpose()?;
    }
    let progress = args
        .progress
        .get_or_insert_with(|| {
            Arc::new(Progress::new(json!({
                "ready":false,"role":"sender","session_id":args.common.session,
                "adapter":args.common.adapter,"codec":args.codec.codec,
            })))
        })
        .clone();
    let result = run_sender(&mut args);
    let mut final_state = progress.snapshot();
    final_state["running"] = json!(false);
    final_state["failed"] = json!(result.is_err());
    final_state["exit_code"] = json!(i32::from(result.is_err()));
    if let Err(error) = &result {
        final_state["error"] = json!(format!("{error:#}"));
        final_state["send_errors"] = json!(1);
    }
    progress.update(&final_state);
    let evidence = write_stats_file(args.stats.as_deref(), args.stats_file.take(), &final_state);
    result.and(evidence)
}

struct TxAuditJob {
    frame_id: u32,
    raw: bool,
    is_pattern: bool,
    frame: Vec<u8>,
    source_frame: Option<Vec<u8>>,
    frame_start: u64,
    source_ready: u64,
    send_start_ns: u64,
    encode_ns: u64,
    encode_done: u64,
}

struct TxAuditWorker {
    tx: Option<mpsc::SyncSender<TxAuditJob>>,
    return_rx: mpsc::Receiver<Vec<u8>>,
    join: Option<thread::JoinHandle<anyhow::Result<(Samples, Samples)>>>,
}

impl TxAuditWorker {
    fn spawn(mut hashlog: BufferedLog, session: &str) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<TxAuditJob>(TX_AUDIT_QUEUE_CAP);
        let (return_tx, return_rx) = mpsc::channel::<Vec<u8>>();
        let join = thread::Builder::new()
            .name(format!("vc-stream-tx-audit-{session}"))
            .spawn(move || -> anyhow::Result<(Samples, Samples)> {
                let mut audit_timing = Samples::default();
                let mut log_timing = Samples::default();
                while let Ok(job) = rx.recv() {
                    let audit_started = qpc_ns();
                    let transport_hash = fnv1a64(&job.frame);
                    let source_hash = job
                        .source_frame
                        .as_deref()
                        .map(fnv1a64)
                        .unwrap_or(transport_hash);
                    audit_timing.push(qpc_ns().saturating_sub(audit_started));

                    let log_started = qpc_ns();
                    let frame_len = job.frame.len();
                    hashlog.write_line(|w| {
                        if job.raw {
                            writeln!(
                                w,
                                "{{\"frame_id\":{},\"hash\":\"{transport_hash:016x}\",\"qpc_ns\":{},\"source_ready_qpc_ns\":{},\"send_qpc_ns\":{}}}",
                                job.frame_id, job.frame_start, job.source_ready, job.send_start_ns
                            )
                        } else if job.is_pattern {
                            writeln!(
                                w,
                                "{{\"frame_id\":{},\"source_hash\":\"{source_hash:016x}\",\"transport_hash\":\"{transport_hash:016x}\",\"encoded_bytes\":{frame_len},\"keyframe\":true,\"fault_injected\":false,\"qpc_ns\":{},\"source_ready_qpc_ns\":{},\"send_qpc_ns\":{},\"encode_wait_ns\":{},\"prepare_lead_ns\":0,\"prepare_work_ns\":{}}}",
                                job.frame_id,
                                job.frame_start,
                                job.source_ready,
                                job.send_start_ns,
                                job.encode_ns,
                                job.encode_done.saturating_sub(job.frame_start)
                            )
                        } else {
                            writeln!(
                                w,
                                "{{\"frame_id\":{},\"source_hash\":\"{source_hash:016x}\",\"transport_hash\":\"{transport_hash:016x}\",\"encoded_bytes\":{frame_len},\"keyframe\":true,\"fault_injected\":false,\"qpc_ns\":{},\"source_ready_qpc_ns\":{},\"send_qpc_ns\":{},\"encode_wait_ns\":{},\"prepare_lead_ns\":0,\"prepare_work_ns\":null}}",
                                job.frame_id,
                                job.frame_start,
                                job.source_ready,
                                job.send_start_ns,
                                job.encode_ns
                            )
                        }
                    })?;
                    log_timing.push(qpc_ns().saturating_sub(log_started));

                    if job.raw {
                        let _ = return_tx.send(job.frame);
                    } else if let Some(src) = job.source_frame {
                        let _ = return_tx.send(src);
                    }
                }
                hashlog.flush()?;
                Ok((audit_timing, log_timing))
            })
            .context("spawn sender audit worker")?;
        Ok(Self {
            tx: Some(tx),
            return_rx,
            join: Some(join),
        })
    }

    fn submit(&self, job: TxAuditJob) -> anyhow::Result<()> {
        self.tx
            .as_ref()
            .context("sender audit channel closed")?
            .send(job)
            .map_err(|_| anyhow::anyhow!("sender audit worker exited unexpectedly"))
    }

    fn drain_recycled(&self, mut on_buf: impl FnMut(Vec<u8>)) {
        while let Ok(buf) = self.return_rx.try_recv() {
            on_buf(buf);
        }
    }

    fn finish(mut self) -> anyhow::Result<(Samples, Samples)> {
        self.tx.take();
        let Some(join) = self.join.take() else {
            return Ok((Samples::default(), Samples::default()));
        };
        match join.join() {
            Ok(result) => result,
            Err(_) => bail!("sender audit worker panicked"),
        }
    }
}

impl Drop for TxAuditWorker {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct CapturedFrame {
    buf: Vec<u8>,
    frame_start: u64,
    source_ready: u64,
}

struct CaptureShared {
    ready: VecDeque<CapturedFrame>,
    free_bufs: Vec<Vec<u8>>,
    source_info: Option<Value>,
    started: bool,
    finished: bool,
    error: Option<String>,
}

struct DesktopCapturePipeline {
    state: Arc<(Mutex<CaptureShared>, Condvar)>,
    cap_stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl DesktopCapturePipeline {
    fn spawn(
        source_cfg: SourceConfig,
        width: u32,
        height: u32,
        fps: u32,
        max_frames: u64,
        session: &str,
        outer_stop: Option<Arc<AtomicBool>>,
    ) -> anyhow::Result<(Self, Value)> {
        let frame_bytes = Geometry::new(width, height)?.frame_bytes();
        let mut free_bufs = Vec::with_capacity(CAPTURE_POOL_BUFFERS);
        for _ in 0..CAPTURE_POOL_BUFFERS {
            free_bufs.push(vec![0u8; frame_bytes]);
        }
        let state = Arc::new((
            Mutex::new(CaptureShared {
                ready: VecDeque::with_capacity(CAPTURE_READY_LIMIT),
                free_bufs,
                source_info: None,
                started: false,
                finished: false,
                error: None,
            }),
            Condvar::new(),
        ));
        let cap_stop = Arc::new(AtomicBool::new(false));
        let (init_tx, init_rx) = mpsc::sync_channel::<anyhow::Result<Value>>(1);

        let worker_state = state.clone();
        let worker_cap_stop = cap_stop.clone();
        let worker_outer_stop = outer_stop;
        let join = thread::Builder::new()
            .name(format!("vc-stream-cap-{session}"))
            .spawn(move || {
                let _priority = ThreadPriority::capture();
                let mut source = match DesktopSource::open(&source_cfg, width, height) {
                    Ok(source) => source,
                    Err(error) => {
                        let _ = init_tx.send(Err(error));
                        return;
                    }
                };
                let initial_info = source.stats();
                {
                    let (lock, cv) = &*worker_state;
                    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                    guard.source_info = Some(initial_info.clone());
                    cv.notify_all();
                }
                if init_tx.send(Ok(initial_info)).is_err() {
                    return;
                }

                // 等待发送端发布 ready 并发出启动信号，避免在初始化窗口内提前消费帧。
                let is_stopped = || {
                    worker_cap_stop.load(Ordering::Acquire)
                        || worker_outer_stop
                            .as_deref()
                            .is_some_and(|s| s.load(Ordering::Acquire))
                };
                {
                    let (lock, cv) = &*worker_state;
                    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while !guard.started && !guard.finished && !is_stopped() {
                        let (next, _) = cv
                            .wait_timeout(guard, Duration::from_millis(5))
                            .unwrap_or_else(|e| e.into_inner());
                        guard = next;
                    }
                    if !guard.started || is_stopped() {
                        guard.source_info = Some(source.stats());
                        guard.finished = true;
                        cv.notify_all();
                        return;
                    }
                }

                let mut pacer = FramePacer::new(fps, qpc_ns());
                let mut captured_count = 0u64;
                let mut last_info_at = Instant::now();
                loop {
                    if max_frames > 0 && captured_count == max_frames {
                        break;
                    }
                    if !loop_sleep_either(
                        pacer.next_ns,
                        &worker_cap_stop,
                        worker_outer_stop.as_deref(),
                    ) {
                        break;
                    }
                    let frame_start = qpc_ns();
                    pacer.advance(frame_start);

                    // 从空闲池取得缓冲区；若双缓冲已满则短暂等待 TX 取走，保持有界内存。
                    let mut buf = {
                        let (lock, cv) = &*worker_state;
                        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(buf) = guard.free_bufs.pop() {
                                break Some(buf);
                            }
                            if guard.ready.len() < CAPTURE_READY_LIMIT {
                                break Some(vec![0u8; frame_bytes]);
                            }
                            if guard.finished || is_stopped() {
                                break None;
                            }
                            let (next, _) = cv
                                .wait_timeout(guard, Duration::from_millis(5))
                                .unwrap_or_else(|e| e.into_inner());
                            guard = next;
                        }
                    };
                    let Some(mut buf) = buf.take() else {
                        break;
                    };

                    let mut got_frame = false;
                    while !is_stopped() {
                        let stop_ref = if worker_cap_stop.load(Ordering::Acquire) {
                            &worker_cap_stop
                        } else {
                            worker_outer_stop.as_deref().unwrap_or(&worker_cap_stop)
                        };
                        match source.capture_cancellable(&mut buf, Some(stop_ref)) {
                            Ok(true) => {
                                got_frame = true;
                                break;
                            }
                            Ok(false) => {
                                if last_info_at.elapsed() >= Duration::from_millis(250) {
                                    let info = source.stats();
                                    let (lock, _) = &*worker_state;
                                    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                                    guard.source_info = Some(info);
                                    last_info_at = Instant::now();
                                }
                                thread::yield_now();
                            }
                            Err(error) => {
                                let info = source.stats();
                                let (lock, cv) = &*worker_state;
                                let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                                guard.source_info = Some(info);
                                guard.error = Some(format!("{error:#}"));
                                guard.finished = true;
                                cv.notify_all();
                                return;
                            }
                        }
                    }
                    if !got_frame {
                        let (lock, cv) = &*worker_state;
                        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                        if guard.free_bufs.len() < CAPTURE_POOL_BUFFERS + 2 {
                            guard.free_bufs.push(buf);
                        }
                        cv.notify_all();
                        break;
                    }
                    let source_ready = qpc_ns();
                    captured_count += 1;
                    let maybe_info =
                        (last_info_at.elapsed() >= Duration::from_millis(250)).then(|| {
                            last_info_at = Instant::now();
                            source.stats()
                        });
                    {
                        let (lock, cv) = &*worker_state;
                        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(info) = maybe_info {
                            guard.source_info = Some(info);
                        }
                        guard.ready.push_back(CapturedFrame {
                            buf,
                            frame_start,
                            source_ready,
                        });
                        cv.notify_all();
                    }
                }

                let final_info = source.stats();
                let (lock, cv) = &*worker_state;
                let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                guard.source_info = Some(final_info);
                guard.finished = true;
                cv.notify_all();
            })
            .context("spawn desktop capture pipeline thread")?;

        match init_rx.recv() {
            Ok(Ok(initial_info)) => Ok((
                Self {
                    state,
                    cap_stop,
                    join: Some(join),
                },
                initial_info,
            )),
            Ok(Err(error)) => {
                cap_stop.store(true, Ordering::Release);
                let _ = join.join();
                Err(error)
            }
            Err(_) => {
                cap_stop.store(true, Ordering::Release);
                let _ = join.join();
                bail!("desktop capture thread ended during initialization")
            }
        }
    }

    fn start(&self) {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.started = true;
        cv.notify_all();
    }

    fn source_info(&self) -> Option<Value> {
        let (lock, _) = &*self.state;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .source_info
            .clone()
    }

    fn recycle(&self, buf: Vec<u8>) {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if guard.free_bufs.len() < CAPTURE_POOL_BUFFERS + 2 {
            guard.free_bufs.push(buf);
            cv.notify_all();
        }
    }

    fn next_frame(
        &self,
        stop: Option<&AtomicBool>,
        mut on_idle: impl FnMut(Option<&Value>),
    ) -> anyhow::Result<Option<CapturedFrame>> {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut last_tick = Instant::now();
        loop {
            if let Some(frame) = guard.ready.pop_front() {
                cv.notify_all();
                return Ok(Some(frame));
            }
            if let Some(error) = guard.error.as_deref() {
                bail!("{error}");
            }
            if guard.finished || stopped(stop) {
                return Ok(None);
            }
            let (next, _) = cv
                .wait_timeout(guard, Duration::from_millis(5))
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
            if last_tick.elapsed() >= Duration::from_millis(250) {
                let info = guard.source_info.clone();
                drop(guard);
                on_idle(info.as_ref());
                last_tick = Instant::now();
                guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            }
        }
    }

    fn shutdown(&mut self) -> anyhow::Result<Option<Value>> {
        self.cap_stop.store(true, Ordering::Release);
        {
            let (lock, cv) = &*self.state;
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            guard.finished = true;
            cv.notify_all();
        }
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            bail!("desktop capture thread panicked");
        }
        let (lock, _) = &*self.state;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = guard.error.take() {
            bail!("{error}");
        }
        Ok(guard.source_info.clone())
    }
}

impl Drop for DesktopCapturePipeline {
    fn drop(&mut self) {
        self.cap_stop.store(true, Ordering::Release);
        {
            let (lock, cv) = &*self.state;
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            guard.finished = true;
            cv.notify_all();
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn loop_sleep_either(
    target_ns: u64,
    cap_stop: &AtomicBool,
    outer_stop: Option<&AtomicBool>,
) -> bool {
    loop {
        if cap_stop.load(Ordering::Acquire) || stopped(outer_stop) {
            return false;
        }
        let remaining = target_ns.saturating_sub(qpc_ns());
        if remaining == 0 {
            return true;
        }
        if remaining > 500_000 {
            scheduling::wait_for((remaining - 420_000).min(5_000_000));
        } else {
            std::hint::spin_loop();
        }
    }
}

fn run_sender(args: &mut SendArgs) -> anyhow::Result<()> {
    validate_sender(args)?;
    let _priority = ThreadPriority::capture();
    let geometry = geometry(&args.common)?;
    let frame_bytes = geometry.frame_bytes();
    let payload_max = payload_max_for_mtu(args.common.mtu)?;
    let api = Npcap::load()?;
    let adapter = api.select_adapter(&args.common.adapter)?;
    let loopback = adapter.eq_ignore_ascii_case(r"\Device\NPF_Loopback");
    let raw = args.codec.codec == "raw";
    let frame_packets = vc_transport::packet_count(frame_bytes, payload_max)? as usize;
    let batch = if raw {
        Some(
            args.batch
                .unwrap_or(if loopback { 1 } else { frame_packets }),
        )
    } else {
        None
    };
    if let Some(batch) = batch {
        ensure!(
            (1..=u16::MAX as usize).contains(&batch),
            "diagnostic batch must be in 1..=65535"
        );
        if !loopback {
            ensure!(
                batch == frame_packets,
                "physical NIC requires one dynamically sized frame batch ({frame_packets} packets); got {batch}"
            );
        }
        ensure!(
            batch == 1 || args.pacing_us == 0,
            "per-packet pacing cannot be combined with one sendqueue batch per frame"
        );
    }
    let handle = api.open(&adapter, 65536, 1)?;
    let _capture = Capture(handle, api.close);
    let mut desktop_pipeline = match args.source.mode.as_str() {
        "pattern" => None,
        "dda" | "wgc" => Some(DesktopCapturePipeline::spawn(
            args.source.clone(),
            args.common.width,
            args.common.height,
            args.common.fps,
            args.frames,
            &args.common.session,
            args.stop.clone(),
        )?),
        other => bail!("source mode {other} is not supported"),
    };
    let mut initial_source_info = desktop_pipeline.as_ref().map(|(_, info)| info.clone());
    let mut encoder = if raw {
        None
    } else {
        Some(Encoder::open_observed(
            &args.codec,
            args.common.width,
            args.common.height,
            args.common.fps,
            &args.operation,
        )?)
    };
    let encoder_settings = encoder.as_ref().map(Encoder::settings).unwrap_or_else(|| {
        json!({
            "family":"raw","identity":"raw","compression":false,"pixel_format":"nv12",
            "width":args.common.width,"height":args.common.height,"fps":args.common.fps,
        })
    });
    let hashlog = take_log_writer(args.hash_file.take(), args.hashlog.as_deref())?;
    let mut audit_worker = hashlog
        .map(|log| TxAuditWorker::spawn(log, &args.common.session))
        .transpose()?;
    let mut pattern_pool: Vec<Vec<u8>> = if desktop_pipeline.is_none() {
        (0..4).map(|_| vec![0u8; frame_bytes]).collect()
    } else {
        Vec::new()
    };
    let mut tx = FrameTransmitter::new(
        &api,
        handle,
        args,
        loopback,
        batch,
        frame_bytes,
        payload_max,
    )?;
    ensure!(
        !stopped(args.stop.as_deref()),
        "sender cancelled during initialization"
    );
    let is_pattern = desktop_pipeline.is_none();
    let ready = json!({
        "ready":true,"running":true,"role":"sender","session_id":args.common.session,
        "adapter":adapter,"width":args.common.width,"height":args.common.height,
        "fps":args.common.fps,"fps_target":args.common.fps,"mtu":args.common.mtu,
        "ethertype":format!("0x{:04x}",args.common.ethertype),"frame_bytes":frame_bytes,
        "packets_per_frame":if raw { Some(frame_packets) } else { None },
        "batch":batch,"batch_packets":batch,"batch_policy":if args.batch.is_some(){"diagnostic_override"}else if loopback{"loopback_packet"}else{"frame"},
        "pacing_us":args.pacing_us,"codec":args.codec.codec,"source":args.source.mode,
        "pattern":if is_pattern{Some(args.pattern.name())}else{None},
        "transform":args.source.transform,"source_info":initial_source_info,
        "encoder":encoder_settings,"encoder_configuration":encoder_settings,"npcap":api.version(),
        "sendqueue":if tx.queue.is_some(){"queue"}else{"packet"},
        "send_errors":0,"sent_frames":0,"frames":0,"sent_packets":0,
        "max_bitrate":if raw{None}else{Some(args.codec.max_bitrate_bps)},
        "max_au_bytes":if raw{None}else{Some(args.codec.max_au_bytes)},
        "gop":if raw{None}else{Some(1)},"encoder_in_flight":1,
        "cpu_scope":"whole_host_process; concurrent DLL sessions share this counter",
    });
    publish_progress(args.progress.as_deref(), args.ready.as_deref(), &ready)?;
    if let Some((pipeline, _)) = desktop_pipeline.as_ref() {
        pipeline.start();
    }
    let cpu_start = process_cpu_ns()?;
    let started = Instant::now();
    let start_qpc = qpc_ns();
    let mut pacer = FramePacer::new(args.common.fps, start_qpc);
    let mut limiter = (!raw).then(|| ByteWindow::new(args.codec.max_bitrate_bps));
    let period = 1_000_000_000u64.div_ceil(u64::from(args.common.fps));
    let mut previous_send = 0u64;
    let mut frames = 0u64;
    let mut packets = 0u64;
    let mut bytes = 0u64;
    let mut encoded_bytes = 0u64;
    let mut first_send = 0u64;
    let mut last_send = 0u64;
    let mut rate_wait_ns = 0u64;
    let mut encode_total_ns = 0u64;
    let mut generation_send_ns = 0u64;
    let mut eos_packets = 0u64;
    let mut pc1 = Samples::default();
    let mut source_timing = Samples::default();
    let mut encode_timing = Samples::default();
    let mut rate_timing = Samples::default();
    let mut transmit_timing = Samples::default();
    let mut capture_to_send = Samples::default();
    let mut rate_window = RateWindow::new(start_qpc);
    let mut live_pc1 = TimedWindow::default();
    let mut live_source = TimedWindow::default();
    let mut last_progress = Instant::now();
    loop {
        if args.frames > 0 && frames == args.frames {
            break;
        }
        let frame_id = frames as u32;
        if let Some(audit) = audit_worker.as_ref() {
            audit.drain_recycled(|buf| {
                if let Some((pipeline, _)) = desktop_pipeline.as_ref() {
                    pipeline.recycle(buf);
                } else if pattern_pool.len() < 8 {
                    pattern_pool.push(buf);
                }
            });
        }
        let (source_frame, frame_start, source_ready) =
            if let Some((pipeline, _)) = desktop_pipeline.as_ref() {
                let Some(captured) = pipeline.next_frame(args.stop.as_deref(), |info| {
                    if last_progress.elapsed() >= Duration::from_millis(250) {
                        live_sender_progress(
                            args.progress.as_deref(),
                            started,
                            frames,
                            packets,
                            bytes,
                            encoded_bytes,
                            info,
                            &mut rate_window,
                            &mut live_pc1,
                            &mut live_source,
                        );
                        last_progress = Instant::now();
                    }
                })?
                else {
                    break;
                };
                (captured.buf, captured.frame_start, captured.source_ready)
            } else {
                if !loop_sleep(pacer.next_ns, args.stop.as_deref()) {
                    break;
                }
                let frame_start = qpc_ns();
                pacer.advance(frame_start);
                let mut buf = pattern_pool.pop().unwrap_or_else(|| vec![0u8; frame_bytes]);
                args.pattern.fill(geometry, frame_id, &mut buf);
                let source_ready = qpc_ns();
                (buf, frame_start, source_ready)
            };
        if stopped(args.stop.as_deref()) {
            if let Some((pipeline, _)) = desktop_pipeline.as_ref() {
                pipeline.recycle(source_frame);
            } else if pattern_pool.len() < 8 {
                pattern_pool.push(source_frame);
            }
            break;
        }
        let encode_started = qpc_ns();
        // Raw 直接借用实际 NV12 像素：没有编解码拷贝，也没有 AU 上限。
        let encoded = if let Some(encoder) = encoder.as_mut() {
            Some(encoder.encode(&source_frame, frame_id)?)
        } else {
            None
        };
        let encode_done = qpc_ns();
        let frame = encoded.as_deref().unwrap_or(&source_frame);
        if !raw {
            parse_au(
                frame,
                au_session(
                    &args.codec.codec,
                    args.common.width,
                    args.common.height,
                    args.common.fps,
                )?,
                frame_id,
                args.codec.max_au_bytes,
            )?;
        }

        let earliest = if !raw && frames > 0 {
            previous_send.saturating_add(period)
        } else {
            0
        };
        let Some(sent) = tx.transmit(
            frame,
            frame_id,
            frame_start,
            if is_pattern { args.pattern.id() } else { 255 },
            earliest,
            limiter.as_mut(),
            args.stop.as_deref(),
        )?
        else {
            if let Some((pipeline, _)) = desktop_pipeline.as_ref() {
                pipeline.recycle(source_frame);
            } else if pattern_pool.len() < 8 {
                pattern_pool.push(source_frame);
            }
            break;
        };
        previous_send = sent.start_ns;
        packets += sent.packets as u64;
        bytes += sent.bytes;
        if !raw {
            encoded_bytes += frame.len() as u64;
        }
        if frames == 0 {
            first_send = sent.start_ns;
        }
        last_send = sent.end_ns;
        let encode_ns = encode_done.saturating_sub(encode_started);
        let source_ns = source_ready.saturating_sub(frame_start);
        let pc1_ns = sent.end_ns.saturating_sub(source_ready);
        encode_total_ns += encode_ns;
        rate_wait_ns += sent.wait_ns;
        generation_send_ns += sent.end_ns.saturating_sub(frame_start);
        source_timing.push(source_ns);
        encode_timing.push(encode_ns);
        rate_timing.push(sent.wait_ns);
        transmit_timing.push(sent.end_ns.saturating_sub(sent.start_ns));
        capture_to_send.push(sent.start_ns.saturating_sub(frame_start));
        pc1.push(pc1_ns);
        live_source.push(sent.end_ns, source_ns);
        live_pc1.push(sent.end_ns, pc1_ns);

        if let Some(audit) = audit_worker.as_ref() {
            let job = if let Some(encoded_buf) = encoded {
                TxAuditJob {
                    frame_id,
                    raw: false,
                    is_pattern,
                    frame: encoded_buf,
                    source_frame: Some(source_frame),
                    frame_start,
                    source_ready,
                    send_start_ns: sent.start_ns,
                    encode_ns,
                    encode_done,
                }
            } else {
                TxAuditJob {
                    frame_id,
                    raw: true,
                    is_pattern,
                    frame: source_frame,
                    source_frame: None,
                    frame_start,
                    source_ready,
                    send_start_ns: sent.start_ns,
                    encode_ns,
                    encode_done,
                }
            };
            audit.submit(job)?;
        } else if let Some((pipeline, _)) = desktop_pipeline.as_ref() {
            pipeline.recycle(source_frame);
        } else if pattern_pool.len() < 8 {
            pattern_pool.push(source_frame);
        }

        frames += 1;
        rate_window.maybe_record(sent.end_ns, frames, packets, bytes);
        if last_progress.elapsed() >= Duration::from_millis(250) {
            let info = desktop_pipeline
                .as_ref()
                .and_then(|(pipeline, _)| pipeline.source_info());
            live_sender_progress(
                args.progress.as_deref(),
                started,
                frames,
                packets,
                bytes,
                encoded_bytes,
                info.as_ref(),
                &mut rate_window,
                &mut live_pc1,
                &mut live_source,
            );
            last_progress = Instant::now();
        }
    }
    if !raw && args.frames > 0 && frames == args.frames && !stopped(args.stop.as_deref()) {
        let eos = vc_media::build_au(
            au_session(
                &args.codec.codec,
                args.common.width,
                args.common.height,
                args.common.fps,
            )?,
            frames as u32,
            &[],
        )?;
        if let Some(sent) = tx.transmit(
            &eos,
            frames as u32,
            qpc_ns(),
            255,
            0,
            None,
            args.stop.as_deref(),
        )? {
            eos_packets = sent.packets as u64;
            packets += eos_packets;
            bytes += sent.bytes;
        }
    }
    if let Some((mut pipeline, _)) = desktop_pipeline.take() {
        initial_source_info = pipeline.shutdown()?.or(initial_source_info);
    }
    let (audit_timing, log_timing) = match audit_worker.take() {
        Some(audit) => audit.finish()?,
        None => (Samples::default(), Samples::default()),
    };
    let elapsed = started.elapsed().as_secs_f64().max(0.000001);
    let segment = pc1.value();
    let final_value = json!({
        "frames":frames,"sent_frames":frames,"sent_packets":packets,"bytes":bytes,
        "send_errors":0,"pps":packets as f64/elapsed,"bps":bytes as f64*8.0/elapsed,
        "fps_actual":frames as f64/elapsed,"uptime_s":elapsed,
        "first_send_qpc_ns":first_send,"last_send_qpc_ns":last_send,
        "generation_send_avg_ms":generation_send_ns as f64/frames.max(1) as f64/1e6,
        "encode_avg_ms":encode_total_ns as f64/frames.max(1) as f64/1e6,
        "encoded_bytes":encoded_bytes,"keyframes":if raw{None}else{Some(frames)},
        "rate_wait_ms":rate_wait_ns as f64/1e6,"eos_sent":eos_packets>0,"eos_packets":eos_packets,
        "cpu_pct":(process_cpu_ns()?.saturating_sub(cpu_start) as f64/1e9)/elapsed*100.0,
        "source_info":initial_source_info,
        "source_skipped":initial_source_info.as_ref().and_then(|s|s["source_skipped"].as_u64()).unwrap_or(0),
        "pc1_seg_scope":"source_ready_to_transmit_complete","pc1_seg_samples":pc1.len(),
        "pc1_seg_avg_ms":segment["avg_ms"],"pc1_seg_p50_ms":segment["p50_ms"],
        "pc1_seg_p99_ms":segment["p99_ms"],"pc1_seg_max_ms":segment["max_ms"],
        "timing_ms":{"window_frames":10000,"source":source_timing.value(),"encode_wait":encode_timing.value(),
            "rate_wait":rate_timing.value(),"transmit":transmit_timing.value(),"audit_hash":audit_timing.value(),
            "log_io":log_timing.value(),"capture_to_send":capture_to_send.value()},
        "cancelled":stopped(args.stop.as_deref()),
    });
    if let Some(progress) = args.progress.as_deref() {
        progress.update(&final_value);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn live_sender_progress(
    progress: Option<&Progress>,
    started: Instant,
    frames: u64,
    packets: u64,
    bytes: u64,
    encoded_bytes: u64,
    source_info: Option<&Value>,
    rate_window: &mut RateWindow,
    live_pc1: &mut TimedWindow,
    live_source: &mut TimedWindow,
) {
    let Some(progress) = progress else {
        return;
    };
    let now_ns = qpc_ns();
    let rate = rate_window.sample(now_ns, frames, packets, bytes);
    let pc1_live = live_pc1.summary_at(now_ns);
    let source_live = live_source.summary_at(now_ns);
    let uptime = started.elapsed().as_secs_f64().max(0.000_001);
    progress.update(&json!({
        "frames": frames,
        "sent_frames": frames,
        "sent_packets": packets,
        "bytes": bytes,
        "encoded_bytes": encoded_bytes,
        "pps": rate.pps,
        "bps": rate.bps,
        "fps_actual": rate.fps,
        "uptime_s": uptime,
        "pc1_seg_avg_ms": pc1_live["avg_ms"],
        "pc1_seg_p99_ms": pc1_live["p99_ms"],
        "timing_ms": {
            "source": source_live,
        },
        "source_info": source_info,
    }));
}

struct ByteWindow {
    entries: VecDeque<(u64, u64)>,
    bytes: u64,
    capacity: u64,
}
impl ByteWindow {
    fn new(bps: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            capacity: bps / 8,
        }
    }
    fn deadline(&mut self, now: u64, bytes: usize) -> anyhow::Result<u64> {
        ensure!(
            bytes as u64 <= self.capacity,
            "single AU {bytes} bytes exceeds the whole one-second budget {}",
            self.capacity
        );
        while self
            .entries
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) >= 1_000_000_000)
        {
            self.bytes -= self.entries.pop_front().unwrap().1;
        }
        let mut remaining = self.bytes;
        for (at, used) in &self.entries {
            if remaining.saturating_add(bytes as u64) <= self.capacity {
                return Ok(now);
            }
            remaining -= used;
            if remaining.saturating_add(bytes as u64) <= self.capacity {
                return Ok(at.saturating_add(1_000_000_000));
            }
        }
        Ok(now)
    }
    fn record(&mut self, at: u64, bytes: usize) {
        self.entries.push_back((at, bytes as u64));
        self.bytes += bytes as u64;
    }
}

struct Sent {
    start_ns: u64,
    end_ns: u64,
    packets: usize,
    bytes: u64,
    wait_ns: u64,
}
/// 直接把 16B `PcapHeader` + 完整以太帧写入 `PcapSendQueue` 缓冲区，省去中转拷贝与逐包 FFI 调用。
fn write_sendqueue_packet(
    queue: &mut PcapSendQueue,
    dst_mac: [u8; 6],
    source_mac: [u8; 6],
    fragment: &vc_transport::fragment::Fragment<'_>,
) -> anyhow::Result<usize> {
    let pkt_len = 42 + fragment.payload.len();
    let entry_bytes = size_of::<PcapHeader>() + pkt_len;
    let offset = queue.len as usize;
    ensure!(
        !queue.buffer.is_null()
            && offset
                .checked_add(entry_bytes)
                .is_some_and(|end| end <= queue.maxlen as usize),
        "pcap_sendqueue buffer capacity {} exceeded (len={offset}, need={entry_bytes})",
        queue.maxlen
    );
    let header = PcapHeader {
        ts_sec: 0,
        ts_usec: 0,
        caplen: pkt_len as u32,
        len: pkt_len as u32,
    };
    unsafe {
        let base = queue.buffer.cast::<u8>().add(offset);
        ptr::write_unaligned(base.cast::<PcapHeader>(), header);
        let dst_slice = std::slice::from_raw_parts_mut(base.add(size_of::<PcapHeader>()), pkt_len);
        let written = write_ethernet(dst_mac, source_mac, fragment, dst_slice)
            .map_err(|needed| anyhow::anyhow!("sendqueue slot requires {needed} bytes"))?;
        debug_assert_eq!(written, pkt_len);
        queue.len = (offset + size_of::<PcapHeader>() + written) as c_uint;
        Ok(written)
    }
}

struct FrameTransmitter<'a> {
    api: &'a Npcap,
    handle: *mut c_void,
    queue: Option<SendQueue>,
    packet: Vec<u8>,
    payload_max: usize,
    batch: usize,
    source_mac: [u8; 6],
    dst_mac: [u8; 6],
    pacing_us: u64,
}
impl<'a> FrameTransmitter<'a> {
    fn new(
        api: &'a Npcap,
        handle: *mut c_void,
        args: &SendArgs,
        loopback: bool,
        batch: Option<usize>,
        frame_bytes: usize,
        payload_max: usize,
    ) -> anyhow::Result<Self> {
        let queue_enabled = batch.is_some_and(|batch| batch > 1) || (batch.is_none() && !loopback);
        let queue = if queue_enabled {
            let bytes = if batch.is_some() {
                frame_bytes
            } else {
                args.codec.max_au_bytes
            };
            let packets = vc_transport::packet_count(bytes, payload_max)? as usize;
            let capacity = bytes
                .checked_add(packets * (42 + size_of::<PcapHeader>()))
                .context("sendqueue size overflow")?;
            let capacity = u32::try_from(capacity).context("sendqueue exceeds u32 capacity")?;
            let queue = unsafe { (api.queue_alloc)(capacity) };
            ensure!(!queue.is_null(), "pcap_sendqueue_alloc({capacity}) failed");
            Some(SendQueue(queue, api.queue_destroy))
        } else {
            None
        };
        Ok(Self {
            api,
            handle,
            queue,
            packet: vec![0u8; 42 + payload_max],
            payload_max,
            batch: batch.unwrap_or(usize::MAX),
            source_mac: args.source_mac,
            dst_mac: args.dst_mac,
            pacing_us: args.pacing_us,
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn transmit(
        &mut self,
        frame: &[u8],
        frame_id: u32,
        source_ns: u64,
        pattern: u8,
        earliest: u64,
        mut limiter: Option<&mut ByteWindow>,
        stop: Option<&AtomicBool>,
    ) -> anyhow::Result<Option<Sent>> {
        let total = vc_transport::packet_count(frame.len(), self.payload_max)? as usize;
        let mut queued = 0;
        let mut bytes = 0;
        let mut start_ns = 0;
        let mut wait_ns = 0;
        for fragment in fragments(
            frame,
            frame_id,
            FragmentConfig {
                payload_max: self.payload_max,
                qpc_ns: source_ns,
                pattern,
            },
        )? {
            let len = if let Some(queue) = self.queue.as_ref() {
                let len = unsafe {
                    write_sendqueue_packet(&mut *queue.0, self.dst_mac, self.source_mac, &fragment)?
                };
                queued += 1;
                len
            } else {
                write_ethernet(self.dst_mac, self.source_mac, &fragment, &mut self.packet)
                    .map_err(|needed| anyhow::anyhow!("packet buffer requires {needed} bytes"))?
            };
            bytes += len as u64;
            let flush = self.queue.is_none()
                || queued == self.batch
                || fragment.header.seq as usize + 1 == total;
            if !flush {
                continue;
            }
            if start_ns == 0 {
                let waiting = qpc_ns();
                let budget = if let Some(limiter) = limiter.as_mut() {
                    limiter.deadline(waiting, frame.len())?
                } else {
                    0
                };
                if !loop_sleep(earliest.max(budget), stop) {
                    if let Some(queue) = self.queue.as_ref() {
                        unsafe {
                            (*queue.0).len = 0;
                        }
                    }
                    return Ok(None);
                }
                start_ns = qpc_ns();
                wait_ns = start_ns.saturating_sub(waiting);
                if let Some(limiter) = limiter.as_mut() {
                    limiter.record(start_ns, frame.len());
                }
            }
            if let Some(queue) = self.queue.as_ref() {
                let expected = unsafe { (*queue.0).len };
                let actual = unsafe { (self.api.queue_transmit)(self.handle, queue.0, 0) };
                ensure!(
                    actual == expected,
                    "pcap_sendqueue_transmit short send: {actual}/{expected} bytes for {queued} packets; no batch fallback"
                );
                unsafe {
                    (*queue.0).len = 0;
                }
                queued = 0;
            } else {
                ensure!(
                    unsafe {
                        (self.api.send_packet)(self.handle, self.packet.as_ptr(), len as c_int)
                    } == 0,
                    "pcap_sendpacket failed: {}",
                    self.api.handle_error(self.handle, "send packet")
                );
            }
            // 取消只发生在整帧之前或之后，绝不打断已开始的帧。停止时诊断节拍
            // 可以缩短。
            if self.pacing_us > 0 {
                let _ = loop_sleep(
                    qpc_ns().saturating_add(self.pacing_us.saturating_mul(1000)),
                    stop,
                );
            }
        }
        Ok(Some(Sent {
            start_ns,
            end_ns: qpc_ns(),
            packets: total,
            bytes,
            wait_ns,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_window_waits_for_enough_expirations_and_rejects_oversize() {
        let mut window = ByteWindow::new(800);
        window.record(10, 60);
        window.record(20, 30);
        assert_eq!(window.deadline(30, 50).unwrap(), 1_000_000_010);
        assert_eq!(window.deadline(30, 80).unwrap(), 1_000_000_020);
        assert_eq!(window.deadline(1_000_000_020, 100).unwrap(), 1_000_000_020);
        assert!(window.deadline(1_000_000_030, 101).is_err());
    }
    #[test]
    fn empty_timing_has_no_invented_zero_latency() {
        assert!(Samples::default().value()["p99_ms"].is_null());
    }

    #[test]
    fn direct_sendqueue_write_matches_npcap_layout() {
        let payload = vec![0xABu8; 1458];
        let frag = fragments(
            &payload,
            9,
            FragmentConfig {
                payload_max: 1458,
                qpc_ns: 12345,
                pattern: 4,
            },
        )
        .unwrap()
        .next()
        .unwrap();
        let pkt_len = 42 + frag.payload.len();
        let mut raw_buf = vec![0u8; 2 * (size_of::<PcapHeader>() + pkt_len)];
        let mut queue = PcapSendQueue {
            maxlen: raw_buf.len() as c_uint,
            len: 0,
            buffer: raw_buf.as_mut_ptr().cast::<c_char>(),
        };
        let written = write_sendqueue_packet(&mut queue, [0xFF; 6], [2; 6], &frag).unwrap();
        assert_eq!(written, pkt_len);
        assert_eq!(queue.len as usize, size_of::<PcapHeader>() + pkt_len);
        let hdr = unsafe { ptr::read_unaligned(raw_buf.as_ptr().cast::<PcapHeader>()) };
        assert_eq!(hdr.caplen as usize, pkt_len);
        assert_eq!(hdr.len as usize, pkt_len);

        let mut expected = vec![0u8; pkt_len];
        write_ethernet([0xFF; 6], [2; 6], &frag, &mut expected).unwrap();
        assert_eq!(
            &raw_buf[size_of::<PcapHeader>()..queue.len as usize],
            &expected
        );
    }

    #[test]
    fn tx_audit_worker_flushes_all_rows_and_recycles_buffers() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "vc-stream-tx-audit-{}-{unique}.jsonl",
            std::process::id()
        ));
        let log = take_log_writer(None, Some(&path)).unwrap().unwrap();
        let worker = TxAuditWorker::spawn(log, "unit").unwrap();
        for fid in 0..8u32 {
            let data = vec![fid as u8; 64];
            worker
                .submit(TxAuditJob {
                    frame_id: fid,
                    raw: true,
                    is_pattern: true,
                    frame: data,
                    source_frame: None,
                    frame_start: 100 + u64::from(fid),
                    source_ready: 110 + u64::from(fid),
                    send_start_ns: 120 + u64::from(fid),
                    encode_ns: 0,
                    encode_done: 110 + u64::from(fid),
                })
                .unwrap();
        }
        let (audit_timing, log_timing) = worker.finish().unwrap();
        assert_eq!(audit_timing.len(), 8);
        assert_eq!(log_timing.len(), 8);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 8);
        let _ = std::fs::remove_file(&path);
    }
}
