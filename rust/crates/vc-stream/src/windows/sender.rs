//! 共享的进程内发送端。物理网卡以一次 queue 提交完整帧。
use super::*;

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
    let mut source = match args.source.mode.as_str() {
        "pattern" => None,
        "dda" | "wgc" => Some(DesktopSource::open(
            &args.source,
            args.common.width,
            args.common.height,
        )?),
        other => bail!("source mode {other} is not supported"),
    };
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
    let mut hashlog = take_log_writer(args.hash_file.take(), args.hashlog.as_deref())?;
    let mut source_frame = vec![0u8; frame_bytes];
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
    let ready = json!({
        "ready":true,"running":true,"role":"sender","session_id":args.common.session,
        "adapter":adapter,"width":args.common.width,"height":args.common.height,
        "fps":args.common.fps,"fps_target":args.common.fps,"mtu":args.common.mtu,
        "ethertype":format!("0x{:04x}",args.common.ethertype),"frame_bytes":frame_bytes,
        "packets_per_frame":if raw { Some(frame_packets) } else { None },
        "batch":batch,"batch_packets":batch,"batch_policy":if args.batch.is_some(){"diagnostic_override"}else if loopback{"loopback_packet"}else{"frame"},
        "pacing_us":args.pacing_us,"codec":args.codec.codec,"source":args.source.mode,
        "pattern":if source.is_none(){Some(args.pattern.name())}else{None},
        "transform":args.source.transform,"source_info":source.as_ref().map(DesktopSource::stats),
        "encoder":encoder_settings,"encoder_configuration":encoder_settings,"npcap":api.version(),
        "sendqueue":if tx.queue.is_some(){"queue"}else{"packet"},
        "send_errors":0,"sent_frames":0,"frames":0,"sent_packets":0,
        "max_bitrate":if raw{None}else{Some(args.codec.max_bitrate_bps)},
        "max_au_bytes":if raw{None}else{Some(args.codec.max_au_bytes)},
        "gop":if raw{None}else{Some(1)},"encoder_in_flight":1,
        "cpu_scope":"whole_host_process; concurrent DLL sessions share this counter",
    });
    publish_progress(args.progress.as_deref(), args.ready.as_deref(), &ready)?;
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
    let mut audit_timing = Samples::default();
    let mut log_timing = Samples::default();
    let mut capture_to_send = Samples::default();
    let mut rate_window = RateWindow::new(start_qpc);
    let mut live_pc1 = TimedWindow::default();
    let mut live_source = TimedWindow::default();
    let mut last_progress = Instant::now();
    loop {
        if args.frames > 0 && frames == args.frames {
            break;
        }
        if !loop_sleep(pacer.next_ns, args.stop.as_deref()) {
            break;
        }
        let frame_id = frames as u32;
        let frame_start = qpc_ns();
        // 从采集起点排程。编码与发送速率上限属于本帧内部的工作，不应当作
        // 唤醒迟到量叠加到每个周期。
        pacer.advance(frame_start);
        if let Some(source) = source.as_mut() {
            // 静态桌面是正常情况。每次源调用都有界；只有真正的源错误或本会话
            // 取消才会结束等待。
            let mut captured = false;
            while !stopped(args.stop.as_deref()) {
                if source.capture_cancellable(
                    &mut source_frame,
                    Some(args.stop.as_deref().unwrap_or(&CLI_STOP)),
                )? {
                    captured = true;
                    break;
                }
                // 静态/未到首帧时仍刷新最近 1 秒滑窗计数，空闲超过 1 秒自动归零。
                if last_progress.elapsed() >= Duration::from_millis(250) {
                    live_sender_progress(
                        args.progress.as_deref(),
                        started,
                        frames,
                        packets,
                        bytes,
                        encoded_bytes,
                        Some(source),
                        &mut rate_window,
                        &mut live_pc1,
                        &mut live_source,
                    );
                    last_progress = Instant::now();
                }
                thread::yield_now();
            }
            if !captured {
                break;
            }
        } else {
            args.pattern.fill(geometry, frame_id, &mut source_frame);
        }
        let source_ready = qpc_ns();
        if stopped(args.stop.as_deref()) {
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
            if source.is_some() {
                255
            } else {
                args.pattern.id()
            },
            earliest,
            limiter.as_mut(),
            args.stop.as_deref(),
        )?
        else {
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
        // 审计哈希在发包之后且仅当启用 hashlog 时计算，避免拖慢线缆发送。
        let audit_started = qpc_ns();
        let hashes = hashlog.as_ref().map(|_| {
            let transport_hash = fnv1a64(frame);
            let source_hash = if raw {
                transport_hash
            } else {
                fnv1a64(&source_frame)
            };
            (transport_hash, source_hash)
        });
        let audit_ns = qpc_ns().saturating_sub(audit_started);
        audit_timing.push(audit_ns);
        let log_started = qpc_ns();
        if let (Some(writer), Some((transport_hash, source_hash))) = (hashlog.as_mut(), hashes) {
            writer.write_line(|w| {
                if raw {
                    writeln!(
                        w,
                        "{{\"frame_id\":{frame_id},\"hash\":\"{transport_hash:016x}\",\"qpc_ns\":{frame_start},\"source_ready_qpc_ns\":{source_ready},\"send_qpc_ns\":{}}}",
                        sent.start_ns
                    )
                } else if source.is_none() {
                    writeln!(
                        w,
                        "{{\"frame_id\":{frame_id},\"source_hash\":\"{source_hash:016x}\",\"transport_hash\":\"{transport_hash:016x}\",\"encoded_bytes\":{},\"keyframe\":true,\"fault_injected\":false,\"qpc_ns\":{frame_start},\"source_ready_qpc_ns\":{source_ready},\"send_qpc_ns\":{},\"encode_wait_ns\":{encode_ns},\"prepare_lead_ns\":0,\"prepare_work_ns\":{}}}",
                        frame.len(),
                        sent.start_ns,
                        encode_done.saturating_sub(frame_start)
                    )
                } else {
                    writeln!(
                        w,
                        "{{\"frame_id\":{frame_id},\"source_hash\":\"{source_hash:016x}\",\"transport_hash\":\"{transport_hash:016x}\",\"encoded_bytes\":{},\"keyframe\":true,\"fault_injected\":false,\"qpc_ns\":{frame_start},\"source_ready_qpc_ns\":{source_ready},\"send_qpc_ns\":{},\"encode_wait_ns\":{encode_ns},\"prepare_lead_ns\":0,\"prepare_work_ns\":null}}",
                        frame.len(),
                        sent.start_ns
                    )
                }
            })?;
        }
        log_timing.push(qpc_ns().saturating_sub(log_started));
        frames += 1;
        rate_window.maybe_record(sent.end_ns, frames, packets, bytes);
        if last_progress.elapsed() >= Duration::from_millis(250) {
            live_sender_progress(
                args.progress.as_deref(),
                started,
                frames,
                packets,
                bytes,
                encoded_bytes,
                source.as_ref(),
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
    if let Some(writer) = hashlog.as_mut() {
        writer.flush()?;
    }
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
        "source_info":source.as_ref().map(DesktopSource::stats),
        "source_skipped":source.as_ref().and_then(|s|s.stats()["source_skipped"].as_u64()).unwrap_or(0),
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
    source: Option<&DesktopSource>,
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
        "source_info": source.map(DesktopSource::stats),
    }));
}

