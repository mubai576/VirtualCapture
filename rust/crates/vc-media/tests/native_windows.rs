//! Windows 原生后端集成测试。
//!
//! 依赖目录查找顺序：环境变量 → 本仓库构建产物。缺少 DLL 或 Vulkan 设备时明确
//! 打印 SKIP 并返回，不伪造成通过；只要找到运行库，就必须真实编码/解码。

#![cfg(windows)]

use std::path::{Path, PathBuf};

use vc_media::{
    AU_HEADER_BYTES, AU_MAGIC, AuSession, CodecConfig, Decoder, Encoder, KIND_EOS, KIND_FRAME,
    MJPEG_IDENTITY, PYROWAVE_IDENTITY, build_au, capabilities, parse_au, pyrowave_frame_budget,
    validate,
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

fn turbojpeg_directory() -> Option<PathBuf> {
    find_library_dir(
        "VC_MEDIA_TURBOJPEG_DIR",
        &[
            "build/_deps/libjpeg-turbo-build/Release",
            "build/Release",
            "build",
        ],
        "turbojpeg.dll",
    )
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
        encoder_backend: MJPEG_IDENTITY.into(),
        decoder_backend: MJPEG_IDENTITY.into(),
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

fn flat_nv12(width: u32, height: u32, luma: u8, cb: u8, cr: u8) -> Vec<u8> {
    let y_bytes = width as usize * height as usize;
    let mut frame = vec![luma; y_bytes];
    for _ in 0..y_bytes / 4 {
        frame.push(cb);
        frame.push(cr);
    }
    frame
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
fn mjpeg_round_trip_validates_envelope_and_headers() {
    let Some(directory) = turbojpeg_directory() else {
        eprintln!("SKIP: 未找到 turbojpeg.dll（页面需打包/构建该运行库）");
        return;
    };
    let config = mjpeg_config();
    for (width, height) in [(2u32, 2u32), (66, 34), (640, 640)] {
        validate(&config, width, height, 30).unwrap();
        let mut encoder =
            Encoder::open_in_directory(&config, width, height, 30, &directory).unwrap();
        assert_eq!(encoder.settings()["family"], "turbojpeg");
        assert_eq!(encoder.settings()["identity"], MJPEG_IDENTITY);
        assert_eq!(encoder.settings()["quality"], 90);
        let input = flat_nv12(width, height, 96, 32, 208);
        let encoded = encoder.encode(&input, 7).unwrap();
        assert!(encoded.len() > AU_HEADER_BYTES);
        assert_eq!(
            u32::from_le_bytes(encoded[0..4].try_into().unwrap()),
            AU_MAGIC
        );
        assert_eq!(encoded[5], 3);
        assert_eq!(encoded[6], KIND_FRAME);
        let unit = parse_au(
            &encoded,
            AuSession::mjpeg(width, height, 30),
            7,
            config.max_au_bytes,
        )
        .unwrap();
        assert!(!unit.is_eos());

        let mut decoder =
            Decoder::open_in_directory(&config, width, height, 30, &directory).unwrap();
        assert_eq!(decoder.settings()["role"], "decoder");
        let decoded = decoder.decode(&encoded, 7).unwrap();
        assert_eq!(decoded.len(), input.len());
        for (index, (&source, &target)) in input.iter().zip(decoded.iter()).enumerate() {
            assert!(
                (i16::from(source) - i16::from(target)).abs() <= 2,
                "{width}x{height} 像素 {index}: {source} vs {target}"
            );
        }
        decoder.reset().unwrap();
        assert_eq!(decoder.decode(&encoded, 7).unwrap().len(), input.len());
        assert!(decoder.decode(&encoded, 8).is_err());
        assert!(decoder.decode(&encoded[..encoded.len() - 1], 7).is_err());
    }
}

#[test]
fn mjpeg_rejects_malformed_envelope_and_mismatched_jpeg() {
    let Some(directory) = turbojpeg_directory() else {
        eprintln!("SKIP: 未找到 turbojpeg.dll");
        return;
    };
    let config = mjpeg_config();
    let mut encoder = Encoder::open_in_directory(&config, 68, 34, 30, &directory).unwrap();
    let source = flat_nv12(68, 34, 80, 40, 200);
    let encoded = encoder.encode(&source, 11).unwrap();
    let payload = encoded[AU_HEADER_BYTES..].to_vec();

    // 封装尺寸与会话/JPEG 头不一致：信封合法，但 JPEG 实际是 68x34。
    let mismatched = build_au(AuSession::mjpeg(66, 34, 30), 11, &payload).unwrap();
    let mut decoder = Decoder::open_in_directory(&config, 66, 34, 30, &directory).unwrap();
    let error = decoder.decode(&mismatched, 11).unwrap_err().to_string();
    assert!(
        error.contains("JPEG") || error.contains("baseline"),
        "未拒绝尺寸不一致 JPEG: {error}"
    );

    let mut decoder = Decoder::open_in_directory(&config, 68, 34, 30, &directory).unwrap();
    // 信封/哈希/kind/帧号篡改全部拒绝。
    for mutate in [
        |bytes: &mut Vec<u8>| bytes[0] ^= 1,                   // magic
        |bytes: &mut Vec<u8>| bytes[32] ^= 1,                  // hash
        |bytes: &mut Vec<u8>| bytes[6] = KIND_EOS,             // kind 与载荷不符
        |bytes: &mut Vec<u8>| bytes[24] = 0,                   // frame id
        |bytes: &mut Vec<u8>| bytes[8] = 67,                   // width
        |bytes: &mut Vec<u8>| bytes[7] = 47,                   // header bytes
        |bytes: &mut Vec<u8>| bytes[28] = 1,                   // reserved
        |bytes: &mut Vec<u8>| *bytes.last_mut().unwrap() ^= 1, // payload
    ] {
        let mut bad = encoded.clone();
        mutate(&mut bad);
        assert!(decoder.decode(&bad, 11).is_err());
    }

    // 截断 JPEG（去掉 EOI），同步修正长度/哈希后仍必须被拒绝。
    let mut truncated = encoded.clone();
    truncated.truncate(truncated.len() - 2);
    let payload_len = (truncated.len() - AU_HEADER_BYTES) as u32;
    truncated[20..24].copy_from_slice(&payload_len.to_le_bytes());
    let hash = vc_transport::checksum::fnv1a64(&truncated[AU_HEADER_BYTES..]);
    truncated[32..40].copy_from_slice(&hash.to_le_bytes());
    assert!(decoder.decode(&truncated, 11).is_err());
    let empty_eos = build_au(AuSession::mjpeg(68, 34, 30), 12, &[]).unwrap();
    assert_eq!(empty_eos[6], KIND_EOS);
    assert!(decoder.decode(&empty_eos, 12).unwrap().is_empty());
}

#[test]
fn mjpeg_bounded_output_fails_instead_of_reallocating() {
    let Some(directory) = turbojpeg_directory() else {
        eprintln!("SKIP: 未找到 turbojpeg.dll");
        return;
    };
    let mut config = mjpeg_config();
    config.max_au_bytes = 1024;
    let mut encoder = Encoder::open_in_directory(&config, 640, 640, 30, &directory).unwrap();
    let input = ramp_nv12(640, 640);
    let error = encoder.encode(&input, 0).unwrap_err().to_string();
    assert!(
        error.contains("JPEG") || error.contains("超出") || error.contains("缓冲"),
        "未在固定输出上限处失败: {error}"
    );
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
    assert_eq!(caps["codecs"][0], "raw");
    assert_eq!(caps["codecs"][1], "mjpeg");
    assert_eq!(caps["codecs"][2], "pyrowave");
    assert_eq!(
        caps["hardware"]["encoder"]["declares_vendor_hardware"],
        false
    );
    assert_eq!(caps["libraries"]["turbojpeg"]["file"], "turbojpeg.dll");
    assert_eq!(
        caps["libraries"]["pyrowave"]["file"],
        "libpyrowave-shared-1.dll"
    );
}
