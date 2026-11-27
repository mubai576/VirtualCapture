//! 有界、乱序容忍的压缩访问单元（AU）重组。
//!
//! 传输层遵守 `references/encoded-stream.md` 的访问单元契约，只处理冻结的 28B
//! `vc_wire.h` 包头和以太帧，**不解析** 48B VCAU 封装：magic、codec、会话尺寸、
//! frame_id 与整体 payload hash 由调用方在完成帧上校验（VCAU 解析在 `vc-media`）。
//! 本模块保证：
//! - EtherType 与源 MAC 会话过滤、payload CRC32、以及精确分片长度；
//! - 同一 AU 的 `total`/`pattern`/`flags`/`qpc_ns` 必须一致，重复包只在字节完全相同时容忍；
//! - 最多 2 个未完成 AU，每个 payload 缓冲不超过 `max_au_bytes`，另计 seen 位图；
//! - 完成帧按最后一片的实际长度截断，而不是按 `total * payload_max`；
//! - 已完成帧的迟到包不会再被发布；frame_id 用 RFC 1982 序号算术（同 [`crate::wire::is_after`]），
//!   跨 2^32 回绕仍把新帧视为新、旧帧视为旧。
//!
//! 与 C++ 的有意差异：C++ 用 `std::map` 的数值顺序选择最旧槽；这里改用序号算术。
//! [`Stats::complete`] 统计所有完整 AU（调用方自行排除 EOS）；[`Stats::packets_lost`]
//! 同时计入坏包与被丢弃 AU 的缺片，口径与 Raw [`crate::Reassembler`] 一致。
//! [`TimingSummary`] 只使用调用方提供的同机单调纳秒，不跨机比较。

use std::collections::VecDeque;

use crate::checksum::crc32;
use crate::reassembly::{
    CompletedFrame, IgnoreReason, PacketBitset, PushOutcome, Stats, TimingSummary, push_window,
    summarize,
};
use crate::wire::{
    ETHERNET_HEADER_LEN, ETHERTYPE, FLAG_KEY_FRAME, HEADER_LEN, Header, L2_OVERHEAD, MAGIC,
    WireError, is_after, packet_count, payload_max_for_mtu,
};

/// 未完成 AU 上限（编码契约固定为 2）。
const MAX_INFLIGHT_AUS: usize = 2;
/// 首片到达 → AU 完整的最近窗口，与 Raw 相同。
const RSEG_WINDOW: usize = 10_000;
/// 完成 AU 到达间隔的最近窗口，与 Raw 相同。
const INTER_ARRIVAL_WINDOW: usize = 120;

struct Slot {
    frame_id: u32,
    total: u16,
    pattern: u8,
    flags: u8,
    qpc_ns: u64,
    first_ns: u64,
    /// 按 `min(max_au_bytes, total*payload_max)` 分配；完成时截断到实际长度。
    buffer: Vec<u8>,
    /// 紧凑到达位图：<=512 包内联 `[u64; 8]`，>512 包池化 `Vec<u64>`。
    seen: PacketBitset,
    got_n: u16,
    /// 已到达的末片实际长度；仅当末片到达后非零。
    tail: u16,
}

impl Slot {
    #[inline]
    fn allocated_bytes(&self) -> usize {
        self.buffer.len() + (self.total as usize).div_ceil(8)
    }
}

/// 单线程编码 AU 重组器：调用方在同一线程按到达顺序 `push`。
///
/// 与 Raw [`crate::Reassembler`] 不同，这里没有会话几何：AU 长度由外层包头
/// `total`/`payload_len` 描述，上限是构造时的 `max_au_bytes`。VCAU 的 48B
/// 封装属于 AU 字节的一部分，原样重组并交给调用方解析。
pub struct EncodedReassembler {
    payload_max: usize,
    max_au_bytes: usize,
    max_total: u16,
    source_mac: Option<[u8; 6]>,
    slots: Vec<Slot>,
    /// 复用 AU 缓冲区与超长 seen 位图，减少频繁堆分配。
    free_buffers: Vec<Vec<u8>>,
    free_bitsets: Vec<Vec<u64>>,
    buffered_bytes: usize,
    stats: Stats,
    have_seen: bool,
    max_seen_fid: u32,
    have_completed: bool,
    last_complete_fid: u32,
    last_complete_ns: u64,
    rseg_ns: VecDeque<u64>,
    inter_arrival_ns: VecDeque<u64>,
}

const FREE_POOL_CAP: usize = MAX_INFLIGHT_AUS + 2;

impl EncodedReassembler {
    /// `mtu` 决定 `payload_max = mtu - 42`；`max_au_bytes` 含调用方的 48B 封装。
    ///
    /// 拒绝无效 MTU、零 `max_au_bytes`，以及按 `payload_max` 分片会超过 `u16` 包数的
    /// 上限——因此不会仅凭包头声明就分配无界内存。
    pub fn new(
        mtu: usize,
        max_au_bytes: usize,
        source_mac: Option<[u8; 6]>,
    ) -> Result<Self, WireError> {
        let payload_max = payload_max_for_mtu(mtu)?;
        let max_total = packet_count(max_au_bytes, payload_max)?;
        Ok(Self {
            payload_max,
            max_au_bytes,
            max_total,
            source_mac,
            slots: Vec::with_capacity(MAX_INFLIGHT_AUS),
            free_buffers: Vec::with_capacity(FREE_POOL_CAP),
            free_bitsets: Vec::with_capacity(FREE_POOL_CAP),
            buffered_bytes: 0,
            stats: Stats::default(),
            have_seen: false,
            max_seen_fid: 0,
            have_completed: false,
            last_complete_fid: 0,
            last_complete_ns: 0,
            rseg_ns: VecDeque::with_capacity(1024),
            inter_arrival_ns: VecDeque::with_capacity(INTER_ARRIVAL_WINDOW),
        })
    }

