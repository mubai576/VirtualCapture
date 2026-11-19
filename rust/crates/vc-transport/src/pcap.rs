//! 只读的经典 pcap 解析器（无第三方依赖）。
//!
//! 支持经典 pcap 的四种魔数组合：小端/大端 × 微秒/纳秒时间戳；链路类型只接受
//! `LINKTYPE_ETHERNET (1)`。pcapng 会明确报错而不是猜测解析。
//!
//! 输入有明确上界：单包捕获长度不超过 [`MAX_CAPTURED_PACKET`]，记录长度在分配前检查；
//! 文件按记录流式读取，不把整个抓包载入内存。调用方还可以限制读取的包数。

use std::fmt;
use std::io::{self, Read};

/// 单个记录允许的最大捕获长度；超过视为损坏/非以太网抓包。
pub const MAX_CAPTURED_PACKET: usize = 1 << 20;

/// 文件字节序（由魔数决定，与主机字节序无关）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    pub fn name(self) -> &'static str {
        match self {
            Self::Little => "little",
            Self::Big => "big",
        }
    }
}

/// 经典 pcap 全局头（已按文件字节序解析）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHeader {
    pub endian: Endian,
    /// 时间戳是否为纳秒精度（否则为微秒）。
    pub nanosecond: bool,
    pub version_major: u16,
    pub version_minor: u16,
    pub thiszone: i32,
    pub sigfigs: u32,
    pub snaplen: u32,
    pub linktype: u32,
}

/// 一条抓包记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// 捕获时间戳（Unix epoch 纳秒；微秒文件已换算）。
    pub ts_ns: u64,
    pub captured_len: u32,
    pub original_len: u32,
    /// 捕获到的链路层字节。
    pub data: Vec<u8>,
}

/// 解析错误。所有错误都带足够上下文，CLI 可直接打印。
#[derive(Debug)]
pub enum PcapError {
    Io(io::Error),
    /// 文件不足以容纳经典 pcap 全局头。
    TooShort {
        got: usize,
        needed: usize,
    },
    /// 识别为 pcapng；本工具只支持经典 pcap。
    PcapNg,
    /// 四个魔数字节不属于任何支持的经典 pcap 变体。
    UnknownMagic([u8; 4]),
    UnsupportedVersion {
        major: u16,
        minor: u16,
    },
    UnsupportedLinkType(u32),
    /// 记录头不完整（文件在记录中间截断）。
    TruncatedRecordHeader {
        index: u64,
        got: usize,
    },
    /// 载荷不足 `caplen`。
    TruncatedPacket {
        index: u64,
        declared: usize,
        got: usize,
    },
    /// `caplen` 超过 [`MAX_CAPTURED_PACKET`]，拒绝分配。
    PacketTooLarge {
        index: u64,
        captured_len: usize,
        limit: usize,
    },
}

impl fmt::Display for PcapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "读取失败：{err}"),
            Self::TooShort { got, needed } => {
                write!(
                    f,
                    "不是经典 pcap：文件只有 {got} 字节，需要至少 {needed} 字节"
                )
            }
            Self::PcapNg => write!(
                f,
                "输入是 pcapng，不支持；请转成经典 pcap（如 tcpdump -w out.pcap -r in.pcapng）"
            ),
            Self::UnknownMagic(bytes) => write!(
                f,
                "未知 pcap 魔数 {:02x}{:02x}{:02x}{:02x}；只支持经典 pcap",
                bytes[0], bytes[1], bytes[2], bytes[3]
            ),
            Self::UnsupportedVersion { major, minor } => {
                write!(f, "不支持的 pcap 版本 {major}.{minor}（只支持 2.x）")
            }
            Self::UnsupportedLinkType(linktype) => write!(
                f,
                "不支持的链路类型 {linktype}（只支持 1 = Ethernet）；请用以太网抓包"
            ),
            Self::TruncatedRecordHeader { index, got } => {
                write!(f, "第 {index} 条记录头被截断：只有 {got}/16 字节")
            }
            Self::TruncatedPacket {
                index,
                declared,
                got,
            } => write!(
                f,
                "第 {index} 条记录载荷被截断：声明 {declared} 字节，实际只有 {got} 字节"
            ),
            Self::PacketTooLarge {
                index,
                captured_len,
                limit,
            } => write!(
                f,
                "第 {index} 条记录捕获长度 {captured_len} 超过上限 {limit} 字节"
            ),
        }
    }
}

impl std::error::Error for PcapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for PcapError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// 经典 pcap 流式读取器。
#[derive(Debug)]
pub struct PcapReader<R> {
    reader: R,
    header: FileHeader,
    index: u64,
}

