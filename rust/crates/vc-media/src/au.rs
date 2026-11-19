//! 压缩访问单元（VCAU）的便携封装解析/构建。
//!
//! 当前契约见 `references/encoded-stream.md`；布局保留历史访问单元格式：
//! 48 字节小端头 + 编码载荷，整段再交给传输层按 `P=mtu-42` 分片。28 字节
//! `vc_wire.h` 包头完全不变，本封装只存在于数据面载荷内。
//!
//! 本模块不依赖 Windows/编解码库，主传输、测试和接收侧可共用同一实现。

use anyhow::{Context, Result, bail, ensure};
use vc_transport::checksum::fnv1a64;

/// VCAU 封装头长度（冻结）。
pub const AU_HEADER_BYTES: usize = 48;
/// VCAU magic `0x55414356`，小端字节为 `56 43 41 55`（"VCAU"）。
pub const AU_MAGIC: u32 = 0x5541_4356;
/// 封装修订号。
pub const AU_REVISION: u8 = 1;
/// 历史 MJPEG wire id；其 CPU/SIMD 后端已移除，保留该值只为安全识别并拒绝旧流。
pub const CODEC_MJPEG: u8 = 3;
/// PyroWave（D24）。
pub const CODEC_PYROWAVE: u8 = 4;
/// 独立帧（每个非 EOS 单元）。
pub const KIND_FRAME: u8 = 1;
/// EOS：无编码载荷，使用下一个 frame_id。
pub const KIND_EOS: u8 = 2;

/// 会话约定的编码身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuSession {
    /// [`CODEC_MJPEG`]（仅用于解析/拒绝旧流）或 [`CODEC_PYROWAVE`]。
    pub codec: u8,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl AuSession {
    /// MJPEG 会话只用于识别/拒绝旧流；本 crate 不再提供 MJPEG 编解码器。
    pub fn mjpeg(width: u32, height: u32, fps: u32) -> Self {
        Self {
            codec: CODEC_MJPEG,
            width,
            height,
            fps,
        }
    }

    pub fn pyrowave(width: u32, height: u32, fps: u32) -> Self {
        Self {
            codec: CODEC_PYROWAVE,
            width,
            height,
            fps,
        }
    }
}

/// 已验证的访问单元。载荷借用输入缓冲区。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessUnit<'a> {
    pub codec: u8,
    pub kind: u8,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frame_id: u32,
    pub payload_hash: u64,
    pub payload: &'a [u8],
}

impl AccessUnit<'_> {
    pub fn is_eos(&self) -> bool {
        self.kind == KIND_EOS
    }
}

/// 编码名 → 封装 codec id。Raw 没有 VCAU 封装，返回 `None`。
pub fn codec_id(name: &str) -> Option<u8> {
    match name {
        "mjpeg" => Some(CODEC_MJPEG),
        "pyrowave" => Some(CODEC_PYROWAVE),
        _ => None,
    }
}

