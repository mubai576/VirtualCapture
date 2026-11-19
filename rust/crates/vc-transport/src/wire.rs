//! 冻结的 28 字节包头与 Raw NV12 会话几何。
//!
//! 字节布局来自 `proto/vc_wire.h`（pack(1)，小端主机）。本文件只做编解码和校验，
//! 不发明字段；`magic`、字段顺序、`payload_len` 语义都与 C++ 完全一致。
//! MTU、宽高、fps、源 MAC 属于控制面，不在包头内。

/// 包头长度（冻结）。
pub const HEADER_LEN: usize = 28;
/// 以太网头长度（目的/源 MAC + EtherType）。
pub const ETHERNET_HEADER_LEN: usize = 14;
/// 链路层 EtherType。
pub const ETHERTYPE: u16 = 0x88B5;
/// 魔数 `VCAP`。
pub const MAGIC: u32 = 0x5643_4150;
/// 会话最大边（与 `VCStream::valid` 一致）。
pub const MAX_DIMENSION: u32 = 8192;
/// 最大 MTU（`VC_MAX_JUMBO`）。
pub const MAX_MTU: usize = 9000;
/// 每包链路层额外开销：以太头 14 + VC 头 28。
pub const L2_OVERHEAD: usize = ETHERNET_HEADER_LEN + HEADER_LEN;

/// `flags` bit0：关键帧（发送端只在 `frame_id == 0` 置位）。
pub const FLAG_KEY_FRAME: u8 = 0x01;

/// 线格式错误。只在调用方需要区分原因时返回；重组热路径把它们折算为计数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// 输入不足以容纳要求的字段。
    TooShort { needed: usize, got: usize },
    /// 以太网 EtherType 不是 0x88B5。
    BadEtherType(u16),
    /// 包头 magic 不是 VCAP。
    BadMagic(u32),
    /// 宽高不满足偶数、范围或 NV12 约束。
    BadGeometry { width: u32, height: u32 },
    /// MTU 无法得到有效载荷，或帧需要超过 65535 个包。
    BadMtu(usize),
    /// 分片参数无效（空帧或包数溢出）。
    BadFragment { bytes: usize, payload_max: usize },
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort { needed, got } => {
                write!(f, "wire 数据不足：需要 {needed} 字节，实际 {got}")
            }
            Self::BadEtherType(value) => write!(f, "EtherType 不是 0x88B5：0x{value:04x}"),
            Self::BadMagic(value) => write!(f, "包头 magic 不是 VCAP：0x{value:08x}"),
            Self::BadGeometry { width, height } => {
                write!(f, "无效 NV12 会话尺寸：{width}×{height}")
            }
            Self::BadMtu(mtu) => write!(f, "无效 MTU：{mtu}"),
            Self::BadFragment { bytes, payload_max } => {
                write!(f, "无法分片：{bytes} 字节，payload_max={payload_max}")
            }
        }
    }
}

impl std::error::Error for WireError {}

/// 冻结的包头条目。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub magic: u32,
    pub frame_id: u32,
    pub seq: u16,
    pub total: u16,
    pub qpc_ns: u64,
    pub payload_crc: u32,
    pub payload_len: u16,
    pub flags: u8,
    pub pattern: u8,
}

impl Header {
    /// 按 `vc_pkt_hdr_t` 逐字段小端编码为 28 字节。
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..8].copy_from_slice(&self.frame_id.to_le_bytes());
        out[8..10].copy_from_slice(&self.seq.to_le_bytes());
        out[10..12].copy_from_slice(&self.total.to_le_bytes());
        out[12..20].copy_from_slice(&self.qpc_ns.to_le_bytes());
        out[20..24].copy_from_slice(&self.payload_crc.to_le_bytes());
        out[24..26].copy_from_slice(&self.payload_len.to_le_bytes());
        out[26] = self.flags;
        out[27] = self.pattern;
        out
    }

    /// 从至少 28 字节的缓冲区解析；额外字节属于载荷，不在这里读取。
    pub fn parse(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < HEADER_LEN {
            return Err(WireError::TooShort {
                needed: HEADER_LEN,
                got: bytes.len(),
            });
        }
        let out = Self {
            magic: u32::from_le_bytes(bytes[0..4].try_into().expect("4 字节")),
            frame_id: u32::from_le_bytes(bytes[4..8].try_into().expect("4 字节")),
            seq: u16::from_le_bytes(bytes[8..10].try_into().expect("2 字节")),
            total: u16::from_le_bytes(bytes[10..12].try_into().expect("2 字节")),
            qpc_ns: u64::from_le_bytes(bytes[12..20].try_into().expect("8 字节")),
            payload_crc: u32::from_le_bytes(bytes[20..24].try_into().expect("4 字节")),
            payload_len: u16::from_le_bytes(bytes[24..26].try_into().expect("2 字节")),
            flags: bytes[26],
            pattern: bytes[27],
        };
        if out.magic != MAGIC {
            return Err(WireError::BadMagic(out.magic));
        }
        Ok(out)
    }

    /// `flags` bit0 是否置位（关键帧/pattern 起点）。
    pub fn is_key_frame(&self) -> bool {
        self.flags & FLAG_KEY_FRAME != 0
    }
}

/// Raw NV12 会话几何。宽高必须为偶数且在 2..=8192；帧字节数为 `w*h*3/2`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    width: u32,
    height: u32,
}

