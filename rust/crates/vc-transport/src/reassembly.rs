//! 有界、乱序容忍的 Raw NV12 重组。
//!
//! 语义沿用 Git 历史中 C++ `Reassembler` 的冻结契约：
//! - 先校验 EtherType / 源 MAC（会话隔离，非认证），再解析 28 字节头；
//! - `total` 必须等于当前会话的 `ceil(frame_bytes/payload_max)`，`seq < total`；
//! - 每包 `payload_len` 必须是该 seq 的精确长度，CRC 只覆盖 `payload_len` 字节；
//! - 已完整发布的帧号之后的迟到包不重建，避免旧帧复活；
//! - 同一帧内重复包忽略（已计入收包数），坏 CRC/尺寸不符计入丢包；
//! - 未完成帧最多 `max_inflight_frames` 个，按帧号淘汰最旧；完成新帧时丢弃更旧残帧；
//! - 缓冲总量不超过 `max_buffered_bytes`，绝不无界增长。
//!
//! 相较历史 C++ 实现的有意差异：
//! 1. 帧号比较用 [`crate::wire::is_after`] 的序号算术，跨 2^32 回绕仍把新帧视为新；
//!    C++ 用数值比较，非回绕场景两者完全一致。
//! 2. 额外记录 `duplicate`、`crc_errors`、`ignored_other_source`、`late` 等诊断计数；
//!    `packets_lost`/`dropped`/`gaps` 的口径保持不变。
//! 3. 帧缓冲按槽独立分配并在完成时移交调用方；SHM 发布由 `vc-stream` 接收路径负责。

use std::collections::VecDeque;
use std::time::Instant;

use crate::checksum::{crc32, fnv1a64, fnv1a64_hex};
use crate::wire::{
    ETHERNET_HEADER_LEN, ETHERTYPE, Geometry, HEADER_LEN, Header, L2_OVERHEAD, MAGIC, WireError,
    is_after, packet_count, payload_len_at, payload_max_for_mtu,
};

const INLINE_BITSET_WORDS: usize = 8;
pub(crate) const INLINE_BITSET_PACKETS: usize = INLINE_BITSET_WORDS * 64;

/// 紧凑分片到达位图：<=512 包（如 640×640@MTU1500 的 422 包）直接内联 `[u64; 8]`
/// 实现零堆分配；超过 512 包时从调用方池中复用 `Vec<u64>`。
pub(crate) enum PacketBitset {
    Inline([u64; INLINE_BITSET_WORDS]),
    Heap(Vec<u64>),
}

impl PacketBitset {
    #[inline]
    pub(crate) fn take(pool: &mut Vec<Vec<u64>>, total: usize) -> Self {
        if total <= INLINE_BITSET_PACKETS {
            Self::Inline([0u64; INLINE_BITSET_WORDS])
        } else {
            let words = total.div_ceil(64);
            if let Some(mut bits) = pool.pop() {
                bits.fill(0);
                bits.resize(words, 0);
                Self::Heap(bits)
            } else {
                Self::Heap(vec![0u64; words])
            }
        }
    }

    #[inline]
    pub(crate) fn recycle(self, pool: &mut Vec<Vec<u64>>, cap: usize) {
        if let Self::Heap(bits) = self
            && pool.len() < cap
        {
            pool.push(bits);
        }
    }

    #[inline]
    pub(crate) fn contains(&self, seq: u16) -> bool {
        let idx = (seq as usize) >> 6;
        let mask = 1u64 << (seq & 63);
        match self {
            Self::Inline(words) => words[idx & (INLINE_BITSET_WORDS - 1)] & mask != 0,
            Self::Heap(words) => words.get(idx).is_some_and(|&word| word & mask != 0),
        }
    }

    #[inline]
    pub(crate) fn insert(&mut self, seq: u16) {
        let idx = (seq as usize) >> 6;
        let mask = 1u64 << (seq & 63);
        match self {
            Self::Inline(words) => words[idx & (INLINE_BITSET_WORDS - 1)] |= mask,
            Self::Heap(words) => {
                if let Some(word) = words.get_mut(idx) {
                    *word |= mask;
                }
            }
        }
    }
}

/// 重组窗口大小（与 C++ 相同的观测窗口）。
const RSEG_WINDOW: usize = 10_000;
const INTER_ARRIVAL_WINDOW: usize = 120;
/// 默认最多同时跟踪 8 个未完成帧；默认总缓冲上限 64 MiB。
pub const DEFAULT_MAX_INFLIGHT_FRAMES: usize = 8;
pub const DEFAULT_MAX_BUFFERED_BYTES: usize = 64 * 1024 * 1024;

/// 会话与容量配置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub geometry: Geometry,
    pub payload_max: usize,
    pub total: u16,
    /// 非空时只接受该源 MAC 的包（控制面确认的会话源）。
    pub source_mac: Option<[u8; 6]>,
    pub max_inflight_frames: usize,
    pub max_buffered_bytes: usize,
}