#[derive(Default)]
struct Samples(VecDeque<u64>);
impl Samples {
    fn push(&mut self, ns: u64) {
        if self.0.len() == 10000 {
            self.0.pop_front();
        }
        self.0.push_back(ns);
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn value(&self) -> Value {
        if self.0.is_empty() {
            return json!({"samples":0,"avg_ms":null,"p50_ms":null,"p99_ms":null,"max_ms":null});
        }
        let mut values: Vec<_> = self.0.iter().copied().collect();
        values.sort_unstable();
        let n = values.len();
        json!({"samples":n,"avg_ms":values.iter().map(|v|*v as f64).sum::<f64>()/n as f64/1e6,
            "p50_ms":values[(n*50).div_ceil(100)-1] as f64/1e6,
            "p99_ms":values[(n*99).div_ceil(100)-1] as f64/1e6,"max_ms":values[n-1] as f64/1e6})
    }
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
            let len = write_ethernet(self.dst_mac, self.source_mac, &fragment, &mut self.packet)
                .map_err(|needed| anyhow::anyhow!("packet buffer requires {needed} bytes"))?;
            bytes += len as u64;
            if let Some(queue) = self.queue.as_ref() {
                let header = PcapHeader {
                    ts_sec: 0,
                    ts_usec: 0,
                    caplen: len as u32,
                    len: len as u32,
                };
                ensure!(
                    unsafe { (self.api.queue_packet)(queue.0, &header, self.packet.as_ptr()) } == 0,
                    "pcap_sendqueue_queue failed for frame {frame_id}"
                );
                queued += 1;
            }
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
}
