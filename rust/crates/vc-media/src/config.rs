//! 编解码配置、显式身份校验与能力声明。
//!
//! 字段/取值范围来自 `references/encoded-stream.md`：
//! Raw 是缺省且无编码参数；MJPEG 必须显式 `experimental=true` 且两端
//! `turbojpeg:cpu`；PyroWave 必须显式 `experimental=true` 且两端
//! `pyrowave:vulkan`。不接受的组合明确报错，绝不静默回退 Raw 或换后端。

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vc_transport::Geometry;

use crate::au::AU_HEADER_BYTES;

/// MJPEG 唯一允许的后端身份（固定 libjpeg-turbo CPU/SIMD）。
pub const MJPEG_IDENTITY: &str = "turbojpeg:cpu";
/// PyroWave 唯一允许的后端身份（Vulkan 计算，intra-only）。
pub const PYROWAVE_IDENTITY: &str = "pyrowave:vulkan";
/// `max_au_bytes` 下限（含 48B 封装）。
pub const MIN_AU_BYTES: usize = 1024;
/// `max_au_bytes` 上限（含 48B 封装）。
pub const MAX_AU_BYTES: usize = 16_777_216;
/// 原生码率下限 0.1 Mbps。
pub const MIN_BITRATE_BPS: u64 = 100_000;
/// 原生码率上限 2000 Mbps（产品配置可保存更高草案，但编码启动受此限制）。
pub const MAX_BITRATE_BPS: u64 = 2_000_000_000;

/// 可移植的编解码配置。默认 Raw，不启用任何开发编码。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CodecConfig {
    /// raw | mjpeg | pyrowave
    pub codec: String,
    /// 开发编码必须显式启用；Raw 必须为 false。
    pub experimental: bool,
    /// 与 codec 精确匹配：MJPEG 为 `turbojpeg:cpu`，PyroWave 为 `pyrowave:vulkan`。
    pub encoder_backend: String,
    /// 接收端身份；两端必须一致。
    pub decoder_backend: String,
    /// 仅 MJPEG 使用；1..100，固定 4:2:0 + 快速整数 DCT。
    pub jpeg_quality: u32,
    /// 含 48B 封装的访问单元内存上限；1024..16777216。
    pub max_au_bytes: usize,
    /// 一秒滑窗发送上限；编码启动范围 0.1..2000 Mbps。
    pub max_bitrate_bps: u64,
    /// 解码在途上限；1..8，不因超时自动加深。
    pub decode_in_flight: u32,
}

impl Default for CodecConfig {
    fn default() -> Self {
        Self {
            codec: "raw".into(),
            experimental: false,
            encoder_backend: String::new(),
            decoder_backend: String::new(),
            jpeg_quality: 80,
            max_au_bytes: 8_388_608,
            max_bitrate_bps: 80_000_000,
            decode_in_flight: 2,
        }
    }
}

fn common_checks(config: &CodecConfig, geometry: &Geometry, fps: u32) -> Result<()> {
    ensure!(
        (1..=1000).contains(&fps),
        "fps 必须在 1..1000（当前 {fps}）"
    );
    ensure!(
        (MIN_AU_BYTES..=MAX_AU_BYTES).contains(&config.max_au_bytes),
        "max_au_bytes 必须在 {MIN_AU_BYTES}..{MAX_AU_BYTES}（当前 {}）",
        config.max_au_bytes
    );
    ensure!(
        (1..=8).contains(&config.decode_in_flight),
        "decode_in_flight 必须在 1..8（当前 {}）",
        config.decode_in_flight
    );
    ensure!(
        (1..=100).contains(&config.jpeg_quality),
        "jpeg_quality 必须在 1..100（当前 {}）",
        config.jpeg_quality
    );
    // uint16 分片上限：config 允许的最小 MTU 为 1500，载荷上限 1458。
    // 16 MiB / 1458 远小于 65535，这里显式检查，避免后续放宽范围时漏掉。
    let minimum_payload =
        vc_transport::payload_max_for_mtu(1500).context("冻结传输的最小 MTU 1500 无效")?;
    ensure!(
        config.max_au_bytes.div_ceil(minimum_payload) <= u16::MAX as usize,
        "max_au_bytes={} 在 MTU 1500 下超过 uint16 分片数",
        config.max_au_bytes
    );
    let _ = geometry;
    Ok(())
}