impl<R: Read> PcapReader<R> {
    /// 读取并校验 24 字节全局头；之后每次 [`Self::next_packet`] 读取一条记录。
    pub fn new(mut reader: R) -> Result<Self, PcapError> {
        let mut buf = [0u8; 24];
        let got = read_full(&mut reader, &mut buf)?;
        if got < 4 {
            return Err(PcapError::TooShort { got, needed: 24 });
        }
        let magic = [buf[0], buf[1], buf[2], buf[3]];
        if magic == [0x0a, 0x0d, 0x0d, 0x0a] {
            return Err(PcapError::PcapNg);
        }
        let (endian, nanosecond) = match magic {
            [0xa1, 0xb2, 0xc3, 0xd4] => (Endian::Big, false),
            [0xd4, 0xc3, 0xb2, 0xa1] => (Endian::Little, false),
            [0xa1, 0xb2, 0x3c, 0x4d] => (Endian::Big, true),
            [0x4d, 0x3c, 0xb2, 0xa1] => (Endian::Little, true),
            other => return Err(PcapError::UnknownMagic(other)),
        };
        if got < 24 {
            return Err(PcapError::TooShort { got, needed: 24 });
        }
        let header = FileHeader {
            endian,
            nanosecond,
            version_major: read_u16(&buf[4..6], endian),
            version_minor: read_u16(&buf[6..8], endian),
            thiszone: read_i32(&buf[8..12], endian),
            sigfigs: read_u32(&buf[12..16], endian),
            snaplen: read_u32(&buf[16..20], endian),
            linktype: read_u32(&buf[20..24], endian),
        };
        if header.version_major != 2 {
            return Err(PcapError::UnsupportedVersion {
                major: header.version_major,
                minor: header.version_minor,
            });
        }
        if header.linktype != 1 {
            return Err(PcapError::UnsupportedLinkType(header.linktype));
        }
        Ok(Self {
            reader,
            header,
            index: 0,
        })
    }

    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    /// 已成功读取的记录数（从 0 开始）。
    pub fn packet_index(&self) -> u64 {
        self.index
    }

    /// 读取下一条记录；正常 EOF 返回 `Ok(None)`，半条记录返回错误。
    pub fn next_packet(&mut self) -> Result<Option<Packet>, PcapError> {
        let mut record = [0u8; 16];
        let got = read_full(&mut self.reader, &mut record)?;
        if got == 0 {
            return Ok(None);
        }
        if got < record.len() {
            return Err(PcapError::TruncatedRecordHeader {
                index: self.index,
                got,
            });
        }
        let endian = self.header.endian;
        let ts_sec = read_u32(&record[0..4], endian) as u64;
        let ts_frac = read_u32(&record[4..8], endian) as u64;
        let captured_len = read_u32(&record[8..12], endian);
        let original_len = read_u32(&record[12..16], endian);
        let caplen = captured_len as usize;
        if caplen > MAX_CAPTURED_PACKET {
            return Err(PcapError::PacketTooLarge {
                index: self.index,
                captured_len: caplen,
                limit: MAX_CAPTURED_PACKET,
            });
        }
        let mut data = vec![0u8; caplen];
        let got = read_full(&mut self.reader, &mut data)?;
        if got < caplen {
            return Err(PcapError::TruncatedPacket {
                index: self.index,
                declared: caplen,
                got,
            });
        }
        let frac_ns = if self.header.nanosecond {
            ts_frac
        } else {
            ts_frac.saturating_mul(1_000)
        };
        let ts_ns = ts_sec.saturating_mul(1_000_000_000).saturating_add(frac_ns);
        self.index += 1;
        Ok(Some(Packet {
            ts_ns,
            captured_len,
            original_len,
            data,
        }))
    }
}

/// 读满缓冲区；返回实际读取的字节数（EOF 时可能小于 `buf.len()`）。
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

fn read_u16(bytes: &[u8], endian: Endian) -> u16 {
    let pair = [bytes[0], bytes[1]];
    match endian {
        Endian::Little => u16::from_le_bytes(pair),
        Endian::Big => u16::from_be_bytes(pair),
    }
}

fn read_u32(bytes: &[u8], endian: Endian) -> u32 {
    let quad = [bytes[0], bytes[1], bytes[2], bytes[3]];
    match endian {
        Endian::Little => u32::from_le_bytes(quad),
        Endian::Big => u32::from_be_bytes(quad),
    }
}