    /// 归还已消费的 AU 缓冲区，避免后续帧重复触发堆分配。
    pub fn recycle(&mut self, buffer: Vec<u8>) {
        if self.free_buffers.len() < FREE_POOL_CAP && buffer.capacity() > 0 {
            self.free_buffers.push(buffer);
        }
    }

    fn take_buffer(&mut self, capacity: usize) -> Vec<u8> {
        if let Some(mut buffer) = self.free_buffers.pop() {
            buffer.resize(capacity, 0);
            buffer
        } else {
            vec![0u8; capacity]
        }
    }

    /// 计数器快照。
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// 当前未完成 AU 数（≤2）。
    pub fn inflight_frames(&self) -> usize {
        self.slots.len()
    }

    /// 当前未完成 AU 占用的 payload 缓冲 + seen 位图字节数。
    pub fn inflight_bytes(&self) -> usize {
        self.buffered_bytes
    }

    /// 首片到达 → AU 完成，最近 10000 个完成 AU。
    pub fn rseg_summary(&self) -> TimingSummary {
        summarize(&self.rseg_ns)
    }

    /// 完成 AU 到达间隔，最近 120 个完成 AU。
    pub fn inter_arrival_summary(&self) -> TimingSummary {
        summarize(&self.inter_arrival_ns)
    }

    /// 丢弃全部未完成 AU 并按缺片计账（停止/超时前调用）。
    pub fn finish(&mut self) {
        while !self.slots.is_empty() {
            self.drop_slot(0);
        }
    }

    /// 解析完整以太帧并重组。只向调用方提供一个 AU 字节缓冲；VCAU 解析/整体
    /// hash 校验必须在调用方完成（本模块不依赖 media，避免循环依赖）。
    ///
    /// `arrived_ns` 是首片到达时刻；`completed_at` 只在最后一片通过校验、复制并
    /// 齐套后调用一次，避免把完成时间取在可能阻塞的收包之前。
    pub fn push_ethernet_timed(
        &mut self,
        frame: &[u8],
        arrived_ns: u64,
        completed_at: impl FnOnce() -> u64,
    ) -> PushOutcome {
        self.push_ethernet_timed_lazy(frame, || arrived_ns, completed_at)
    }