impl Config {
    /// 由会话几何与 MTU 建立默认配置（8 帧 / 64 MiB）。
    pub fn new(geometry: Geometry, mtu: usize) -> Result<Self, WireError> {
        let payload_max = payload_max_for_mtu(mtu)?;
        let total = packet_count(geometry.frame_bytes(), payload_max)?;
        Ok(Self {
            geometry,
            payload_max,
            total,
            source_mac: None,
            max_inflight_frames: DEFAULT_MAX_INFLIGHT_FRAMES,
            max_buffered_bytes: DEFAULT_MAX_BUFFERED_BYTES.max(geometry.frame_bytes()),
        })
    }

    pub fn with_source_mac(mut self, mac: Option<[u8; 6]>) -> Self {
        self.source_mac = mac;
        self
    }
}

/// 计数器。语义见模块文档；`crc_errors` 同时计入 `packets_lost`，便于定位。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub packets_recv: u64,
    pub bytes_recv: u64,
    pub packets_lost: u64,
    pub dropped: u64,
    pub complete: u64,
    pub gaps: u64,
    pub duplicate: u64,
    pub crc_errors: u64,
    pub ignored_other_source: u64,
    pub late: u64,
}

/// 完成帧。缓冲区随结构体移交；调用方负责发布/哈希后释放。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedFrame {
    pub frame_id: u32,
    pub pattern: u8,
    pub key_frame: bool,
    pub first_packet_ns: u64,
    pub completed_ns: u64,
    pub data: Vec<u8>,
}

impl CompletedFrame {
    /// 整帧 FNV-1a64（与 receiver hashlog 的口径一致）。
    pub fn hash(&self) -> u64 {
        fnv1a64(&self.data)
    }

    pub fn hash_hex(&self) -> String {
        fnv1a64_hex(self.hash())
    }
}

/// 包被忽略的原因。只有调用方需要区分时才使用；统计口径已记在 [`Stats`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoreReason {
    /// 不足以太头 + VC 头。
    ShortFrame,
    /// EtherType 不是 0x88B5。
    WrongEtherType,
    /// 源 MAC 不是本会话（未计入丢包）。
    OtherSource,
    /// magic 不是 VCAP（未计入丢包）。
    BadMagic,
    /// total/seq 不属于当前会话。
    BadGeometry,
    /// 载荷长度与 seq 不符。
    LengthMismatch,
    /// 捕获长度短于包头声明的载荷。
    TruncatedPayload,
    /// CRC 不符。
    BadCrc,
    /// 同帧重复包。
    Duplicate,
    /// 已完成帧号之后的迟到包。
    Late,
    /// 同帧出现不同 total/pattern。
    SlotConflict,
    /// 缓冲上限不允许再接收新帧。
    Capacity,
}

/// 一次 `push` 的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// 本包凑齐整帧。
    Completed(CompletedFrame),
    /// 已接收，帧仍未完成。
    Accepted,
    /// 未接收，原因见 `IgnoreReason`。
    Ignored(IgnoreReason),
}

impl PushOutcome {
    pub fn completed(self) -> Option<CompletedFrame> {
        match self {
            Self::Completed(frame) => Some(frame),
            _ => None,
        }
    }
}

/// 最近窗口的统计摘要（同机单调时钟，不跨机比较）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TimingSummary {
    pub samples: usize,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

struct Slot {
    frame_id: u32,
    total: u16,
    pattern: u8,
    key_frame: bool,
    first_ns: u64,
    buffer: Vec<u8>,
    got: PacketBitset,
    got_n: u16,
}

/// 单线程重组器：调用方在同一线程按到达顺序 `push`。
pub struct Reassembler {
    config: Config,
    frame_bytes: usize,
    tail_payload_len: usize,
    slots: Vec<Slot>,
    /// 复用的帧缓冲区与超长位图切片，避免逐帧大内存分配与页清零。
    free_buffers: Vec<Vec<u8>>,
    free_bitsets: Vec<Vec<u64>>,
    free_pool_cap: usize,
    buffered_bytes: usize,
    stats: Stats,
    have_seen: bool,
    max_seen_fid: u32,
    last_complete_ns: u64,
    last_complete_fid: u32,
    rseg_ns: VecDeque<u64>,
    inter_arrival_ns: VecDeque<u64>,
    origin: Instant,
}

impl Reassembler {
    pub fn new(config: Config) -> Result<Self, WireError> {
        let frame_bytes = config.geometry.frame_bytes();
        if config.payload_max == 0 || config.total == 0 {
            return Err(WireError::BadMtu(config.payload_max + L2_OVERHEAD));
        }
        if config.total as usize != packet_count(frame_bytes, config.payload_max)? as usize {
            return Err(WireError::BadFragment {
                bytes: frame_bytes,
                payload_max: config.payload_max,
            });
        }
        if config.max_inflight_frames == 0 || config.max_buffered_bytes < frame_bytes {
            return Err(WireError::BadFragment {
                bytes: frame_bytes,
                payload_max: config.payload_max,
            });
        }
        let tail_payload_len = payload_len_at(
            config.total - 1,
            config.total,
            frame_bytes,
            config.payload_max,
        );
        let slot_cap = config.max_inflight_frames.min(16);
        let free_pool_cap = slot_cap + 2;
        Ok(Self {
            config,
            frame_bytes,
            tail_payload_len,
            slots: Vec::with_capacity(slot_cap),
            free_buffers: Vec::with_capacity(free_pool_cap),
            free_bitsets: Vec::with_capacity(free_pool_cap),
            free_pool_cap,
            buffered_bytes: 0,
            stats: Stats::default(),
            have_seen: false,
            max_seen_fid: 0,
            last_complete_ns: 0,
            last_complete_fid: 0,
            rseg_ns: VecDeque::with_capacity(1024),
            inter_arrival_ns: VecDeque::with_capacity(INTER_ARRIVAL_WINDOW),
            origin: Instant::now(),
        })
    }

