//! Windows 原生后端集成测试。
//!
//! 依赖目录查找顺序：环境变量 → 本仓库构建产物。缺少 DLL 或 Vulkan 设备时明确
//! 打印 SKIP 并返回，不伪造成通过；只要找到运行库，就必须真实编码/解码。

#![cfg(windows)]

use std::path::{Path, PathBuf};

use vc_media::{
    AuSession, CodecConfig, Decoder, Encoder, PYROWAVE_IDENTITY, build_au, capabilities, parse_au,
    pyrowave_frame_budget, validate,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("仓库根目录")
}

fn find_library_dir(
    variable: &str,
    relative_candidates: &[&str],
    library: &str,
) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(variable) {
        let path = PathBuf::from(value);
        if path.join(library).is_file() {
            return Some(path);
        }
    }
    for candidate in relative_candidates {
        let path = repo_root().join(candidate);
        if path.join(library).is_file() {
            return Some(path);
        }
    }
    None
}

fn pyrowave_directory() -> Option<PathBuf> {
    find_library_dir(
        "VC_MEDIA_PYROWAVE_DIR",
        &[
            "build",
            "build/Release",
            "build/_deps/pyrowave-build/Release",
        ],
        "libpyrowave-shared-1.dll",
    )
}

fn mjpeg_config() -> CodecConfig {
    CodecConfig {
        codec: "mjpeg".into(),
        experimental: true,
        encoder_backend: "turbojpeg:cpu".into(),
        decoder_backend: "turbojpeg:cpu".into(),
        jpeg_quality: 90,
        ..CodecConfig::default()
    }
}

fn pyrowave_config() -> CodecConfig {
    CodecConfig {
        codec: "pyrowave".into(),
        experimental: true,
        encoder_backend: PYROWAVE_IDENTITY.into(),
        decoder_backend: PYROWAVE_IDENTITY.into(),
        max_bitrate_bps: 800_000_000,
        ..CodecConfig::default()
    }
}

fn ramp_nv12(width: u32, height: u32) -> Vec<u8> {
    let y_bytes = width as usize * height as usize;
    let mut frame = Vec::with_capacity(y_bytes + y_bytes / 2);
    for y in 0..height as usize {
        for x in 0..width as usize {
            frame.push(((x * 3 + y * 5 + 7) & 0xff) as u8);
        }
    }
    for y in 0..height as usize / 2 {
        for x in 0..width as usize / 2 {
            frame.push(if (x + y) % 2 == 0 { 32 } else { 208 });
            frame.push(if (x + y) % 3 == 0 { 208 } else { 32 });
        }
    }
    frame
}

#[test]
fn raw_passthrough_round_trips_without_dlls() {
    let config = CodecConfig::default();
    validate(&config, 64, 36, 30).unwrap();
    let mut encoder = Encoder::open(&config, 64, 36, 30).unwrap();
    assert_eq!(encoder.settings()["family"], "raw");
    let input = ramp_nv12(64, 36);
    let encoded = encoder.encode(&input, 0).unwrap();
    assert_eq!(encoded, input);

    let mut decoder = Decoder::open(&config, 64, 36, 30).unwrap();
    assert_eq!(decoder.settings()["family"], "raw");
    assert_eq!(decoder.decode(&input, 0).unwrap(), input);
    assert!(decoder.decode(&input[..input.len() - 1], 0).is_err());
    decoder.reset().unwrap();
    assert_eq!(decoder.decode(&input, 5).unwrap(), input);
}

#[test]
fn mjpeg_is_rejected_with_actionable_gpu_only_error() {
    let config = mjpeg_config();
    for error in [
        validate(&config, 640, 640, 30).unwrap_err(),
        Encoder::open(&config, 640, 640, 30)
            .err()
            .expect("MJPEG 编码器不得打开"),
        Decoder::open(&config, 640, 640, 30)
            .err()
            .expect("MJPEG 解码器不得打开"),
        // 即使显式给出不存在的依赖目录，也必须在查找 DLL 之前拒绝。
        Encoder::open_in_directory(&config, 640, 640, 30, Path::new("Z:\\missing"))
            .err()
            .expect("MJPEG 编码器不得打开"),
        Decoder::open_in_directory(&config, 640, 640, 30, Path::new("Z:\\missing"))
            .err()
            .expect("MJPEG 解码器不得打开"),
    ] {
        let error = format!("{error:#}");
        assert!(
            error.contains("CPU") && error.contains("turbojpeg:cpu"),
            "{error}"
        );
        assert!(
            error.contains("pyrowave") && error.contains("raw"),
            "{error}"
        );
        assert!(
            !error.contains("找不到"),
            "错误应说明 GPU-only 原因：{error}"
        );
    }
}

