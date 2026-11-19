//! Raw NV12 分片：与 `sender/vc_sender.cpp` 相同的切包参数和包头字段。
//!
//! 发送端真正的网卡写入仍由现有 C++ 实现负责；本模块只生成可交给任何 L2 发送 API
//! （未来 Npcap 或测试桩）的包头/载荷。分片不复制帧数据：`Fragment::payload` 是原帧切片。
//! `flags` 在 `frame_id == 0` 时置关键帧位，与现有 sender 一致。

use crate::checksum::crc32;
use crate::wire::{
    ETHERNET_HEADER_LEN, ETHERTYPE, FLAG_KEY_FRAME, HEADER_LEN, Header, MAGIC, WireError,
    packet_count, payload_len_at,
};

/// 分片参数。`payload_max` 来自 `mtu - 42`，其余三个字段由会话/发送循环提供。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FragmentConfig {
    pub payload_max: usize,
    pub qpc_ns: u64,
    pub pattern: u8,
}

/// 单包：已填好的包头 + 原帧切片。
#[derive(Clone, Copy, Debug)]
pub struct Fragment<'a> {
    pub header: Header,
    pub payload: &'a [u8],
}

/// 按 `seq` 顺序产生一帧的全部分片。
#[derive(Clone, Debug)]
pub struct FragmentIter<'a> {
    frame: &'a [u8],
    frame_id: u32,
    config: FragmentConfig,
    total: u16,
    seq: u16,
}

impl<'a> Iterator for FragmentIter<'a> {
    type Item = Fragment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.seq >= self.total {
            return None;
        }
        let seq = self.seq;
        self.seq += 1;
        let offset = seq as usize * self.config.payload_max;
        let len = payload_len_at(seq, self.total, self.frame.len(), self.config.payload_max);
        let payload = &self.frame[offset..offset + len];
        let header = Header {
            magic: MAGIC,
            frame_id: self.frame_id,
            seq,
            total: self.total,
            qpc_ns: self.config.qpc_ns,
            payload_crc: crc32(payload),
            payload_len: len as u16,
            flags: if self.frame_id == 0 {
                FLAG_KEY_FRAME
            } else {
                0
            },
            pattern: self.config.pattern,
        };
        Some(Fragment { header, payload })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.total - self.seq) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for FragmentIter<'_> {}

/// 为一帧建立分片迭代器。空帧、`payload_max == 0` 或包数超过 65535 时拒绝。
pub fn fragments<'a>(
    frame: &'a [u8],
    frame_id: u32,
    config: FragmentConfig,
) -> Result<FragmentIter<'a>, WireError> {
    let total = packet_count(frame.len(), config.payload_max)?;
    Ok(FragmentIter {
        frame,
        frame_id,
        config,
        total,
        seq: 0,
    })
}

/// 把分片写成完整以太帧：`dst(6) src(6) EtherType(2) VC头(28) payload`。
///
/// 返回写入的字节数；`out` 不足时返回 `Err(needed)`。MAC 与 EtherType 属于调用方会话设置，
/// 不经控制面确认前不要自行更改。
pub fn write_ethernet(
    dst: [u8; 6],
    src: [u8; 6],
    fragment: &Fragment<'_>,
    out: &mut [u8],
) -> Result<usize, usize> {
    let needed = ETHERNET_HEADER_LEN + HEADER_LEN + fragment.payload.len();
    if out.len() < needed {
        return Err(needed);
    }
    out[0..6].copy_from_slice(&dst);
    out[6..12].copy_from_slice(&src);
    out[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
    out[14..14 + HEADER_LEN].copy_from_slice(&fragment.header.encode());
    out[14 + HEADER_LEN..needed].copy_from_slice(fragment.payload);
    Ok(needed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Geometry, payload_max_for_mtu};

    fn frame(bytes: usize) -> Vec<u8> {
        (0..bytes).map(|i| (i * 37 % 251) as u8).collect()
    }

    #[test]
    fn fragment_count_and_lengths_match_contract() {
        let geometry = Geometry::default_product();
        let payload_max = payload_max_for_mtu(9000).unwrap();
        let data = frame(geometry.frame_bytes());
        let config = FragmentConfig {
            payload_max,
            qpc_ns: 42,
            pattern: 4,
        };
        let parts: Vec<_> = fragments(&data, 0, config).unwrap().collect();
        assert_eq!(parts.len(), 69);
        assert_eq!(parts[0].payload.len(), 8958);
        assert_eq!(parts[68].payload.len(), 5256);
        assert_eq!(parts[0].header.total, 69);
        assert!(parts[0].header.is_key_frame());
        // sender 在 frame_id==0 的**每一包**都置位，与 seq 无关。
        assert!(parts[1].header.is_key_frame());

        // 每包载荷连续覆盖整帧，CRC 即载荷 CRC。
        let mut joined = Vec::new();
        for (seq, part) in parts.iter().enumerate() {
            assert_eq!(part.header.seq as usize, seq);
            assert_eq!(part.header.payload_len as usize, part.payload.len());
            assert_eq!(part.header.payload_crc, crc32(part.payload));
            joined.extend_from_slice(part.payload);
        }
        assert_eq!(joined, data);
    }

    #[test]
    fn mtu1500_fragments_to_422_packets() {
        let payload_max = payload_max_for_mtu(1500).unwrap();
        let data = frame(614_400);
        let parts = fragments(
            &data,
            9,
            FragmentConfig {
                payload_max,
                qpc_ns: 0,
                pattern: 2,
            },
        )
        .unwrap()
        .count();
        assert_eq!(parts, 422);
    }

    #[test]
    fn ethernet_frame_layout_is_exact() {
        let data = frame(3000);
        let fragment = fragments(
            &data,
            7,
            FragmentConfig {
                payload_max: 1458,
                qpc_ns: 1,
                pattern: 1,
            },
        )
        .unwrap()
        .next()
        .unwrap();
        let mut out = vec![0u8; 2048];
        let written = write_ethernet([1; 6], [2; 6], &fragment, &mut out).expect("足够空间");
        assert_eq!(written, 14 + 28 + 1458);
        assert_eq!(&out[0..6], &[1; 6]);
        assert_eq!(&out[6..12], &[2; 6]);
        assert_eq!(&out[12..14], &ETHERTYPE.to_be_bytes());
        assert_eq!(&out[14..42], &fragment.header.encode());
        assert_eq!(&out[42..written], fragment.payload);
        assert!(write_ethernet([0; 6], [0; 6], &fragment, &mut out[..100]).is_err());
    }

    #[test]
    fn rejects_empty_or_oversized_fragmentation() {
        let config = FragmentConfig {
            payload_max: 100,
            qpc_ns: 0,
            pattern: 0,
        };
        assert!(fragments(&[], 0, config).is_err());
        let oversized = vec![0u8; 65536 * 100];
        assert!(fragments(&oversized, 0, config).is_err());
    }
}