    /// 归还已消费的完整帧缓冲区以供后续帧复用，避免重复触发系统页分配。
    pub fn recycle(&mut self, mut buffer: Vec<u8>) {
        let frame_bytes = self.frame_bytes;
        if buffer.capacity() >= frame_bytes && self.free_buffers.len() < self.free_pool_cap {
            buffer.resize(frame_bytes, 0);
            self.free_buffers.push(buffer);
        }
    }

    fn take_buffer(&mut self) -> Vec<u8> {
        if let Some(mut buffer) = self.free_buffers.pop() {
            buffer.resize(self.frame_bytes, 0);
            buffer
        } else {
            vec![0u8; self.frame_bytes]
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn inflight_frames(&self) -> usize {
        self.slots.len()
    }

    pub fn inflight_bytes(&self) -> usize {
        self.buffered_bytes
    }

    /// 首包到达 → 重组完成，最近 10000 帧。
    pub fn rseg_summary(&self) -> TimingSummary {
        summarize(&self.rseg_ns)
    }

    /// 完成帧到达间隔，最近 120 帧。
    pub fn inter_arrival_summary(&self) -> TimingSummary {
        summarize(&self.inter_arrival_ns)
    }

    /// 丢弃全部未完成帧并计账（停止/超时前调用）。
    pub fn finish(&mut self) {
        while !self.slots.is_empty() {
            self.drop_slot(0);
        }
    }

    /// 本机单调时钟（纳秒），用于默认时间戳。
    pub fn now_ns(&self) -> u64 {
        self.origin.elapsed().as_nanos() as u64
    }

    /// 解析完整以太帧并重组。Npcap 回调可直接把 `caplen` 字节交到这里。
    pub fn push_ethernet(&mut self, frame: &[u8]) -> PushOutcome {
        let now = self.now_ns();
        self.push_ethernet_at(frame, now)
    }

    /// 回放或确定性测试：用给定的包时间戳作为到达与重组完成时间。
    pub fn push_ethernet_at(&mut self, frame: &[u8], now_ns: u64) -> PushOutcome {
        self.push_ethernet_timed(frame, now_ns, || now_ns)
    }

    /// 实时接收：使用同一单调时基，最后一片通过 CRC、复制并完成重组后才调用时钟。
    pub fn push_ethernet_timed(
        &mut self,
        frame: &[u8],
        arrived_ns: u64,
        completed_at: impl FnOnce() -> u64,
    ) -> PushOutcome {
        self.push_ethernet_timed_lazy(frame, || arrived_ns, completed_at)
    }

    /// 实时接收优化路径：仅在为新帧创建槽位时才调用 `on_first_packet`，避免非首片
    /// 或无关包触发不必要的时间戳读取。
    #[inline]
    pub fn push_ethernet_timed_lazy(
        &mut self,
        frame: &[u8],
        on_first_packet: impl FnOnce() -> u64,
        completed_at: impl FnOnce() -> u64,
    ) -> PushOutcome {
        if frame.len() < L2_OVERHEAD {
            return PushOutcome::Ignored(IgnoreReason::ShortFrame);
        }
        if u16::from_be_bytes([frame[12], frame[13]]) != ETHERTYPE {
            return PushOutcome::Ignored(IgnoreReason::WrongEtherType);
        }
        if let Some(expected) = self.config.source_mac
            && frame[6..12] != expected
        {
            self.stats.ignored_other_source += 1;
            return PushOutcome::Ignored(IgnoreReason::OtherSource);
        }
        let hdr_bytes: &[u8; HEADER_LEN] = frame[ETHERNET_HEADER_LEN..L2_OVERHEAD]
            .try_into()
            .expect("28 字节");
        let header = match Header::parse_array(hdr_bytes) {
            Ok(header) => header,
            Err(WireError::BadMagic(_)) => return PushOutcome::Ignored(IgnoreReason::BadMagic),
            Err(_) => return PushOutcome::Ignored(IgnoreReason::ShortFrame),
        };
        let payload = &frame[L2_OVERHEAD..];
        self.push_packet_core(header, payload, on_first_packet, completed_at)
    }

    /// 已解析包头的路径。`source_mac` 只在配置了会话源 MAC 时使用：
    /// 配置了却传 `None` 会按其他会话拒绝，避免调用方绕过会话隔离。
    pub fn push_packet_at(
        &mut self,
        header: Header,
        payload: &[u8],
        source_mac: Option<[u8; 6]>,
        now_ns: u64,
    ) -> PushOutcome {
        self.push_packet_timed_lazy(header, payload, source_mac, || now_ns, || now_ns)
    }

    #[cfg(test)]
    fn push_packet_timed(
        &mut self,
        header: Header,
        payload: &[u8],
        source_mac: Option<[u8; 6]>,
        arrived_ns: u64,
        completed_at: impl FnOnce() -> u64,
    ) -> PushOutcome {
        self.push_packet_timed_lazy(header, payload, source_mac, || arrived_ns, completed_at)
    }

    fn push_packet_timed_lazy(
        &mut self,
        header: Header,
        payload: &[u8],
        source_mac: Option<[u8; 6]>,
        on_first_packet: impl FnOnce() -> u64,
        completed_at: impl FnOnce() -> u64,
    ) -> PushOutcome {
        if let Some(expected) = self.config.source_mac
            && source_mac != Some(expected)
        {
            self.stats.ignored_other_source += 1;
            return PushOutcome::Ignored(IgnoreReason::OtherSource);
        }
        if header.magic != MAGIC {
            return PushOutcome::Ignored(IgnoreReason::BadMagic);
        }
        self.push_packet_core(header, payload, on_first_packet, completed_at)
    }

    #[inline]
    fn push_packet_core(
        &mut self,
        header: Header,
        payload: &[u8],
        on_first_packet: impl FnOnce() -> u64,
        completed_at: impl FnOnce() -> u64,
    ) -> PushOutcome {
        if header.total != self.config.total || header.seq >= header.total {
            self.stats.packets_lost += 1;
            return PushOutcome::Ignored(IgnoreReason::BadGeometry);
        }
        let expected_len = if header.seq + 1 < header.total {
            self.config.payload_max
        } else {
            self.tail_payload_len
        };
        if header.payload_len as usize != expected_len {
            self.stats.packets_lost += 1;
            return PushOutcome::Ignored(IgnoreReason::LengthMismatch);
        }
        let Some(payload) = payload.get(..expected_len) else {
            return PushOutcome::Ignored(IgnoreReason::TruncatedPayload);
        };
        if crc32(payload) != header.payload_crc {
            self.stats.packets_lost += 1;
            self.stats.crc_errors += 1;
            return PushOutcome::Ignored(IgnoreReason::BadCrc);
        }
        self.stats.packets_recv += 1;
        self.stats.bytes_recv += (L2_OVERHEAD + expected_len) as u64;

        // 与 C++ 相同的断档检测点：CRC 通过后、迟到判断前。
        if !self.have_seen {
            self.have_seen = true;
            self.max_seen_fid = header.frame_id;
        } else if is_after(header.frame_id, self.max_seen_fid) {
            self.stats.gaps += (header.frame_id.wrapping_sub(self.max_seen_fid) - 1) as u64;
            self.max_seen_fid = header.frame_id;
        }
        if self.last_complete_ns != 0 && !is_after(header.frame_id, self.last_complete_fid) {
            self.stats.late += 1;
            return PushOutcome::Ignored(IgnoreReason::Late);
        }

        let index = match self
            .slots
            .iter()
            .position(|slot| slot.frame_id == header.frame_id)
        {
            Some(index) => index,
            None => {
                if let Err(reason) = self.make_room() {
                    return PushOutcome::Ignored(reason);
                }
                let first_ns = on_first_packet();
                let buffer = self.take_buffer();
                let got = PacketBitset::take(&mut self.free_bitsets, header.total as usize);
                self.slots.push(Slot {
                    frame_id: header.frame_id,
                    total: header.total,
                    pattern: header.pattern,
                    key_frame: header.is_key_frame(),
                    first_ns,
                    buffer,
                    got,
                    got_n: 0,
                });
                self.buffered_bytes += self.frame_bytes;
                self.slots.len() - 1
            }
        };
        let slot = &mut self.slots[index];
        if slot.total != header.total || slot.pattern != header.pattern {
            self.stats.packets_lost += 1;
            return PushOutcome::Ignored(IgnoreReason::SlotConflict);
        }
        if slot.got.contains(header.seq) {
            self.stats.duplicate += 1;
            return PushOutcome::Ignored(IgnoreReason::Duplicate);
        }
        let offset = header.seq as usize * self.config.payload_max;
        let end = offset + expected_len;
        if end > slot.buffer.len() {
            self.stats.packets_lost += 1;
            return PushOutcome::Ignored(IgnoreReason::BadGeometry);
        }
        slot.buffer[offset..end].copy_from_slice(payload);
        slot.got.insert(header.seq);
        slot.got_n += 1;

        if slot.got_n != slot.total {
            return PushOutcome::Accepted;
        }
        let completed_ns = completed_at();

        let slot = self.slots.remove(index);
        self.buffered_bytes -= slot.buffer.len();
        slot.got.recycle(&mut self.free_bitsets, self.free_pool_cap);
        self.stats.complete += 1;
        let rseg = completed_ns.saturating_sub(slot.first_ns);
        push_window(&mut self.rseg_ns, rseg, RSEG_WINDOW);
        if self.last_complete_ns != 0 {
            push_window(
                &mut self.inter_arrival_ns,
                completed_ns.saturating_sub(self.last_complete_ns),
                INTER_ARRIVAL_WINDOW,
            );
        }
        self.last_complete_ns = completed_ns;
        self.last_complete_fid = slot.frame_id;
        // 新帧已完成：更旧的残帧不会再有意义，按 C++ 规则计丢并释放。
        let mut i = 0;
        while i < self.slots.len() {
            if is_after(self.slots[i].frame_id, slot.frame_id) {
                i += 1;
            } else {
                self.drop_slot(i);
            }
        }
        PushOutcome::Completed(CompletedFrame {
            frame_id: slot.frame_id,
            pattern: slot.pattern,
            key_frame: slot.key_frame,
            first_packet_ns: slot.first_ns,
            completed_ns,
            data: slot.buffer,
        })
    }

    fn make_room(&mut self) -> Result<(), IgnoreReason> {
        let frame_bytes = self.frame_bytes;
        while self.slots.len() >= self.config.max_inflight_frames {
            match self.oldest_index() {
                Some(index) => self.drop_slot(index),
                None => return Err(IgnoreReason::Capacity),
            }
        }
        while self.buffered_bytes + frame_bytes > self.config.max_buffered_bytes {
            match self.oldest_index() {
                Some(index) => self.drop_slot(index),
                None => {
                    self.stats.packets_lost += 1;
                    return Err(IgnoreReason::Capacity);
                }
            }
        }
        Ok(())
    }

    fn oldest_index(&self) -> Option<usize> {
        (0..self.slots.len()).find(|&i| {
            !self
                .slots
                .iter()
                .enumerate()
                .any(|(j, other)| j != i && is_after(other.frame_id, self.slots[i].frame_id))
        })
    }

    fn drop_slot(&mut self, index: usize) {
        let slot = self.slots.remove(index);
        self.buffered_bytes = self.buffered_bytes.saturating_sub(slot.buffer.len());
        self.stats.dropped += 1;
        self.stats.packets_lost += slot.total.wrapping_sub(slot.got_n) as u64;
        self.recycle(slot.buffer);
        slot.got.recycle(&mut self.free_bitsets, self.free_pool_cap);
    }
}

/// 只保留最新一帧的消费槽：未来 receiver 的发布/哈希线程可用它对齐“只留最新”。
#[derive(Default)]
pub struct LatestFrame {
    slot: Option<CompletedFrame>,
    pub superseded: u64,
}

impl LatestFrame {
    /// 放入新完成帧；若上一帧尚未取走，计入 `superseded`。
    pub fn put(&mut self, frame: CompletedFrame) {
        if self.slot.replace(frame).is_some() {
            self.superseded += 1;
        }
    }

    pub fn take(&mut self) -> Option<CompletedFrame> {
        self.slot.take()
    }

    pub fn peek(&self) -> Option<&CompletedFrame> {
        self.slot.as_ref()
    }
}

pub(crate) fn push_window(window: &mut VecDeque<u64>, value: u64, cap: usize) {
    if window.len() >= cap {
        window.pop_front();
    }
    window.push_back(value);
}

pub(crate) fn summarize(window: &VecDeque<u64>) -> TimingSummary {
    if window.is_empty() {
        return TimingSummary::default();
    }
    let mut sorted: Vec<u64> = window.iter().copied().collect();
    sorted.sort_unstable();
    let sum: u128 = sorted.iter().map(|v| *v as u128).sum();
    let pct = |p: usize| sorted[(sorted.len() * p) / 100] as f64 / 1e6;
    TimingSummary {
        samples: sorted.len(),
        avg_ms: (sum as f64 / sorted.len() as f64) / 1e6,
        p50_ms: pct(50),
        p99_ms: pct(99),
        max_ms: *sorted.last().unwrap_or(&0) as f64 / 1e6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::{FragmentConfig, fragments};
    use crate::wire::{FLAG_KEY_FRAME, Geometry, MAGIC};

    fn config() -> Config {
        Config::new(Geometry::new(64, 64).unwrap(), 1500).unwrap()
    }

    fn frame(seed: u32, bytes: usize) -> Vec<u8> {
        let mut state = seed | 1;
        (0..bytes)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state & 0xFF) as u8
            })
            .collect()
    }