fn read_i32(bytes: &[u8], endian: Endian) -> i32 {
    read_u32(bytes, endian) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn header(endian: Endian, nano: bool, linktype: u32) -> Vec<u8> {
        let mut out = Vec::new();
        // 魔数是文件本机字节序下的 0xa1b2c3d4（纳秒变体 0xa1b23c4d）。
        let magic = if nano { 0xa1b2_3c4du32 } else { 0xa1b2_c3d4 };
        let put16 = |out: &mut Vec<u8>, v: u16| match endian {
            Endian::Little => out.extend_from_slice(&v.to_le_bytes()),
            Endian::Big => out.extend_from_slice(&v.to_be_bytes()),
        };
        let put32 = |out: &mut Vec<u8>, v: u32| match endian {
            Endian::Little => out.extend_from_slice(&v.to_le_bytes()),
            Endian::Big => out.extend_from_slice(&v.to_be_bytes()),
        };
        put32(&mut out, magic);
        put16(&mut out, 2);
        put16(&mut out, 4);
        put32(&mut out, 0);
        put32(&mut out, 0);
        put32(&mut out, 65535);
        put32(&mut out, linktype);
        out
    }

    fn record(endian: Endian, sec: u32, frac: u32, data: &[u8], orig: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let put32 = |out: &mut Vec<u8>, v: u32| match endian {
            Endian::Little => out.extend_from_slice(&v.to_le_bytes()),
            Endian::Big => out.extend_from_slice(&v.to_be_bytes()),
        };
        put32(&mut out, sec);
        put32(&mut out, frac);
        put32(&mut out, data.len() as u32);
        put32(&mut out, orig);
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn reads_all_four_classic_variants() {
        for endian in [Endian::Little, Endian::Big] {
            for nano in [false, true] {
                let mut bytes = header(endian, nano, 1);
                bytes.extend(record(endian, 7, if nano { 123 } else { 456 }, b"frame", 5));
                bytes.extend(record(endian, 8, 0, b"", 0));
                let mut reader = PcapReader::new(Cursor::new(bytes)).unwrap();
                assert_eq!(reader.header().endian, endian);
                assert_eq!(reader.header().nanosecond, nano);
                let first = reader.next_packet().unwrap().unwrap();
                assert_eq!(first.data, b"frame");
                assert_eq!(first.captured_len, 5);
                assert_eq!(first.original_len, 5);
                let frac_ns = if nano { 123 } else { 456_000 };
                assert_eq!(first.ts_ns, 7_000_000_000 + frac_ns);
                let second = reader.next_packet().unwrap().unwrap();
                assert!(second.data.is_empty());
                assert!(reader.next_packet().unwrap().is_none());
                assert_eq!(reader.packet_index(), 2);
            }
        }
    }

    #[test]
    fn pcapng_is_rejected_explicitly() {
        let bytes = [0x0au8, 0x0d, 0x0d, 0x0a, 0, 0, 0, 0];
        let err = PcapReader::new(Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, PcapError::PcapNg));
        assert!(err.to_string().contains("pcapng"));
    }

    #[test]
    fn unsupported_linktype_is_rejected() {
        let bytes = header(Endian::Little, false, 101);
        let err = PcapReader::new(Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, PcapError::UnsupportedLinkType(101)));
    }

    #[test]
    fn truncated_record_and_payload_are_errors() {
        let mut bytes = header(Endian::Little, false, 1);
        bytes.extend_from_slice(&[1, 2, 3]);
        let mut reader = PcapReader::new(Cursor::new(bytes)).unwrap();
        let err = reader.next_packet().unwrap_err();
        assert!(matches!(
            err,
            PcapError::TruncatedRecordHeader { index: 0, got: 3 }
        ));

        let mut bytes = header(Endian::Little, false, 1);
        bytes.extend_from_slice(&record(Endian::Little, 1, 0, b"abc", 3));
        bytes.truncate(bytes.len() - 1);
        let mut reader = PcapReader::new(Cursor::new(bytes)).unwrap();
        let err = reader.next_packet().unwrap_err();
        assert!(matches!(
            err,
            PcapError::TruncatedPacket {
                index: 0,
                declared: 3,
                got: 2
            }
        ));
    }

    #[test]
    fn oversized_record_is_rejected_before_allocation() {
        let mut bytes = header(Endian::Little, false, 1);
        let mut rec = record(Endian::Little, 1, 0, b"", 0);
        rec[8..12].copy_from_slice(&((MAX_CAPTURED_PACKET + 1) as u32).to_le_bytes());
        bytes.extend_from_slice(&rec);
        let mut reader = PcapReader::new(Cursor::new(bytes)).unwrap();
        let err = reader.next_packet().unwrap_err();
        assert!(matches!(err, PcapError::PacketTooLarge { .. }));
    }

    #[test]
    fn short_and_unknown_magic_are_errors() {
        let err = PcapReader::new(Cursor::new(vec![0xd4, 0xc3])).unwrap_err();
        assert!(matches!(err, PcapError::TooShort { got: 2, needed: 24 }));

        let err = PcapReader::new(Cursor::new(vec![0; 24])).unwrap_err();
        assert!(matches!(err, PcapError::UnknownMagic([0, 0, 0, 0])));
    }
}