fn require_encoded_identity(config: &CodecConfig, identity: &str) -> Result<()> {
    ensure!(
        config.experimental,
        "{} 是开发编码，必须显式 experimental=true；不会自动启用或回退",
        config.codec
    );
    ensure!(
        config.encoder_backend == identity,
        "{} 的 encoder_backend 必须精确为 {identity}（当前 {:?}）",
        config.codec,
        config.encoder_backend
    );
    ensure!(
        config.decoder_backend == identity,
        "{} 的 decoder_backend 必须精确为 {identity}（当前 {:?}）",
        config.codec,
        config.decoder_backend
    );
    ensure!(
        (MIN_BITRATE_BPS..=MAX_BITRATE_BPS).contains(&config.max_bitrate_bps),
        "max_bitrate_bps 必须在 {MIN_BITRATE_BPS}..{MAX_BITRATE_BPS}（0.1..2000 Mbps，当前 {}）",
        config.max_bitrate_bps
    );
    Ok(())
}

/// 校验配置与会话几何；不打开 DLL、不分配编解码器。
pub fn validate(config: &CodecConfig, width: u32, height: u32, fps: u32) -> Result<()> {
    let geometry = Geometry::new(width, height)
        .with_context(|| format!("无效 NV12 会话尺寸 {width}x{height}"))?;
    common_checks(config, &geometry, fps)?;
    match config.codec.as_str() {
        "raw" => {
            ensure!(
                !config.experimental,
                "raw 不接受 experimental=true；要启用开发编码请把 codec 改为 mjpeg/pyrowave"
            );
            ensure!(
                config.encoder_backend.is_empty(),
                "raw 不接受 encoder_backend={:?}；Raw 没有编码后端",
                config.encoder_backend
            );
            ensure!(
                config.decoder_backend.is_empty(),
                "raw 不接受 decoder_backend={:?}；Raw 没有解码后端",
                config.decoder_backend
            );
        }
        "mjpeg" => {
            require_encoded_identity(config, MJPEG_IDENTITY)?;
        }
        "pyrowave" => {
            require_encoded_identity(config, PYROWAVE_IDENTITY)?;
            let budget = pyrowave_frame_budget_for(config, &geometry, fps)?;
            ensure!(
                budget >= 1024,
                "PyroWave 每帧预算 {budget} 字节低于 1024；请提高 max_bitrate_bps 或降低 fps"
            );
        }
        other => {
            bail!("codec={other} 不受支持；只允许 raw/mjpeg/pyrowave，H.264/H.265 等已按 D23 移除")
        }
    }
    Ok(())
}

fn pyrowave_frame_budget_for(config: &CodecConfig, geometry: &Geometry, fps: u32) -> Result<u64> {
    ensure!(fps >= 1, "fps 必须 >= 1");
    ensure!(
        config.max_au_bytes >= AU_HEADER_BYTES,
        "max_au_bytes 小于 {AU_HEADER_BYTES} 字节封装头"
    );
    let raw_bytes = geometry.frame_bytes() as u64;
    let by_rate = config.max_bitrate_bps / 8 / u64::from(fps);
    Ok(by_rate
        .min(config.max_au_bytes as u64 - AU_HEADER_BYTES as u64)
        .min(raw_bytes))
}