    fn packets(frame_id: u32, data: &[u8], payload_max: usize) -> Vec<(Header, Vec<u8>)> {
        fragments(
            data,
            frame_id,
            FragmentConfig {
                payload_max,
                qpc_ns: 7,
                pattern: 3,
            },
        )
        .unwrap()
        .map(|f| (f.header, f.payload.to_vec()))
        .collect()
    }

    fn push_all(r: &mut Reassembler, packets: &[(Header, Vec<u8>)]) -> Vec<PushOutcome> {
        packets
            .iter()
            .map(|(h, p)| r.push_packet_at(*h, p, None, 1_000))
            .collect()
    }

    #[test]
    fn reassembles_out_of_order_with_duplicate_and_bad_crc() {
        let mut r = Reassembler::new(config()).unwrap();
        let data = frame(1, 6144);
        let mut pkts = packets(41, &data, 1458);
        assert_eq!(pkts.len(), 5);
        pkts.swap(0, 2);
        let duplicate = pkts[1].clone();
        let mut bad = pkts[2].clone();
        bad.0.payload_crc ^= 1;
        let mut script = vec![pkts[0].clone(), bad, pkts[2].clone()];
        script.extend_from_slice(&pkts[1..]);
        script.push(duplicate);
        let outcomes = push_all(&mut r, &script);
        let done = outcomes
            .into_iter()
            .filter_map(PushOutcome::completed)
            .collect::<Vec<_>>();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].frame_id, 41);
        assert_eq!(done[0].data, data);
        assert_eq!(r.stats().complete, 1);
        assert_eq!(r.stats().crc_errors, 1);
        assert_eq!(r.stats().duplicate, 1);
        assert_eq!(r.stats().packets_lost, 1);
        assert_eq!(r.stats().dropped, 0);
        assert_eq!(r.inflight_frames(), 0);
        assert_eq!(r.inflight_bytes(), 0);
    }

    #[test]
    fn incomplete_frame_dropped_and_counted_on_finish() {
        let mut r = Reassembler::new(config()).unwrap();
        let data = frame(2, 6144);
        let pkts = packets(5, &data, 1458);
        push_all(&mut r, &pkts[..3]);
        assert_eq!(r.inflight_frames(), 1);
        r.finish();
        assert_eq!(r.stats().dropped, 1);
        assert_eq!(r.stats().packets_lost, 2);
        assert_eq!(r.inflight_bytes(), 0);
    }

    #[test]
    fn late_packets_do_not_resurrect_completed_frame() {
        let mut r = Reassembler::new(config()).unwrap();
        let first = frame(3, 6144);
        let second = frame(4, 6144);
        let p1 = packets(10, &first, 1458);
        let p2 = packets(11, &second, 1458);
        push_all(&mut r, &p1);
        push_all(&mut r, &p2);
        let outcome = r.push_packet_at(p1[0].0, &p1[0].1, None, 2_000);
        assert_eq!(outcome, PushOutcome::Ignored(IgnoreReason::Late));
        assert_eq!(r.stats().late, 1);
        assert_eq!(r.stats().complete, 2);
    }

    #[test]
    fn newer_frame_drops_older_incomplete() {
        let mut r = Reassembler::new(config()).unwrap();
        let a = frame(5, 6144);
        let b = frame(6, 6144);
        let pa = packets(20, &a, 1458);
        let pb = packets(21, &b, 1458);
        push_all(&mut r, &pa[..2]);
        for p in &pb {
            r.push_packet_at(p.0, &p.1, None, 1_000);
        }
        assert_eq!(r.stats().complete, 1);
        assert_eq!(r.stats().dropped, 1);
        assert_eq!(r.inflight_frames(), 0);
    }

    #[test]
    fn capacity_is_bounded_even_with_many_frame_ids() {
        let mut cfg = config();
        cfg.max_inflight_frames = 2;
        cfg.max_buffered_bytes = 3 * 6144;
        let mut r = Reassembler::new(cfg).unwrap();
        for fid in 100..110u32 {
            let data = frame(fid, 6144);
            let pkts = packets(fid, &data, 1458);
            r.push_packet_at(pkts[0].0, &pkts[0].1, None, 1_000);
            assert!(r.inflight_frames() <= 2);
            assert!(r.inflight_bytes() <= cfg.max_buffered_bytes);
        }
        assert!(r.stats().dropped >= 8);
    }

    #[test]
    fn wrong_geometry_length_and_magic_are_rejected() {
        let mut r = Reassembler::new(config()).unwrap();
        let data = frame(7, 6144);
        let pkts = packets(30, &data, 1458);

        let mut bad_total = pkts[0].0;
        bad_total.total -= 1;
        assert_eq!(
            r.push_packet_at(bad_total, &pkts[0].1, None, 1_000),
            PushOutcome::Ignored(IgnoreReason::BadGeometry)
        );

        let mut bad_len = pkts[0].0;
        bad_len.payload_len -= 1;
        assert_eq!(
            r.push_packet_at(bad_len, &pkts[0].1, None, 1_000),
            PushOutcome::Ignored(IgnoreReason::LengthMismatch)
        );

        let mut bad_magic = pkts[0].0;
        bad_magic.magic = 0x1234;
        assert_eq!(
            r.push_packet_at(bad_magic, &pkts[0].1, None, 1_000),
            PushOutcome::Ignored(IgnoreReason::BadMagic)
        );

        // 捕获长度短于声明载荷：不统计丢包（与 C++ 一致，属于抓包/驱动问题）。
        assert_eq!(
            r.push_packet_at(pkts[0].0, &pkts[0].1[..10], None, 1_000),
            PushOutcome::Ignored(IgnoreReason::TruncatedPayload)
        );
        assert_eq!(r.stats().packets_lost, 2);
        assert_eq!(r.stats().packets_recv, 0);
    }

    #[test]
    fn ethernet_source_mac_is_session_filter() {
        let mac = [2, 0, 0, 0, 0, 9];
        let mut cfg = config();
        cfg.source_mac = Some(mac);
        let mut r = Reassembler::new(cfg).unwrap();
        let data = frame(8, 6144);
        let frag = fragments(
            &data,
            50,
            FragmentConfig {
                payload_max: 1458,
                qpc_ns: 0,
                pattern: 0,
            },
        )
        .unwrap()
        .next()
        .unwrap();
        let mut eth = vec![0u8; 14 + 28 + frag.payload.len()];
        crate::fragment::write_ethernet([9; 6], mac, &frag, &mut eth).unwrap();
        assert!(matches!(
            r.push_ethernet_at(&eth, 1_000),
            PushOutcome::Accepted
        ));

        eth[6] ^= 1; // 源 MAC 不同
        assert_eq!(
            r.push_ethernet_at(&eth, 1_000),
            PushOutcome::Ignored(IgnoreReason::OtherSource)
        );
        assert_eq!(r.stats().ignored_other_source, 1);

        // 已配置源 MAC 时，绕过头解析的调用也不能传 None 混入。
        assert_eq!(
            r.push_packet_at(frag.header, frag.payload, None, 1_000),
            PushOutcome::Ignored(IgnoreReason::OtherSource)
        );

        eth[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        assert_eq!(
            r.push_ethernet_at(&eth, 1_000),
            PushOutcome::Ignored(IgnoreReason::WrongEtherType)
        );
    }

    #[test]
    fn timed_completion_runs_after_the_final_fragment_is_copied() {
        let mut r = Reassembler::new(config()).unwrap();
        let data = frame(9, 6144);
        let pkts = packets(77, &data, 1458);
        let mut completion_calls = 0;
        for (header, payload) in pkts {
            let outcome = r.push_packet_timed(header, &payload, None, 1_000, || {
                completion_calls += 1;
                2_500
            });
            if header.seq + 1 == header.total {
                let PushOutcome::Completed(frame) = outcome else {
                    panic!("last packet must complete frame")
                };
                assert_eq!(frame.first_packet_ns, 1_000);
                assert_eq!(frame.completed_ns, 2_500);
                assert_eq!(frame.data, data);
            }
        }
        assert_eq!(completion_calls, 1);
        assert_eq!(r.rseg_summary().max_ms, 0.0015);
    }

    #[test]
    fn frame_id_wrap_is_treated_as_newer() {
        let mut r = Reassembler::new(config()).unwrap();
        let data = frame(9, 6144);
        let last = packets(u32::MAX, &data, 1458);
        let first = packets(0, &data, 1458);
        push_all(&mut r, &last);
        push_all(&mut r, &first);
        assert_eq!(r.stats().complete, 2);
        assert_eq!(
            r.push_packet_at(last[0].0, &last[0].1, None, 2_000),
            PushOutcome::Ignored(IgnoreReason::Late)
        );
    }

    #[test]
    fn key_frame_flag_is_available_on_fragment() {
        let data = vec![0u8; 100];
        let part = fragments(
            &data,
            0,
            FragmentConfig {
                payload_max: 1458,
                qpc_ns: 0,
                pattern: 4,
            },
        )
        .unwrap()
        .next()
        .unwrap();
        assert_eq!(part.header.flags, FLAG_KEY_FRAME);
        assert_eq!(part.header.magic, MAGIC);
    }

    #[test]
    fn recycle_reuses_buffer_and_lazy_timestamp_runs_once_per_frame() {
        let mut r = Reassembler::new(config()).unwrap();
        let data = frame(13, 6144);
        let pkts = packets(1, &data, 1458);
        let mut first_calls = 0usize;
        let mut ptr_first = std::ptr::null();
        for (i, (h, p)) in pkts.iter().enumerate() {
            let mut eth = vec![0u8; 14 + 28 + p.len()];
            eth[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
            eth[14..42].copy_from_slice(&h.encode());
            eth[42..].copy_from_slice(p);
            let outcome = r.push_ethernet_timed_lazy(
                &eth,
                || {
                    first_calls += 1;
                    1_000
                },
                || 2_000,
            );
            if i + 1 == pkts.len() {
                let PushOutcome::Completed(done) = outcome else {
                    panic!("expected completed frame");
                };
                assert_eq!(done.data, data);
                ptr_first = done.data.as_ptr();
                r.recycle(done.data);
            }
        }
        assert_eq!(first_calls, 1, "首包时间戳只应在建槽时读取一次");

        let data2 = frame(14, 6144);
        let pkts2 = packets(2, &data2, 1458);
        for (i, (h, p)) in pkts2.iter().enumerate() {
            let outcome = r.push_packet_at(*h, p, None, 3_000);
            if i + 1 == pkts2.len() {
                let PushOutcome::Completed(done) = outcome else {
                    panic!("expected second completed frame");
                };
                assert_eq!(done.data, data2);
                assert_eq!(done.data.as_ptr(), ptr_first, "应复用归还的帧缓冲区");
            }
        }
    }

    #[test]
    fn packet_bitset_inline_up_to_512_and_pooled_heap_beyond() {
        let mut pool = Vec::new();
        let mut inline = PacketBitset::take(&mut pool, 422);
        assert!(matches!(inline, PacketBitset::Inline(_)));
        assert!(pool.is_empty());
        for seq in [0u16, 1, 63, 64, 255, 421, 511] {
            assert!(!inline.contains(seq));
            inline.insert(seq);
            assert!(inline.contains(seq));
        }
        inline.recycle(&mut pool, 4);
        assert!(pool.is_empty(), "内联位图不应进堆池");

        let mut heap = PacketBitset::take(&mut pool, 600);
        assert!(matches!(heap, PacketBitset::Heap(_)));
        heap.insert(0);
        heap.insert(599);
        assert!(heap.contains(0) && heap.contains(599) && !heap.contains(598));
        heap.recycle(&mut pool, 4);
        assert_eq!(pool.len(), 1);

        let reused = PacketBitset::take(&mut pool, 550);
        assert!(pool.is_empty(), "应从池中取出已释放的 Vec<u64>");
        assert!(!reused.contains(0) && !reused.contains(549));
    }

    #[test]
    fn mtu1500_422_packet_and_multi_buffer_out_of_order_reassembly() {
        let geom = Geometry::default_product(); // 640x640 -> 614_400 B -> 422 pkts @ MTU 1500
        let cfg = Config::new(geom, 1500).unwrap();
        assert_eq!(cfg.total, 422);
        let mut r = Reassembler::new(cfg).unwrap();

        let f1 = frame(101, geom.frame_bytes());
        let f2 = frame(202, geom.frame_bytes());
        let p1 = packets(1, &f1, cfg.payload_max);
        let p2 = packets(2, &f2, cfg.payload_max);
        assert_eq!(p1.len(), 422);

        let to_eth = |h: &Header, p: &[u8]| {
            let mut eth = vec![0u8; L2_OVERHEAD + p.len()];
            eth[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
            h.encode_into((&mut eth[14..42]).try_into().unwrap());
            eth[42..].copy_from_slice(p);
            eth
        };

        // 交错发 f1 与 f2（多槽位并行），并插一个坏 CRC 与重复包。
        for i in (0..422).rev() {
            if i == 200 {
                let mut bad = to_eth(&p1[i].0, &p1[i].1);
                bad[42] ^= 0xFF;
                assert_eq!(
                    r.push_ethernet_at(&bad, 1_000),
                    PushOutcome::Ignored(IgnoreReason::BadCrc)
                );
            }
            let out2 = r.push_ethernet_at(&to_eth(&p2[i].0, &p2[i].1), 1_000 + i as u64);
            if i > 0 {
                assert_eq!(out2, PushOutcome::Accepted);
            } else {
                // p2 完成会淘汰仍未完成的 p1
                let PushOutcome::Completed(done) = out2 else {
                    panic!("p2 末包应完成帧 2");
                };
                assert_eq!(done.frame_id, 2);
                assert_eq!(done.data, f2);
                r.recycle(done.data);
            }
            if i >= 10 {
                let out1 = r.push_ethernet_at(&to_eth(&p1[i].0, &p1[i].1), 1_000);
                if i == 0 {
                    unreachable!();
                }
                assert_eq!(out1, PushOutcome::Accepted);
                if i == 300 {
                    assert_eq!(
                        r.push_ethernet_at(&to_eth(&p1[i].0, &p1[i].1), 1_000),
                        PushOutcome::Ignored(IgnoreReason::Duplicate)
                    );
                }
            }
        }
        assert_eq!(r.stats().complete, 1);
        assert_eq!(r.stats().dropped, 1);
        assert_eq!(r.stats().crc_errors, 1);
        assert_eq!(r.stats().duplicate, 1);
        assert_eq!(r.inflight_frames(), 0);
        assert_eq!(r.inflight_bytes(), 0);
        assert!(r.free_bitsets.is_empty(), "422 包走内联位图，不分配堆位图");
    }

    #[test]
    fn reassembles_over_512_packets_with_pooled_heap_bitset() {
        let geom = Geometry::new(1280, 720).unwrap(); // 1_382_400 B -> 949 pkts @ MTU 1500
        let cfg = Config::new(geom, 1500).unwrap();
        assert_eq!(cfg.total, 949);
        let mut r = Reassembler::new(cfg).unwrap();

        for fid in 1..=2u32 {
            let data = frame(fid + 50, geom.frame_bytes());
            let pkts = packets(fid, &data, cfg.payload_max);
            assert_eq!(pkts.len(), 949);
            for (idx, (h, p)) in pkts.iter().rev().enumerate() {
                let outcome = r.push_packet_at(*h, p, None, 5_000);
                if idx + 1 == pkts.len() {
                    let PushOutcome::Completed(done) = outcome else {
                        panic!("帧 {fid} 应完成");
                    };
                    assert_eq!(done.data, data);
                    r.recycle(done.data);
                } else {
                    assert_eq!(outcome, PushOutcome::Accepted);
                }
            }
        }
        assert_eq!(r.stats().complete, 2);
        assert_eq!(r.free_bitsets.len(), 1, "超 512 包位图应被池化复用");
    }
}