    /// 实时接收优化路径：仅在为新 AU 创建槽位时调用 `on_first_packet`。
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
        if let Some(expected) = self.source_mac
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
        self.push_packet_timed_lazy(header, payload, on_first_packet, completed_at)
    }

    #[inline]
    fn push_packet_timed_lazy(
        &mut self,
        header: Header,
        payload: &[u8],
        on_first_packet: impl FnOnce() -> u64,
        completed_at: impl FnOnce() -> u64,
    ) -> PushOutcome {
        if header.magic != MAGIC {
            return PushOutcome::Ignored(IgnoreReason::BadMagic);
        }
        // 先校验所有元数据，再决定是否分配/写入。任何超限声明都不会触发大分配。
        let total = header.total;
        let seq = header.seq;
        let len = header.payload_len as usize;
        if total == 0 || total > self.max_total || seq >= total {
            self.stats.packets_lost += 1;
            return PushOutcome::Ignored(IgnoreReason::BadGeometry);
        }
        // 非末片必须是精确的 payload_max；末片为 1..=payload_max（空片无效）。
        if len == 0 || len > self.payload_max || (seq + 1 < total && len != self.payload_max) {
            self.stats.packets_lost += 1;
            return PushOutcome::Ignored(IgnoreReason::LengthMismatch);
        }
        // `seq < total <= max_total <= u16::MAX` 且 `payload_max <= MAX_MTU`，不会溢出。
        let offset = (seq as usize) * self.payload_max;
        let end = offset + len;
        if end > self.max_au_bytes {
            self.stats.packets_lost += 1;
            return PushOutcome::Ignored(IgnoreReason::BadGeometry);
        }
        // 与 C++ 相同：pattern 0..=4（motion/pattern 源）或 255（DDA）。
        if header.pattern > 4 && header.pattern != 255 {
            self.stats.packets_lost += 1;
            return PushOutcome::Ignored(IgnoreReason::BadGeometry);
        }
        // flags 只保留 bit0（冻结协议的关键帧位），其余位属于无效单元。
        if header.flags & !FLAG_KEY_FRAME != 0 {
            self.stats.packets_lost += 1;
            return PushOutcome::Ignored(IgnoreReason::BadGeometry);
        }
        let Some(payload) = payload.get(..len) else {
            // 抓包截断不是对端坏包，不计丢包（与 Raw 一致）。
            return PushOutcome::Ignored(IgnoreReason::TruncatedPayload);
        };
        if crc32(payload) != header.payload_crc {
            self.stats.packets_lost += 1;
            self.stats.crc_errors += 1;
            return PushOutcome::Ignored(IgnoreReason::BadCrc);
        }
        self.stats.packets_recv += 1;
        self.stats.bytes_recv += (L2_OVERHEAD + len) as u64;

        // 断档检测与 Raw 相同：CRC 通过后、迟到判断前。
        let fid = header.frame_id;
        if !self.have_seen {
            self.have_seen = true;
            self.max_seen_fid = fid;
        } else if is_after(fid, self.max_seen_fid) {
            self.stats.gaps += (fid.wrapping_sub(self.max_seen_fid) - 1) as u64;
            self.max_seen_fid = fid;
        }
        // 已完成帧（含回绕后的“更旧”帧）不得复活。
        if self.have_completed && !is_after(fid, self.last_complete_fid) {
            self.stats.late += 1;
            return PushOutcome::Ignored(IgnoreReason::Late);
        }

        let index = match self.slots.iter().position(|slot| slot.frame_id == fid) {
            Some(index) => index,
            None => {
                // 最多两个未完成 AU；按 RFC 序号淘汰最旧槽。
                while self.slots.len() >= MAX_INFLIGHT_AUS {
                    let oldest = self.oldest_index().expect("槽位非空");
                    self.drop_slot(oldest);
                }
                let capacity = self
                    .max_au_bytes
                    .min((total as usize).saturating_mul(self.payload_max));
                debug_assert!(capacity > 0, "校验后 AU 缓冲必须非空");
                let first_ns = on_first_packet();
                let buffer = self.take_buffer(capacity);
                let seen = PacketBitset::take(&mut self.free_bitsets, total as usize);
                let slot = Slot {
                    frame_id: fid,
                    total,
                    pattern: header.pattern,
                    flags: header.flags,
                    qpc_ns: header.qpc_ns,
                    first_ns,
                    buffer,
                    seen,
                    got_n: 0,
                    tail: 0,
                };
                self.buffered_bytes += slot.allocated_bytes();
                self.slots.push(slot);
                self.slots.len() - 1
            }
        };

        // 同一 AU 的所有分片必须回显相同的 total/pattern/flags/qpc。
        let slot = &mut self.slots[index];
        if slot.total != total
            || slot.pattern != header.pattern
            || slot.flags != header.flags
            || slot.qpc_ns != header.qpc_ns
        {
            self.stats.packets_lost += 1;
            return PushOutcome::Ignored(IgnoreReason::SlotConflict);
        }

        // 重复分片：完全一致才容忍；字节或末片长度冲突则丢整个未完成 AU，
        // 新包不写入（C++ 同样丢弃槽位且不覆盖）。
        if slot.seen.contains(seq) {
            let tail_matches = seq + 1 != total || slot.tail as usize == len;
            let identical = tail_matches && slot.buffer[offset..end] == *payload;
            self.stats.duplicate += 1;
            if identical {
                return PushOutcome::Ignored(IgnoreReason::Duplicate);
            }
            self.stats.packets_lost += 1;
            self.drop_slot(index);
            return PushOutcome::Ignored(IgnoreReason::SlotConflict);
        }

        slot.buffer[offset..end].copy_from_slice(payload);
        slot.seen.insert(seq);
        slot.got_n += 1;
        if seq + 1 == total {
            slot.tail = len as u16;
        }
        if slot.got_n != slot.total {
            return PushOutcome::Accepted;
        }

        // 最后一片已通过 CRC/长度/元数据检查并复制；现在才取完成时间。
        let completed_ns = completed_at();
        let mut slot = self.slots.remove(index);
        self.buffered_bytes = self.buffered_bytes.saturating_sub(slot.allocated_bytes());
        slot.seen.recycle(&mut self.free_bitsets, FREE_POOL_CAP);
        let actual = (slot.total as usize - 1) * self.payload_max + slot.tail as usize;
        debug_assert!(actual <= slot.buffer.len());
        // 变长 AU：返回真实字节数，而不是 total*payload_max 的分配长度。
        slot.buffer.truncate(actual);

        self.stats.complete += 1;
        push_window(
            &mut self.rseg_ns,
            completed_ns.saturating_sub(slot.first_ns),
            RSEG_WINDOW,
        );
        if self.have_completed {
            push_window(
                &mut self.inter_arrival_ns,
                completed_ns.saturating_sub(self.last_complete_ns),
                INTER_ARRIVAL_WINDOW,
            );
        }
        self.have_completed = true;
        self.last_complete_ns = completed_ns;
        let completed_fid = slot.frame_id;
        self.last_complete_fid = completed_fid;

        // 新 AU 已完成：更旧的残帧不会再有意义，按缺片计丢并释放。
        let mut i = 0;
        while i < self.slots.len() {
            if is_after(self.slots[i].frame_id, completed_fid) {
                i += 1;
            } else {
                self.drop_slot(i);
            }
        }

        PushOutcome::Completed(CompletedFrame {
            frame_id: completed_fid,
            pattern: slot.pattern,
            key_frame: slot.flags & FLAG_KEY_FRAME != 0,
            first_packet_ns: slot.first_ns,
            completed_ns,
            data: slot.buffer,
        })
    }

    /// 串行序号意义下最旧的槽：不存在比它更旧的槽（回绕时不能按数值取最小）。
    fn oldest_index(&self) -> Option<usize> {
        (0..self.slots.len()).find(|&i| {
            !self
                .slots
                .iter()
                .enumerate()
                .any(|(j, other)| j != i && is_after(self.slots[i].frame_id, other.frame_id))
        })
    }

    fn drop_slot(&mut self, index: usize) {
        let slot = self.slots.remove(index);
        self.buffered_bytes = self.buffered_bytes.saturating_sub(slot.allocated_bytes());
        self.stats.dropped += 1;
        self.stats.packets_lost += slot.total.wrapping_sub(slot.got_n) as u64;
        if self.free_buffers.len() < FREE_POOL_CAP {
            self.free_buffers.push(slot.buffer);
        }
        slot.seen.recycle(&mut self.free_bitsets, FREE_POOL_CAP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checksum::crc32;
    use crate::fragment::{FragmentConfig, fragments};
    use crate::wire::MAGIC;

    const DST: [u8; 6] = [0x02, 0, 0, 0, 0, 1];
    const SRC: [u8; 6] = [0x02, 0, 0, 0, 0, 2];

    fn pattern(bytes: usize, seed: u32) -> Vec<u8> {
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

    fn packets(
        data: &[u8],
        fid: u32,
        payload_max: usize,
        qpc_ns: u64,
        pattern: u8,
    ) -> Vec<(Header, Vec<u8>)> {
        fragments(
            data,
            fid,
            FragmentConfig {
                payload_max,
                qpc_ns,
                pattern,
            },
        )
        .unwrap()
        .map(|fragment| (fragment.header, fragment.payload.to_vec()))
        .collect()
    }

    fn header(
        fid: u32,
        seq: u16,
        total: u16,
        qpc_ns: u64,
        pattern: u8,
        flags: u8,
        payload: &[u8],
    ) -> Header {
        Header {
            magic: MAGIC,
            frame_id: fid,
            seq,
            total,
            qpc_ns,
            payload_crc: crc32(payload),
            payload_len: payload.len() as u16,
            flags,
            pattern,
        }
    }

    fn ethernet(header: &Header, payload: &[u8], src: [u8; 6]) -> Vec<u8> {
        let mut out = vec![0u8; ETHERNET_HEADER_LEN + HEADER_LEN + payload.len()];
        out[0..6].copy_from_slice(&DST);
        out[6..12].copy_from_slice(&src);
        out[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
        out[14..14 + HEADER_LEN].copy_from_slice(&header.encode());
        out[14 + HEADER_LEN..].copy_from_slice(payload);
        out
    }

    fn push(
        reassembler: &mut EncodedReassembler,
        header: &Header,
        payload: &[u8],
        arrived_ns: u64,
    ) -> PushOutcome {
        let frame = ethernet(header, payload, SRC);
        reassembler.push_ethernet_timed(&frame, arrived_ns, || arrived_ns + 1_500)
    }

    #[test]
    fn reassembles_reverse_order_with_identical_duplicate() {
        let mut r = EncodedReassembler::new(1500, 16 * 1024, None).unwrap();
        let data = pattern(6000, 1);
        let pkts = packets(&data, 41, 1458, 7, 3);
        assert_eq!(pkts.len(), 5);

        assert_eq!(
            push(&mut r, &pkts[4].0, &pkts[4].1, 1_000),
            PushOutcome::Accepted
        );
        assert_eq!(
            push(&mut r, &pkts[3].0, &pkts[3].1, 1_100),
            PushOutcome::Accepted
        );
        assert_eq!(
            push(&mut r, &pkts[3].0, &pkts[3].1, 1_200),
            PushOutcome::Ignored(IgnoreReason::Duplicate)
        );
        assert_eq!(
            push(&mut r, &pkts[2].0, &pkts[2].1, 1_300),
            PushOutcome::Accepted
        );
        assert_eq!(
            push(&mut r, &pkts[1].0, &pkts[1].1, 1_400),
            PushOutcome::Accepted
        );
        let outcome =
            r.push_ethernet_timed(&ethernet(&pkts[0].0, &pkts[0].1, SRC), 1_500, || 2_500);
        let PushOutcome::Completed(frame) = outcome else {
            panic!("末片应完成 AU");
        };
        assert_eq!(frame.frame_id, 41);
        assert_eq!(frame.pattern, 3);
        assert!(!frame.key_frame);
        assert_eq!(frame.first_packet_ns, 1_000);
        assert_eq!(frame.completed_ns, 2_500);
        assert_eq!(frame.data, data);
        assert_eq!(r.stats().complete, 1);
        assert_eq!(r.stats().duplicate, 1);
        assert_eq!(r.stats().packets_recv, 6);
        assert_eq!(r.stats().packets_lost, 0);
        assert_eq!(r.stats().dropped, 0);
        assert_eq!(r.inflight_frames(), 0);
        assert_eq!(r.inflight_bytes(), 0);
        assert_eq!(r.rseg_summary().samples, 1);
        assert_eq!(r.rseg_summary().max_ms, 0.0015);
    }

    #[test]
    fn timing_summaries_follow_arrival_and_completion_ns() {
        let mut r = EncodedReassembler::new(1500, 4096, None).unwrap();
        let data = pattern(2000, 2);

        let first = packets(&data, 20, 1458, 1, 0);
        assert_eq!(
            r.push_ethernet_timed(&ethernet(&first[0].0, &first[0].1, SRC), 1_000, || 2_500),
            PushOutcome::Accepted
        );
        assert!(matches!(
            r.push_ethernet_timed(&ethernet(&first[1].0, &first[1].1, SRC), 1_100, || 2_500),
            PushOutcome::Completed(_)
        ));

        let second = packets(&data, 21, 1458, 1, 0);
        assert_eq!(
            r.push_ethernet_timed(&ethernet(&second[0].0, &second[0].1, SRC), 3_000, || 4_000),
            PushOutcome::Accepted
        );
        assert!(matches!(
            r.push_ethernet_timed(&ethernet(&second[1].0, &second[1].1, SRC), 3_100, || 4_000),
            PushOutcome::Completed(_)
        ));

        let rseg = r.rseg_summary();
        assert_eq!(rseg.samples, 2);
        assert_eq!(rseg.max_ms, 0.0015);
        let inter = r.inter_arrival_summary();
        assert_eq!(inter.samples, 1);
        assert_eq!(inter.max_ms, 0.0015);
    }

    #[test]
    fn conflicting_duplicate_drops_slot_and_discards_new_packet() {
        let mut r = EncodedReassembler::new(1500, 16 * 1024, None).unwrap();
        let data = pattern(6000, 3);
        let pkts = packets(&data, 41, 1458, 7, 0);
        assert_eq!(
            push(&mut r, &pkts[0].0, &pkts[0].1, 1_000),
            PushOutcome::Accepted
        );
        assert_eq!(r.inflight_frames(), 1);

        let mut conflicting = pkts[0].1.clone();
        conflicting[0] ^= 0xFF;
        let mut conflict_header = pkts[0].0;
        conflict_header.payload_crc = crc32(&conflicting);
        assert_eq!(
            push(&mut r, &conflict_header, &conflicting, 1_100),
            PushOutcome::Ignored(IgnoreReason::SlotConflict)
        );
        assert_eq!(r.stats().duplicate, 1);
        assert_eq!(r.stats().dropped, 1);
        // 坏重复包 + 被丢弃 AU 的 4 个缺片。
        assert_eq!(r.stats().packets_lost, 5);
        assert_eq!(r.inflight_frames(), 0);
        assert_eq!(r.inflight_bytes(), 0);

        // 槽位已丢弃；完整重放原始分片仍能完成原始字节。
        for (index, (h, payload)) in pkts.iter().enumerate() {
            let outcome = push(&mut r, h, payload, 2_000 + index as u64);
            if index + 1 == pkts.len() {
                let PushOutcome::Completed(frame) = outcome else {
                    panic!("重放应完成 AU");
                };
                assert_eq!(frame.data, data);
            }
        }
        assert_eq!(r.stats().complete, 1);
    }

    #[test]
    fn final_fragment_first_tracks_tail_and_truncates() {
        let mut r = EncodedReassembler::new(1500, 16 * 1024, None).unwrap();
        let data = pattern(6000, 4);
        let pkts = packets(&data, 42, 1458, 5, 0);
        let final_index = pkts.len() - 1;
        assert_eq!(pkts[final_index].1.len(), 168);

        // 末片先到：槽位分配 total*payload，但完成时按 tail 截断。
        assert_eq!(
            push(&mut r, &pkts[final_index].0, &pkts[final_index].1, 900),
            PushOutcome::Accepted
        );
        // 末片相同重复可容忍。
        assert_eq!(
            push(&mut r, &pkts[final_index].0, &pkts[final_index].1, 950),
            PushOutcome::Ignored(IgnoreReason::Duplicate)
        );
        for index in [2usize, 3, 1] {
            assert_eq!(
                push(&mut r, &pkts[index].0, &pkts[index].1, 1_000 + index as u64),
                PushOutcome::Accepted
            );
        }
        let PushOutcome::Completed(frame) = push(&mut r, &pkts[0].0, &pkts[0].1, 1_100) else {
            panic!("补齐后应完成 AU");
        };
        assert_eq!(frame.first_packet_ns, 900);
        assert_eq!(frame.data.len(), 6000);
        assert_eq!(frame.data, data);

        // 末片长度不同的重复：丢弃槽，新包不覆盖。
        let mut r = EncodedReassembler::new(1500, 4096, None).unwrap();
        let tail = vec![0xAB; 100];
        let tail_header = header(43, 1, 2, 1, 0, 0, &tail);
        assert_eq!(
            push(&mut r, &tail_header, &tail, 1_000),
            PushOutcome::Accepted
        );
        let shorter = vec![0xAB; 80];
        let shorter_header = header(43, 1, 2, 1, 0, 0, &shorter);
        assert_eq!(
            push(&mut r, &shorter_header, &shorter, 1_100),
            PushOutcome::Ignored(IgnoreReason::SlotConflict)
        );
        assert_eq!(r.stats().duplicate, 1);
        assert_eq!(r.stats().dropped, 1);
        assert_eq!(r.inflight_frames(), 0);
    }

    #[test]
    fn rejects_oversized_and_malformed_metadata_without_buffering() {
        let mut r = EncodedReassembler::new(1500, 3000, None).unwrap();
        let unit = vec![0x5A; 1458];

        let first = header(9, 0, 3, 1, 0, 0, &unit);
        assert_eq!(push(&mut r, &first, &unit, 1_000), PushOutcome::Accepted);
        let second = header(9, 1, 3, 1, 0, 0, &unit);
        assert_eq!(push(&mut r, &second, &unit, 1_100), PushOutcome::Accepted);
        let frames_before = r.inflight_frames();
        let bytes_before = r.inflight_bytes();

        // total*payload 本身合法，但末片 offset+len 超过 max_au_bytes。
        let tail = vec![1u8; 200];
        let oversized_tail = header(9, 2, 3, 1, 0, 0, &tail);
        assert_eq!(
            push(&mut r, &oversized_tail, &tail, 1_200),
            PushOutcome::Ignored(IgnoreReason::BadGeometry)
        );
        assert_eq!(r.inflight_frames(), frames_before);
        assert_eq!(r.inflight_bytes(), bytes_before);

        let cases = [
            // total=0（同时 seq 越界）
            (header(10, 0, 0, 1, 0, 0, &unit), IgnoreReason::BadGeometry),
            // total 超过 max_au_bytes 允许的包数（3000/1458 -> 3）
            (header(10, 0, 4, 1, 0, 0, &unit), IgnoreReason::BadGeometry),
            // seq >= total（序号越界）
            (header(10, 3, 3, 1, 0, 0, &unit), IgnoreReason::BadGeometry),
            // 非末片长度不是 payload_max
            (
                header(10, 0, 2, 1, 0, 0, &unit[..100]),
                IgnoreReason::LengthMismatch,
            ),
            // 末片空载荷
            (header(10, 0, 1, 1, 0, 0, &[]), IgnoreReason::LengthMismatch),
            // 单包声明超过 payload_max
            (
                header(10, 0, 1, 1, 0, 0, &vec![0u8; 1459]),
                IgnoreReason::LengthMismatch,
            ),
            // pattern 只允许 0..=4 或 255
            (
                header(10, 0, 1, 1, 5, 0, &unit[..100]),
                IgnoreReason::BadGeometry,
            ),
            // flags 只允许 bit0
            (
                header(10, 0, 1, 1, 0, 2, &unit[..100]),
                IgnoreReason::BadGeometry,
            ),
        ];
        for (bad_header, reason) in cases {
            let payload_len = bad_header.payload_len as usize;
            let payload = vec![0x5A; payload_len];
            // 空载荷 case 用空切片；其余内容不影响元数据拒绝。
            let payload = if payload_len == 0 {
                &[][..]
            } else {
                &payload[..]
            };
            assert_eq!(
                push(&mut r, &bad_header, payload, 2_000),
                PushOutcome::Ignored(reason)
            );
        }
        assert_eq!(r.inflight_frames(), frames_before);
        assert_eq!(r.inflight_bytes(), bytes_before);
        // 1 个越界末片 + 8 个坏元数据包。
        assert_eq!(r.stats().packets_lost, 9);
    }

    #[test]
    fn filters_source_ethertype_magic_crc_and_truncation() {
        let mac = [2, 0, 0, 0, 0, 9];
        let mut r = EncodedReassembler::new(1500, 4096, Some(mac)).unwrap();
        let data = pattern(100, 7);
        let good_header = header(3, 0, 1, 5, 0, 0, &data);
        let good = ethernet(&good_header, &data, mac);

        let mut wrong_source = good.clone();
        wrong_source[6] ^= 1;
        assert_eq!(
            r.push_ethernet_timed(&wrong_source, 1, || 2),
            PushOutcome::Ignored(IgnoreReason::OtherSource)
        );
        assert_eq!(r.stats().ignored_other_source, 1);

        let mut wrong_ethertype = good.clone();
        wrong_ethertype[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        assert_eq!(
            r.push_ethernet_timed(&wrong_ethertype, 1, || 2),
            PushOutcome::Ignored(IgnoreReason::WrongEtherType)
        );
        assert_eq!(
            r.push_ethernet_timed(&good[..41], 1, || 2),
            PushOutcome::Ignored(IgnoreReason::ShortFrame)
        );

        let mut bad_magic = good.clone();
        bad_magic[ETHERNET_HEADER_LEN] = 0;
        assert_eq!(
            r.push_ethernet_timed(&bad_magic, 1, || 2),
            PushOutcome::Ignored(IgnoreReason::BadMagic)
        );
        assert_eq!(r.stats().packets_lost, 0);

        let mut bad_crc = good.clone();
        bad_crc[ETHERNET_HEADER_LEN + 20] ^= 1;
        assert_eq!(
            r.push_ethernet_timed(&bad_crc, 1, || 2),
            PushOutcome::Ignored(IgnoreReason::BadCrc)
        );
        assert_eq!(r.stats().crc_errors, 1);
        assert_eq!(r.stats().packets_lost, 1);

        let truncated = &good[..ETHERNET_HEADER_LEN + HEADER_LEN + 10];
        assert_eq!(
            r.push_ethernet_timed(truncated, 1, || 2),
            PushOutcome::Ignored(IgnoreReason::TruncatedPayload)
        );
        assert_eq!(r.stats().packets_recv, 0);

        let PushOutcome::Completed(frame) = r.push_ethernet_timed(&good, 1, || 2) else {
            panic!("单包 AU 应完成");
        };
        assert_eq!(frame.data, data);
        assert_eq!(r.stats().packets_recv, 1);
        assert_eq!(r.stats().bytes_recv, (L2_OVERHEAD + data.len()) as u64);
    }

    #[test]
    fn rfc_serial_order_handles_wrap_and_rejects_resurrection() {
        let mut r = EncodedReassembler::new(1500, 4096, None).unwrap();
        let data = pattern(2000, 8);
        let last = packets(&data, u32::MAX, 1458, 1, 0);
        let zero = packets(&data, 0, 1458, 1, 0);

        assert_eq!(
            push(&mut r, &last[0].0, &last[0].1, 1_000),
            PushOutcome::Accepted
        );
        assert!(matches!(
            push(&mut r, &last[1].0, &last[1].1, 1_100),
            PushOutcome::Completed(_)
        ));
        assert_eq!(
            push(&mut r, &zero[0].0, &zero[0].1, 1_200),
            PushOutcome::Accepted
        );
        assert!(matches!(
            push(&mut r, &zero[1].0, &zero[1].1, 1_300),
            PushOutcome::Completed(_)
        ));
        // MAX -> 0 是相邻序号，不产生 gap。
        assert_eq!(r.stats().gaps, 0);
        assert_eq!(r.stats().complete, 2);

        // 已完成 u32::MAX 的迟到包在回绕后仍被判旧。
        assert_eq!(
            push(&mut r, &last[0].0, &last[0].1, 1_400),
            PushOutcome::Ignored(IgnoreReason::Late)
        );

        // 完成新帧时丢弃更旧的未完成槽；其剩余分片不得复活。
        let mut s = EncodedReassembler::new(1500, 4096, None).unwrap();
        let old = packets(&data, 30, 1458, 1, 0);
        let new = packets(&data, 31, 1458, 1, 0);
        assert_eq!(
            push(&mut s, &old[0].0, &old[0].1, 1_000),
            PushOutcome::Accepted
        );
        assert_eq!(
            push(&mut s, &new[0].0, &new[0].1, 1_100),
            PushOutcome::Accepted
        );
        assert!(matches!(
            push(&mut s, &new[1].0, &new[1].1, 1_200),
            PushOutcome::Completed(_)
        ));
        assert_eq!(s.stats().dropped, 1);
        assert_eq!(
            push(&mut s, &old[1].0, &old[1].1, 1_300),
            PushOutcome::Ignored(IgnoreReason::Late)
        );
    }

    #[test]
    fn oldest_slot_eviction_uses_serial_order_across_wrap() {
        let mut r = EncodedReassembler::new(1500, 4096, None).unwrap();
        let data = pattern(2000, 9);
        let max = packets(&data, u32::MAX, 1458, 1, 0);
        let zero = packets(&data, 0, 1458, 1, 0);
        let one = packets(&data, 1, 1458, 1, 0);

        assert_eq!(
            push(&mut r, &max[0].0, &max[0].1, 1_000),
            PushOutcome::Accepted
        );
        assert_eq!(
            push(&mut r, &zero[0].0, &zero[0].1, 1_100),
            PushOutcome::Accepted
        );
        // 第三个 AU：序号意义下 u32::MAX 比 0/1 更旧，应淘汰它而不是数值最小者。
        assert_eq!(
            push(&mut r, &one[0].0, &one[0].1, 1_200),
            PushOutcome::Accepted
        );
        assert_eq!(r.inflight_frames(), 2);
        assert_eq!(r.stats().dropped, 1);

        assert!(matches!(
            push(&mut r, &zero[1].0, &zero[1].1, 1_300),
            PushOutcome::Completed(_)
        ));
        assert!(matches!(
            push(&mut r, &one[1].0, &one[1].1, 1_400),
            PushOutcome::Completed(_)
        ));
        assert_eq!(
            push(&mut r, &max[1].0, &max[1].1, 1_500),
            PushOutcome::Ignored(IgnoreReason::Late)
        );
    }

    #[test]
    fn per_au_metadata_conflicts_do_not_replace_slot() {
        let mut r = EncodedReassembler::new(1500, 4096, None).unwrap();
        let data = pattern(2000, 11);
        let pkts = packets(&data, 50, 1458, 7, 3);
        assert_eq!(
            push(&mut r, &pkts[0].0, &pkts[0].1, 1_000),
            PushOutcome::Accepted
        );

        let mut bad_qpc = pkts[0].0;
        bad_qpc.qpc_ns = 8;
        assert_eq!(
            push(&mut r, &bad_qpc, &pkts[0].1, 1_100),
            PushOutcome::Ignored(IgnoreReason::SlotConflict)
        );
        let mut bad_pattern = pkts[0].0;
        bad_pattern.pattern = 2;
        assert_eq!(
            push(&mut r, &bad_pattern, &pkts[0].1, 1_200),
            PushOutcome::Ignored(IgnoreReason::SlotConflict)
        );
        let mut bad_flags = pkts[0].0;
        bad_flags.flags = 1;
        assert_eq!(
            push(&mut r, &bad_flags, &pkts[0].1, 1_300),
            PushOutcome::Ignored(IgnoreReason::SlotConflict)
        );
        // seq0 非末片长度合法，但 total 与本槽不同。
        let mut bad_total = pkts[0].0;
        bad_total.total = 3;
        assert_eq!(
            push(&mut r, &bad_total, &pkts[0].1, 1_400),
            PushOutcome::Ignored(IgnoreReason::SlotConflict)
        );
        assert_eq!(r.inflight_frames(), 1);
        assert_eq!(r.stats().packets_lost, 4);

        let PushOutcome::Completed(frame) = push(&mut r, &pkts[1].0, &pkts[1].1, 1_500) else {
            panic!("原始元数据仍应完成 AU");
        };
        assert_eq!(frame.pattern, 3);
        assert!(!frame.key_frame);
        assert_eq!(frame.data, data);
        assert_eq!(r.stats().complete, 1);
    }

    #[test]
    fn inflight_is_bounded_to_two_aus_and_counts_tracking_bytes() {
        let mut r = EncodedReassembler::new(1500, 4096, None).unwrap();
        let data = pattern(3000, 10);
        for fid in 10..13u32 {
            let pkts = packets(&data, fid, 1458, 1, 0);
            assert_eq!(pkts.len(), 3);
            assert_eq!(
                push(&mut r, &pkts[0].0, &pkts[0].1, 1_000),
                PushOutcome::Accepted
            );
        }
        // total=3 -> 分配 min(4096, 4374)=4096 字节 + 1 字节 seen。
        assert_eq!(r.inflight_frames(), 2);
        assert_eq!(r.inflight_bytes(), 2 * (4096 + 1));
        assert_eq!(r.stats().dropped, 1);
        assert_eq!(r.stats().packets_lost, 2);

        r.finish();
        assert_eq!(r.inflight_frames(), 0);
        assert_eq!(r.inflight_bytes(), 0);
        assert_eq!(r.stats().dropped, 3);
        assert_eq!(r.stats().packets_lost, 6);
    }

    #[test]
    fn lazy_timestamp_runs_once_and_recycle_reuses_capacity() {
        let mut r = EncodedReassembler::new(1500, 16 * 1024, None).unwrap();
        let data = pattern(6000, 9);
        let pkts = packets(&data, 1, 1458, 7, 0);
        let mut first_calls = 0usize;
        let mut ptr_first = std::ptr::null();
        for (i, (h, p)) in pkts.iter().enumerate() {
            let frame = ethernet(h, p, SRC);
            let outcome = r.push_ethernet_timed_lazy(
                &frame,
                || {
                    first_calls += 1;
                    1_000
                },
                || 2_000,
            );
            if i + 1 == pkts.len() {
                let PushOutcome::Completed(done) = outcome else {
                    panic!("expected completed AU");
                };
                assert_eq!(done.data, data);
                ptr_first = done.data.as_ptr();
                r.recycle(done.data);
            }
        }
        assert_eq!(first_calls, 1);
        let data2 = pattern(5000, 10);
        let pkts2 = packets(&data2, 2, 1458, 8, 0);
        for (i, (h, p)) in pkts2.iter().enumerate() {
            let outcome = push(&mut r, h, p, 3_000);
            if i + 1 == pkts2.len() {
                let PushOutcome::Completed(done) = outcome else {
                    panic!("expected second AU");
                };
                assert_eq!(done.data, data2);
                assert_eq!(done.data.as_ptr(), ptr_first);
            }
        }
    }

    #[test]
    fn reassembles_422_and_over_512_packet_aus_with_compact_bitset() {
        // 422 包（inline [u64; 8]）：614_000 字节 @ MTU 1500 -> 422 包
        let mut r = EncodedReassembler::new(1500, 700_000, None).unwrap();
        let data_422 = pattern(614_000, 31);
        let pkts_422 = packets(&data_422, 10, 1458, 99, 0);
        assert_eq!(pkts_422.len(), 422);
        for (idx, (h, p)) in pkts_422.iter().rev().enumerate() {
            let outcome = push(&mut r, h, p, 1_000 + idx as u64);
            if idx + 1 == pkts_422.len() {
                let PushOutcome::Completed(done) = outcome else {
                    panic!("422 包 AU 应完成");
                };
                assert_eq!(done.data, data_422);
            } else {
                assert_eq!(outcome, PushOutcome::Accepted);
            }
        }
        assert!(r.free_bitsets.is_empty(), "422 包 AU 不应分配堆位图");

        // >512 包（pooled Vec<u64>）：874_000 字节 @ MTU 1500 -> 600 包
        let mut r_big = EncodedReassembler::new(1500, 900_000, None).unwrap();
        for fid in 1..=2u32 {
            let data = pattern(874_000, fid + 40);
            let pkts = packets(&data, fid, 1458, 123, 0);
            assert_eq!(pkts.len(), 600);
            for (idx, (h, p)) in pkts.iter().rev().enumerate() {
                let outcome = push(&mut r_big, h, p, 2_000 + idx as u64);
                if idx + 1 == pkts.len() {
                    let PushOutcome::Completed(done) = outcome else {
                        panic!("600 包 AU 应完成");
                    };
                    assert_eq!(done.data, data);
                    r_big.recycle(done.data);
                } else {
                    assert_eq!(outcome, PushOutcome::Accepted);
                }
            }
        }
        assert_eq!(r_big.stats().complete, 2);
        assert_eq!(r_big.free_bitsets.len(), 1, "超 512 包位图应被池化复用");
    }
}