/// PyroWave 每帧输出上限：`min(max_bitrate/8/fps, max_au_bytes-48, 一帧 NV12 字节数)`。
///
/// 该值只是编码器每帧的 bitstream 上限（超限时编码器降低该帧精度），
/// 不是恒定码率，也不替代发送端一秒滑窗限流。
pub fn pyrowave_frame_budget(
    config: &CodecConfig,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<u64> {
    let geometry = Geometry::new(width, height)
        .with_context(|| format!("无效 NV12 会话尺寸 {width}x{height}"))?;
    pyrowave_frame_budget_for(config, &geometry, fps)
}

/// 运行时能力声明：只声明编译进来的路径和候选后端，不冒充已配置/已验证硬件。
pub fn capabilities() -> Value {
    let encoded = |codec: &str, identity: &str, family: &str, name: &str| {
        json!({
            "codec": codec,
            "identity": identity,
            "family": family,
            "name": name,
            "experimental": true,
            "hardware": false,
            "frames_tested": 0,
            "stream_validated": false
        })
    };
    let mjpeg = encoded(
        "mjpeg",
        MJPEG_IDENTITY,
        "turbojpeg",
        "libjpeg-turbo 3.1.2 CPU/SIMD",
    );
    let pyrowave = encoded(
        "pyrowave",
        PYROWAVE_IDENTITY,
        "pyrowave",
        "PyroWave Vulkan compute (intra-only)",
    );
    let raw = json!({
        "codec": "raw",
        "identity": "raw",
        "family": "raw",
        "name": "NV12 passthrough",
        "experimental": false,
        "hardware": false,
        "compression": false,
        "frames_tested": 0,
        "stream_validated": false
    });
    json!({
        "codec_revision": 1,
        "mjpeg_revision": 1,
        "pyrowave_revision": 1,
        "pixel_format": "nv12",
        "codecs": ["raw", "mjpeg", "pyrowave"],
        "encoded_requires_experimental": true,
        "no_automatic_fallback": true,
        "libraries": {
            "turbojpeg": {
                "file": "turbojpeg.dll",
                "alternate": "libturbojpeg.dll"
            },
            "pyrowave": {
                "file": "libpyrowave-shared-1.dll"
            }
        },
        "native": {
            "encoders": [raw.clone(), mjpeg.clone(), pyrowave.clone()],
            "decoders": [raw.clone(), mjpeg.clone(), pyrowave.clone()]
        },
        "hardware": {
            "encoder": {
                "revision": 1,
                "status": "candidates_only",
                "declares_vendor_hardware": false,
                "frames_tested": 0,
                "backends": [mjpeg.clone(), pyrowave.clone()]
            },
            "decoder": {
                "revision": 1,
                "status": "candidates_only",
                "declares_vendor_hardware": false,
                "frames_tested": 0,
                "backends": [mjpeg, pyrowave]
            }
        },
        "stream_validated": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded_config(codec: &str, identity: &str) -> CodecConfig {
        CodecConfig {
            codec: codec.into(),
            experimental: true,
            encoder_backend: identity.into(),
            decoder_backend: identity.into(),
            ..CodecConfig::default()
        }
    }

    #[test]
    fn default_is_raw_with_documented_values() {
        let config = CodecConfig::default();
        assert_eq!(
            config,
            CodecConfig {
                codec: "raw".into(),
                experimental: false,
                encoder_backend: String::new(),
                decoder_backend: String::new(),
                jpeg_quality: 80,
                max_au_bytes: 8_388_608,
                max_bitrate_bps: 80_000_000,
                decode_in_flight: 2,
            }
        );
        validate(&config, 640, 640, 240).unwrap();
    }

    #[test]
    fn serde_defaults_and_unknown_fields() {
        let config: CodecConfig = serde_json::from_str(r#"{"codec":"raw"}"#).unwrap();
        assert_eq!(config, CodecConfig::default());
        assert!(serde_json::from_str::<CodecConfig>(r#"{"codec":"raw","bogus":1}"#).is_err());
        let encoded: CodecConfig = serde_json::from_str(
            r#"{"codec":"mjpeg","experimental":true,"encoder_backend":"turbojpeg:cpu","decoder_backend":"turbojpeg:cpu","jpeg_quality":86}"#,
        )
        .unwrap();
        assert_eq!(encoded.jpeg_quality, 86);
        assert_eq!(encoded.max_au_bytes, 8_388_608);
    }

    #[test]
    fn rejects_invalid_common_geometry_and_limits() {
        let config = CodecConfig::default();
        assert!(validate(&config, 641, 640, 240).is_err());
        assert!(validate(&config, 640, 640, 0).is_err());
        assert!(validate(&config, 640, 640, 1001).is_err());
        let mut bad = config.clone();
        bad.max_au_bytes = 1023;
        assert!(validate(&bad, 640, 640, 240).is_err());
        bad.max_au_bytes = MAX_AU_BYTES + 1;
        assert!(validate(&bad, 640, 640, 240).is_err());
        let mut bad = config.clone();
        bad.decode_in_flight = 0;
        assert!(validate(&bad, 640, 640, 240).is_err());
        bad.decode_in_flight = 9;
        assert!(validate(&bad, 640, 640, 240).is_err());
        let mut bad = config;
        bad.jpeg_quality = 0;
        assert!(validate(&bad, 640, 640, 240).is_err());
        bad.jpeg_quality = 101;
        assert!(validate(&bad, 640, 640, 240).is_err());
    }

    #[test]
    fn encrypted_codecs_need_explicit_experimental_and_exact_identity() {
        let mut config = encoded_config("mjpeg", MJPEG_IDENTITY);
        validate(&config, 640, 640, 240).unwrap();

        config.experimental = false;
        assert!(validate(&config, 640, 640, 240).is_err());
        config.experimental = true;
        config.encoder_backend = "turbojpeg:gpu".into();
        assert!(validate(&config, 640, 640, 240).is_err());
        config.encoder_backend = MJPEG_IDENTITY.into();
        config.decoder_backend = String::new();
        assert!(validate(&config, 640, 640, 240).is_err());

        let config = encoded_config("mjpeg", PYROWAVE_IDENTITY);
        assert!(validate(&config, 640, 640, 240).is_err());
        let config = encoded_config("pyrowave", MJPEG_IDENTITY);
        assert!(validate(&config, 640, 640, 240).is_err());
        let config = encoded_config("pyrowave", PYROWAVE_IDENTITY);
        validate(&config, 640, 640, 240).unwrap();
    }

    #[test]
    fn raw_rejects_encoded_switches() {
        let config = CodecConfig {
            experimental: true,
            ..CodecConfig::default()
        };
        assert!(validate(&config, 640, 640, 240).is_err());
        let config = CodecConfig {
            encoder_backend: MJPEG_IDENTITY.into(),
            ..CodecConfig::default()
        };
        assert!(validate(&config, 640, 640, 240).is_err());
        assert!(validate(&CodecConfig::default(), 640, 640, 240).is_ok());
    }

    #[test]
    fn unsupported_codecs_are_rejected_by_name() {
        for codec in ["h264", "hevc", "nv12", "", "MJPEG"] {
            let config = CodecConfig {
                codec: codec.into(),
                ..CodecConfig::default()
            };
            assert!(validate(&config, 640, 640, 240).is_err(), "{codec}");
        }
    }

    #[test]
    fn pyrowave_budget_matches_documented_minimum() {
        let mut config = encoded_config("pyrowave", PYROWAVE_IDENTITY);
        // 80 Mbps / 8 / 240 = 41666; max_au-48 = 8388560; raw = 614400.
        assert_eq!(
            pyrowave_frame_budget(&config, 640, 640, 240).unwrap(),
            41_666
        );
        config.max_bitrate_bps = 800_000_000;
        assert_eq!(
            pyrowave_frame_budget(&config, 640, 640, 240).unwrap(),
            416_666
        );
        config.max_bitrate_bps = 2_000_000_000;
        assert_eq!(
            pyrowave_frame_budget(&config, 640, 640, 240).unwrap(),
            614_400
        );
        config.max_au_bytes = 1024;
        assert_eq!(pyrowave_frame_budget(&config, 640, 640, 1).unwrap(), 976);
        // 每帧预算 <1024 时必须拒绝启动。
        config.max_bitrate_bps = 100_000;
        config.max_au_bytes = 8_388_608;
        assert!(validate(&config, 640, 640, 1000).is_err());
    }

    #[test]
    fn capabilities_do_not_claim_configured_hardware() {
        let caps = capabilities();
        assert_eq!(caps["codec_revision"], 1);
        assert_eq!(caps["pixel_format"], "nv12");
        assert_eq!(caps["no_automatic_fallback"], true);
        let codecs = caps["codecs"].as_array().unwrap();
        assert_eq!(codecs.len(), 3);
        assert_eq!(caps["hardware"]["encoder"]["status"], "candidates_only");
        assert_eq!(
            caps["hardware"]["encoder"]["declares_vendor_hardware"],
            false
        );
        assert_eq!(caps["hardware"]["decoder"]["frames_tested"], 0);
        let backends = caps["hardware"]["encoder"]["backends"].as_array().unwrap();
        assert!(backends.iter().all(|backend| backend["frames_tested"] == 0));
        assert!(backends.iter().all(|backend| backend["hardware"] == false));
        assert_eq!(caps["native"]["encoders"][1]["identity"], MJPEG_IDENTITY);
        assert_eq!(caps["native"]["decoders"][2]["identity"], PYROWAVE_IDENTITY);
        assert_eq!(caps["stream_validated"], false);
    }
}