/// 封装 codec id → 编码名。
pub fn codec_name(codec: u8) -> Option<&'static str> {
    match codec {
        CODEC_MJPEG => Some("mjpeg"),
        CODEC_PYROWAVE => Some("pyrowave"),
        _ => None,
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("VCAU 固定偏移已检查长度"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("VCAU 固定偏移已检查长度"),
    )
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// 完整性和每帧独立性的最小检查（对应 C++ `vcau::random_access`）。
fn check_payload_shape(codec: u8, payload: &[u8]) -> Result<()> {
    match codec {
        CODEC_MJPEG => ensure!(
            payload.len() >= 4
                && payload[0] == 0xff
                && payload[1] == 0xd8
                && payload[payload.len() - 2] == 0xff
                && payload[payload.len() - 1] == 0xd9,
            "MJPEG 载荷必须是完整 SOI..EOI JPEG（{} 字节）",
            payload.len()
        ),
        CODEC_PYROWAVE => ensure!(
            payload.len() >= 8,
            "PyroWave 载荷至少 8 字节（当前 {} 字节）",
            payload.len()
        ),
        other => bail!("VCAU codec={other} 不受支持；只允许 3=mjpeg、4=pyrowave"),
    }
    Ok(())
}

/// 用显式 kind 构建 VCAU。主传输通常用 [`build_au`]，它按载荷是否为空选择 kind。
pub fn build_au_kind(
    session: AuSession,
    frame_id: u32,
    kind: u8,
    payload: &[u8],
) -> Result<Vec<u8>> {
    ensure!(
        matches!(session.codec, CODEC_MJPEG | CODEC_PYROWAVE),
        "VCAU codec={} 不受支持；Raw 没有 VCAU 封装",
        session.codec
    );
    ensure!(
        matches!(kind, KIND_FRAME | KIND_EOS),
        "VCAU kind 必须是 1（独立帧）或 2（EOS），当前 {kind}"
    );
    if kind == KIND_EOS {
        ensure!(payload.is_empty(), "EOS 不得携带编码载荷");
    } else {
        ensure!(!payload.is_empty(), "非 EOS 访问单元不得是空图像帧");
        check_payload_shape(session.codec, payload)?;
    }
    ensure!(
        payload.len() <= u32::MAX as usize,
        "编码载荷超过 u32 长度上限"
    );
    let total = AU_HEADER_BYTES
        .checked_add(payload.len())
        .context("VCAU 总长度溢出")?;
    let mut out = vec![0u8; total];
    write_u32(&mut out, 0, AU_MAGIC);
    out[4] = AU_REVISION;
    out[5] = session.codec;
    out[6] = kind;
    out[7] = AU_HEADER_BYTES as u8;
    write_u32(&mut out, 8, session.width);
    write_u32(&mut out, 12, session.height);
    write_u32(&mut out, 16, session.fps);
    write_u32(&mut out, 20, payload.len() as u32);
    write_u32(&mut out, 24, frame_id);
    write_u64(&mut out, 32, fnv1a64(payload));
    // 28 与 40 保留为 0（vec 已置零）。
    out[AU_HEADER_BYTES..].copy_from_slice(payload);
    Ok(out)
}

/// 非空载荷构建独立帧；空载荷构建 EOS（与 C++ `pack_into` 的 kind 选择一致）。
pub fn build_au(session: AuSession, frame_id: u32, payload: &[u8]) -> Result<Vec<u8>> {
    let kind = if payload.is_empty() {
        KIND_EOS
    } else {
        KIND_FRAME
    };
    build_au_kind(session, frame_id, kind, payload)
}

/// 按会话与帧号严格解析/校验 VCAU；`max_au_bytes` 含 48B 头。
pub fn parse_au<'a>(
    bytes: &'a [u8],
    session: AuSession,
    frame_id: u32,
    max_au_bytes: usize,
) -> Result<AccessUnit<'a>> {
    ensure!(
        bytes.len() >= AU_HEADER_BYTES,
        "VCAU 数据 {} 字节，不足 {} 字节头",
        bytes.len(),
        AU_HEADER_BYTES
    );
    ensure!(
        bytes.len() <= max_au_bytes,
        "VCAU 数据 {} 字节超过 max_au_bytes={max_au_bytes}",
        bytes.len()
    );
    ensure!(
        read_u32(bytes, 0) == AU_MAGIC,
        "VCAU magic 无效：0x{:08x}",
        read_u32(bytes, 0)
    );
    ensure!(
        bytes[4] == AU_REVISION,
        "VCAU revision={} 不受支持（需要 {AU_REVISION}）",
        bytes[4]
    );
    let codec = bytes[5];
    ensure!(
        codec == session.codec,
        "VCAU codec={codec} 与会话 codec={} 不符",
        session.codec
    );
    let kind = bytes[6];
    ensure!(
        matches!(kind, KIND_FRAME | KIND_EOS),
        "VCAU kind={kind} 无效（只允许 1/2）"
    );
    ensure!(
        bytes[7] == AU_HEADER_BYTES as u8,
        "VCAU header_bytes={}，需要 {AU_HEADER_BYTES}",
        bytes[7]
    );
    ensure!(
        read_u32(bytes, 28) == 0 && read_u64(bytes, 40) == 0,
        "VCAU 保留字段必须为 0"
    );
    let width = read_u32(bytes, 8);
    let height = read_u32(bytes, 12);
    let fps = read_u32(bytes, 16);
    ensure!(
        width == session.width && height == session.height && fps == session.fps,
        "VCAU 会话 {width}x{height}@{fps} 与会话 {}x{}@{} 不符",
        session.width,
        session.height,
        session.fps
    );
    let payload_len = read_u32(bytes, 20) as usize;
    ensure!(
        payload_len == bytes.len() - AU_HEADER_BYTES,
        "VCAU payload_len={payload_len} 与实际 {} 不符",
        bytes.len() - AU_HEADER_BYTES
    );
    ensure!(
        read_u32(bytes, 24) == frame_id,
        "VCAU frame_id={} 与期望 {frame_id} 不符",
        read_u32(bytes, 24)
    );
    let payload = &bytes[AU_HEADER_BYTES..];
    if kind == KIND_EOS {
        ensure!(payload.is_empty(), "VCAU EOS 不得携带编码载荷");
    } else {
        ensure!(!payload.is_empty(), "VCAU 空图像帧无效");
        check_payload_shape(codec, payload)?;
    }
    let payload_hash = read_u64(bytes, 32);
    ensure!(
        payload_hash == fnv1a64(payload),
        "VCAU 载荷 FNV-1a64 不匹配：头 {payload_hash:016x}，实际 {:016x}",
        fnv1a64(payload)
    );
    Ok(AccessUnit {
        codec,
        kind,
        width,
        height,
        fps,
        frame_id,
        payload_hash,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg_payload() -> Vec<u8> {
        let mut payload = vec![0xff, 0xd8, 0x01, 0x02, 0x03];
        payload.extend_from_slice(&[0xff, 0xd9]);
        payload
    }

    #[test]
    fn builds_exact_48_byte_envelope_and_round_trips() {
        let session = AuSession::mjpeg(66, 34, 30);
        let payload = jpeg_payload();
        let bytes = build_au(session, 7, &payload).unwrap();
        assert_eq!(bytes.len(), AU_HEADER_BYTES + payload.len());
        assert_eq!(&bytes[0..4], &[0x56, 0x43, 0x41, 0x55]);
        assert_eq!(bytes[4], 1);
        assert_eq!(bytes[5], 3);
        assert_eq!(bytes[6], KIND_FRAME);
        assert_eq!(bytes[7], 48);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 66);
        assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 34);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 30);
        assert_eq!(
            u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize,
            payload.len()
        );
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 7);
        assert_eq!(&bytes[28..32], &[0, 0, 0, 0]);
        assert_eq!(
            u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            fnv1a64(&payload)
        );
        assert_eq!(&bytes[40..48], &[0; 8]);
        assert_eq!(&bytes[48..], payload.as_slice());

        let unit = parse_au(&bytes, session, 7, 8388608).unwrap();
        assert_eq!(unit.payload, payload.as_slice());
        assert!(!unit.is_eos());
        assert_eq!(unit.payload_hash, fnv1a64(&payload));
    }

    #[test]
    fn empty_payload_is_eos_with_offset_basis_hash() {
        let session = AuSession::pyrowave(64, 64, 30);
        let bytes = build_au(session, 9, &[]).unwrap();
        assert_eq!(bytes.len(), AU_HEADER_BYTES);
        assert_eq!(bytes[6], KIND_EOS);
        assert_eq!(
            u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            fnv1a64(&[])
        );
        let unit = parse_au(&bytes, session, 9, 1024).unwrap();
        assert!(unit.is_eos());
        assert!(unit.payload.is_empty());
    }

    #[test]
    fn rejects_invalid_build_inputs() {
        let mjpeg = AuSession::mjpeg(64, 64, 30);
        assert!(build_au_kind(mjpeg, 1, KIND_FRAME, &[]).is_err());
        assert!(build_au_kind(mjpeg, 1, KIND_EOS, &jpeg_payload()).is_err());
        assert!(build_au(mjpeg, 1, b"not a jpeg").is_err());
        assert!(build_au(AuSession::pyrowave(64, 64, 30), 1, &[1, 2, 3]).is_err());
        assert!(build_au_kind(mjpeg, 1, 0, &jpeg_payload()).is_err());
        assert!(
            build_au_kind(
                AuSession {
                    codec: 1,
                    width: 640,
                    height: 640,
                    fps: 240
                },
                1,
                KIND_FRAME,
                &jpeg_payload()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_every_tampered_envelope_field() {
        let session = AuSession::mjpeg(64, 64, 30);
        let payload = jpeg_payload();
        let bytes = build_au(session, 5, &payload).unwrap();

        type Mutate = Box<dyn Fn(&mut Vec<u8>)>;
        let mutations: Vec<(&str, Mutate)> = vec![
            ("magic", Box::new(|b| b[0] ^= 1)),
            ("revision", Box::new(|b| b[4] = 2)),
            ("codec", Box::new(|b| b[5] = 4)),
            ("kind", Box::new(|b| b[6] = 0)),
            ("header_bytes", Box::new(|b| b[7] = 47)),
            ("width", Box::new(|b| b[8] ^= 1)),
            ("height", Box::new(|b| b[12] ^= 1)),
            ("fps", Box::new(|b| b[16] ^= 1)),
            ("payload_len", Box::new(|b| b[20] ^= 1)),
            ("frame_id", Box::new(|b| b[24] ^= 1)),
            ("reserved28", Box::new(|b| b[28] = 1)),
            ("reserved40", Box::new(|b| b[40] = 1)),
            ("hash", Box::new(|b| b[32] ^= 1)),
            ("payload", Box::new(|b| *b.last_mut().unwrap() ^= 1)),
        ];
        for (name, mutate) in mutations {
            let mut bad = bytes.clone();
            mutate(&mut bad);
            assert!(
                parse_au(&bad, session, 5, 8388608).is_err(),
                "{name} 篡改未被拒绝"
            );
        }
        assert!(parse_au(&bytes, session, 6, 8388608).is_err());
        assert!(parse_au(&bytes[..bytes.len() - 1], session, 5, 8388608).is_err());
        assert!(parse_au(&bytes, session, 5, 48).is_err());
    }

    #[test]
    fn codec_names_round_trip() {
        assert_eq!(codec_id("mjpeg"), Some(CODEC_MJPEG));
        assert_eq!(codec_id("pyrowave"), Some(CODEC_PYROWAVE));
        assert_eq!(codec_id("raw"), None);
        assert_eq!(codec_name(CODEC_MJPEG), Some("mjpeg"));
        assert_eq!(codec_name(CODEC_PYROWAVE), Some("pyrowave"));
        assert_eq!(codec_name(2), None);
    }
}