#[test]
fn pyrowave_round_trip_luma_chroma_and_reset() {
    let Some(directory) = pyrowave_directory() else {
        eprintln!("SKIP: 未找到 libpyrowave-shared-1.dll");
        return;
    };
    let config = pyrowave_config();
    let (width, height, fps) = (64u32, 64u32, 30u32);
    let mut encoder = match Encoder::open_in_directory(&config, width, height, fps, &directory) {
        Ok(encoder) => encoder,
        Err(error) if format!("{error:#}").contains("no Vulkan") => {
            eprintln!("SKIP: PyroWave 无可用 Vulkan 1.3 设备: {error:#}");
            return;
        }
        Err(error) => panic!("PyroWave 编码器打开失败: {error:#}"),
    };
    let expected_budget = pyrowave_frame_budget(&config, width, height, fps).unwrap();
    let settings = encoder.settings();
    assert_eq!(settings["family"], "pyrowave");
    assert_eq!(settings["identity"], PYROWAVE_IDENTITY);
    assert_eq!(settings["max_bytes_per_frame"], expected_budget);
    assert_eq!(settings["upload_path"], "cpu_nv12");
    assert_eq!(settings["zero_copy"], false);
    assert!(matches!(
        settings["device"]["status"].as_str(),
        Some("selected" | "unavailable")
    ));
    if settings["device"]["status"] == "selected" {
        eprintln!(
            "PyroWave 设备：{} ({}, vendor=0x{:04x}, device=0x{:04x})",
            settings["device"]["name"],
            settings["device"]["type"],
            settings["device"]["vendor_id"].as_u64().unwrap_or(0),
            settings["device"]["device_id"].as_u64().unwrap_or(0)
        );
    }

    let input = ramp_nv12(width, height);
    let encoded = encoder.encode(&input, 3).unwrap();
    let unit = parse_au(
        &encoded,
        AuSession::pyrowave(width, height, fps),
        3,
        config.max_au_bytes,
    )
    .unwrap();
    assert!(!unit.is_eos());
    assert!(unit.payload.len() >= 8);
    assert!(unit.payload.len() as u64 <= expected_budget);

    let mut decoder = match Decoder::open_in_directory(&config, width, height, fps, &directory) {
        Ok(decoder) => decoder,
        Err(error) if format!("{error:#}").contains("no Vulkan") => {
            eprintln!("SKIP: PyroWave 无可用 Vulkan 1.3 设备: {error:#}");
            return;
        }
        Err(error) => panic!("PyroWave 解码器打开失败: {error:#}"),
    };
    let decoder_settings = decoder.settings();
    assert_eq!(decoder_settings["max_bytes_per_frame"], 0);
    let device_before = decoder_settings["device"].clone();

    let decoded = decoder.decode(&encoded, 3).unwrap();
    assert_eq!(decoded.len(), input.len());
    let y_bytes = width as usize * height as usize;
    let luma_nonzero = decoded[..y_bytes].iter().filter(|&&byte| byte != 0).count();
    let chroma_nonzero = decoded[y_bytes..].iter().filter(|&&byte| byte != 0).count();
    assert!(luma_nonzero > y_bytes / 4, "亮度未写回：{luma_nonzero}");
    assert!(chroma_nonzero > 0, "色度未写回");
    assert!(
        decoded[..y_bytes]
            .windows(2)
            .filter(|pair| pair[0] != pair[1])
            .count()
            > y_bytes / 8,
        "解码图像缺少实际内容"
    );

    decoder.reset().unwrap();
    assert_eq!(decoder.settings()["device"], device_before);
    assert_eq!(decoder.decode(&encoded, 3).unwrap().len(), input.len());
    assert!(decoder.decode(&encoded, 4).is_err());

    let mut bad = encoded.clone();
    bad[32] ^= 1;
    assert!(decoder.decode(&bad, 3).is_err());
    let eos = build_au(AuSession::pyrowave(width, height, fps), 4, &[]).unwrap();
    assert!(decoder.decode(&eos, 4).unwrap().is_empty());
}

#[test]
fn pyrowave_rejects_budget_below_native_minimum() {
    let Some(directory) = pyrowave_directory() else {
        eprintln!("SKIP: 未找到 libpyrowave-shared-1.dll");
        return;
    };
    let mut config = pyrowave_config();
    config.max_au_bytes = 1024;
    // 1024-48=976 < 1024，必须在打开编解码器前拒绝。
    let error = match Encoder::open_in_directory(&config, 64, 64, 30, &directory) {
        Ok(_) => panic!("预算低于 1024 时不应创建 PyroWave 编码器"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("1024"), "{error:#}");
}

#[test]
fn default_library_directory_is_current_module_directory() {
    let directory = vc_media::default_library_directory().unwrap();
    let executable = std::env::current_exe().unwrap();
    let expected = executable.parent().unwrap().canonicalize().unwrap();
    assert_eq!(directory, expected);
}

#[test]
fn capabilities_shape_is_honest() {
    let caps = capabilities();
    assert_eq!(caps["codecs"], serde_json::json!(["raw", "pyrowave"]));
    assert_eq!(caps["removed_codecs"], serde_json::json!(["mjpeg"]));
    assert_eq!(
        caps["hardware"]["encoder"]["declares_vendor_hardware"],
        false
    );
    assert!(caps["libraries"].get("turbojpeg").is_none());
    assert_eq!(
        caps["libraries"]["pyrowave"]["file"],
        "libpyrowave-shared-1.dll"
    );
}