impl Geometry {
    pub fn new(width: u32, height: u32) -> Result<Self, WireError> {
        let bad = width < 2
            || height < 2
            || width > MAX_DIMENSION
            || height > MAX_DIMENSION
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2);
        if bad {
            return Err(WireError::BadGeometry { width, height });
        }
        Ok(Self { width, height })
    }

    /// 默认产品几何 640×640。
    pub fn default_product() -> Self {
        Self {
            width: 640,
            height: 640,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// 整帧字节数：Y 平面 `w*h` + UV 平面一半。
    pub fn frame_bytes(&self) -> usize {
        (self.width as usize * self.height as usize * 3) / 2
    }
}

/// `payload = mtu - 42`（以太头 14 + VC 头 28），与 sender/receiver 一致。
pub fn payload_max_for_mtu(mtu: usize) -> Result<usize, WireError> {
    if mtu <= L2_OVERHEAD || mtu > MAX_MTU {
        return Err(WireError::BadMtu(mtu));
    }
    Ok(mtu - L2_OVERHEAD)
}

/// 单帧包数；超过 u16 上限返回错误（发送端同样拒绝）。
pub fn packet_count(frame_bytes: usize, payload_max: usize) -> Result<u16, WireError> {
    if frame_bytes == 0 || payload_max == 0 {
        return Err(WireError::BadFragment {
            bytes: frame_bytes,
            payload_max,
        });
    }
    let total = frame_bytes.div_ceil(payload_max);
    u16::try_from(total).map_err(|_| WireError::BadFragment {
        bytes: frame_bytes,
        payload_max,
    })
}

/// 指定 seq 的载荷长度：除尾包外都是 `payload_max`。
pub fn payload_len_at(seq: u16, total: u16, frame_bytes: usize, payload_max: usize) -> usize {
    debug_assert!(seq < total);
    frame_bytes
        .saturating_sub(seq as usize * payload_max)
        .min(payload_max)
}

/// 从完整以太帧读取 EtherType（网络字节序）。
pub fn ethertype_at(frame: &[u8]) -> Option<u16> {
    frame.get(12..14).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

/// 从完整以太帧读取源 MAC。
pub fn source_mac_at(frame: &[u8]) -> Option<[u8; 6]> {
    frame
        .get(6..12)
        .map(|b| [b[0], b[1], b[2], b[3], b[4], b[5]])
}

/// 序号算术：`a` 是否比 `b` 新（RFC 1982，半个 2^32 窗口）。
///
/// 帧号在会话内单调递增；正常差值下与 `a > b` 一致，跨 2^32 回绕仍正确。
pub fn is_after(a: u32, b: u32) -> bool {
    let delta = a.wrapping_sub(b);
    delta != 0 && delta < 0x8000_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_matches_frozen_nv12_sizes() {
        assert_eq!(Geometry::default_product().frame_bytes(), 614_400);
        assert_eq!(Geometry::new(416, 416).unwrap().frame_bytes(), 259_584);
        assert_eq!(Geometry::new(1280, 720).unwrap().frame_bytes(), 1_382_400);
        assert!(Geometry::new(641, 640).is_err());
        assert!(Geometry::new(0, 640).is_err());
        assert!(Geometry::new(8194, 2).is_err());
    }

    #[test]
    fn mtu_and_packet_counts_match_sender_selftest() {
        let frame = Geometry::default_product().frame_bytes();
        let p9000 = payload_max_for_mtu(9000).unwrap();
        assert_eq!(p9000, 8958);
        assert_eq!(packet_count(frame, p9000).unwrap(), 69);
        assert_eq!(payload_len_at(68, 69, frame, p9000), 5256);

        let p1500 = payload_max_for_mtu(1500).unwrap();
        assert_eq!(p1500, 1458);
        assert_eq!(packet_count(frame, p1500).unwrap(), 422);
        assert_eq!(payload_len_at(421, 422, frame, p1500), 582);

        assert!(payload_max_for_mtu(42).is_err());
        assert!(payload_max_for_mtu(9001).is_err());
    }

    #[test]
    fn header_layout_is_frozen() {
        let header = Header {
            magic: MAGIC,
            frame_id: 0x0102_0304,
            seq: 0x0005,
            total: 69,
            qpc_ns: 0x1122_3344_5566_7788,
            payload_crc: 0xDEAD_BEEF,
            payload_len: 8958,
            flags: 1,
            pattern: 4,
        };
        let bytes = header.encode();
        assert_eq!(bytes.len(), 28);
        assert_eq!(&bytes[0..4], &[0x50, 0x41, 0x43, 0x56]);
        assert_eq!(&bytes[4..8], &[0x04, 0x03, 0x02, 0x01]);
        assert_eq!(&bytes[8..10], &[0x05, 0x00]);
        assert_eq!(&bytes[10..12], &[0x45, 0x00]);
        assert_eq!(
            &bytes[12..20],
            &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]
        );
        assert_eq!(&bytes[20..24], &[0xEF, 0xBE, 0xAD, 0xDE]);
        assert_eq!(&bytes[24..26], &[0xFE, 0x22]);
        assert_eq!(bytes[26], 1);
        assert_eq!(bytes[27], 4);
        assert_eq!(Header::parse(&bytes).unwrap(), header);
        assert!(header.is_key_frame());
    }

    #[test]
    fn header_parse_rejects_short_and_wrong_magic() {
        assert_eq!(
            Header::parse(&[0u8; 27]),
            Err(WireError::TooShort {
                needed: 28,
                got: 27
            })
        );
        let mut bytes = Header::default().encode();
        bytes[0] = 0;
        assert!(matches!(Header::parse(&bytes), Err(WireError::BadMagic(0))));
    }

    #[test]
    fn serial_comparison_handles_wrap() {
        assert!(is_after(5, 4));
        assert!(!is_after(4, 5));
        assert!(!is_after(7, 7));
        assert!(is_after(0, u32::MAX));
        assert!(is_after(3, u32::MAX - 2));
        assert!(!is_after(u32::MAX, 0));
    }
}
