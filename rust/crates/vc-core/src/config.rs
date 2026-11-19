//! 配置模型与校验。文件为 ROOT/vc-rust-config.json（Rust 控制面独立文件，
//! 不读写旧 Python agent 的 vc-config.json）。
//!
//! 硬边界：损坏的 JSON 必须报错，不能静默回退默认值；保存必须是同目录临时文件 +
//! 原子替换，避免半截配置。编码草案可以保存在配置里，但 Rust R1 运行时只接受 raw
//! （`validate_raw`）。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 当前配置结构版本。不认识的版本不得被静默信任。
pub const CONFIG_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// 配置文件版本，当前为 [`CONFIG_VERSION`]。
    pub version: u32,
    /// 主机填写：被采集机控制地址，如 `http://192.168.2.59:5143`。
    pub peer_url: String,
    /// 被采集机填写：本机 Npcap 设备名（完整 `\Device\NPF_{GUID}` 或描述子串）。
    pub adapter: String,
    pub capture: Capture,
    pub transport: Transport,
    pub receive: Receive,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            peer_url: String::new(),
            adapter: String::new(),
            capture: Capture::default(),
            transport: Transport::default(),
            receive: Receive::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Capture {
    /// pattern | dda | wgc
    pub mode: String,
    /// 测试图案：motion 等，沿用原生 pattern。
    pub pattern: String,
    pub gpu_convert: bool,
    pub fps: u32,
    pub batch: u32,
    pub mtu: u32,
    pub dst_mac: String,
    /// DDA/WGC 显示器编号 0..15。
    pub output: u32,
    /// WGC 窗口标题子串；空表示整屏。
    pub wgc_window: String,
    pub width: u32,
    pub height: u32,
    /// crop | fit | fill | stretch
    pub transform: String,
    /// raw | mjpeg | pyrowave
    pub codec: String,
    pub bitrate_mbps: u32,
    pub max_bitrate_mbps: u32,
    pub jpeg_quality: u32,
    pub frames: u64,
    /// 编码链路必须显式启用（开发功能）。草案可保存，Rust R1 仍拒绝启动编码。
    pub codec_experimental: bool,
    pub encoder_backend: String,
    pub decoder_backend: String,
    pub decode_in_flight: u32,
    pub max_au_bytes: u32,
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            mode: "dda".into(),
            pattern: "motion".into(),
            gpu_convert: true,
            fps: 240,
            batch: 69,
            mtu: 9000,
            dst_mac: "ff:ff:ff:ff:ff:ff".into(),
            output: 0,
            wgc_window: String::new(),
            width: 640,
            height: 640,
            transform: "crop".into(),
            codec: "raw".into(),
            bitrate_mbps: 50,
            max_bitrate_mbps: 80,
            jpeg_quality: 80,
            frames: 0,
            codec_experimental: false,
            encoder_backend: String::new(),
            decoder_backend: String::new(),
            decode_in_flight: 2,
            max_au_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Transport {
    /// 预算预留百分比 0..50。
    pub headroom_pct: u32,
}

impl Default for Transport {
    fn default() -> Self {
        Self { headroom_pct: 20 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Receive {
    pub shm: String,
    pub mtu: u32,
    pub vcam_autoregister: bool,
}

impl Default for Receive {
    fn default() -> Self {
        Self {
            shm: r"Local\VirtualCaptureVcamFrame".into(),
            mtu: 9000,
            vcam_autoregister: true,
        }
    }
}

impl Config {
    /// 读取配置。文件不存在时返回默认配置；存在但 JSON/结构损坏或版本未知时返回
    /// `InvalidData`，绝不静默回退默认值。
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error),
        };
        let config: Config = serde_json::from_str(&text).map_err(|error| {
            invalid_data(format!("配置文件 {} 无法解析：{error}", path.display()))
        })?;
        if config.version != CONFIG_VERSION {
            return Err(invalid_data(format!(
                "配置文件 {} 版本为 {}，本程序只支持版本 {}；不会猜测未知字段的含义。",
                path.display(),
                config.version,
                CONFIG_VERSION
            )));
        }
        Ok(config)
    }

    /// 读取 ROOT 配置；错误必须由调用方显式处理（不能静默使用默认值）。
    pub fn load_root() -> std::io::Result<Self> {
        Self::load(&crate::paths::config_path())
    }

    /// 原子保存：写入同目录临时文件、刷盘，再替换目标文件。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| invalid_data(format!("配置无法序列化：{error}")))?;
        let tmp = temp_path(path);
        let write_result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })();
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        Ok(())
    }

    pub fn save_root(&self) -> std::io::Result<()> {
        self.save(&crate::paths::config_path())
    }

    /// 应用部分配置：对象逐键合并，标量整体替换；未知键丢弃。
    /// 合并结果必须通过 [`Self::validate`] 才会写入 `self`。
    pub fn apply_patch(&mut self, patch: &Value) -> Vec<String> {
        let current = match serde_json::to_value(&*self) {
            Ok(value) => value,
            Err(error) => return vec![format!("当前配置无法序列化：{error}")],
        };
        let merged = merge_value(current, patch);
        match serde_json::from_value::<Config>(merged) {
            Ok(next) => {
                let errors = next.validate();
                if errors.is_empty() {
                    *self = next;
                }
                errors
            }
            Err(error) => vec![format!("配置格式无效：{error}")],
        }
    }

    /// 结构/取值范围校验。编码草案（codec=mjpeg/pyrowave）在参数合法时允许保存；
    /// Rust R1 能否启动由 [`Self::validate_raw`] 判定。
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let c = &self.capture;

        if self.version != CONFIG_VERSION {
            errors.push(format!(
                "配置版本必须是 {CONFIG_VERSION}（当前 {}）。",
                self.version
            ));
        }
        if !self.peer_url.is_empty() && !valid_peer_url(&self.peer_url) {
            errors.push("peer_url 必须是 http://IPv4:PORT（端口 1..65535）。".into());
        }
        if !valid_adapter(&self.adapter) {
            errors.push(
                "adapter 必须是空值、<=64 字符的 [A-Za-z0-9 _.()-]，或完整 \\Device\\NPF_{GUID}/Loopback 设备名。"
                    .into(),
            );
        }
        if !matches!(c.mode.as_str(), "pattern" | "dda" | "wgc") {
            errors.push("capture.mode 必须是 pattern/dda/wgc。".into());
        }
        if !matches!(
            c.pattern.as_str(),
            "counter" | "checker" | "rings" | "noise" | "motion"
        ) {
            errors.push("capture.pattern 必须是 counter/checker/rings/noise/motion。".into());
        }
        if c.output > 15 {
            errors.push("capture.output 必须是 0..15。".into());
        }
        if !valid_wgc_window(&c.wgc_window) {
            errors.push("capture.wgc_window 必须是不超过 128 字符的单行文本。".into());
        }
        if !(2..=8192).contains(&c.width) || !c.width.is_multiple_of(2) {
            errors.push("capture.width 必须是 2..8192 的偶数。".into());
        }
        if !(2..=8192).contains(&c.height) || !c.height.is_multiple_of(2) {
            errors.push("capture.height 必须是 2..8192 的偶数。".into());
        }
        if !(1..=1000).contains(&c.fps) {
            errors.push("capture.fps 必须是 1..1000。".into());
        }
        if !(1..=1000).contains(&c.batch) {
            errors.push("capture.batch 必须是 1..1000。".into());
        }
        if !(1500..=9000).contains(&c.mtu) {
            errors.push("capture.mtu 必须是 1500..9000。".into());
        }
        if !(1500..=9000).contains(&self.receive.mtu) {
            errors.push("receive.mtu 必须是 1500..9000。".into());
        }
        if !valid_mac(&c.dst_mac) {
            errors.push("capture.dst_mac 必须是 aa:bb:cc:dd:ee:ff。".into());
        }
        if !valid_shm(&self.receive.shm) {
            errors
                .push("receive.shm 必须是 Local\\<name> 或 Global\\<name>（1..64 字符）。".into());
        }
        if !matches!(c.transform.as_str(), "crop" | "fit" | "fill" | "stretch") {
            errors.push("capture.transform 必须是 crop/fit/fill/stretch。".into());
        }
        if !matches!(c.codec.as_str(), "raw" | "mjpeg" | "pyrowave") {
            errors.push("capture.codec 必须是 raw/mjpeg/pyrowave。".into());
        }
        for (name, value) in [
            ("encoder_backend", c.encoder_backend.as_str()),
            ("decoder_backend", c.decoder_backend.as_str()),
        ] {
            if !matches!(value, "" | "turbojpeg:cpu" | "pyrowave:vulkan") {
                errors.push(format!(
                    "capture.{name} 必须是空值、turbojpeg:cpu 或 pyrowave:vulkan；已移除其他编解码后端。"
                ));
            }
        }
        let expected_backend = match c.codec.as_str() {
            "mjpeg" => Some("turbojpeg:cpu"),
            "pyrowave" => Some("pyrowave:vulkan"),
            _ => None,
        };
        if let Some(expected) = expected_backend {
            for (name, value) in [
                ("encoder_backend", c.encoder_backend.as_str()),
                ("decoder_backend", c.decoder_backend.as_str()),
            ] {
                if !value.is_empty() && value != expected {
                    errors.push(format!(
                        "capture.{name} 与 codec={} 不匹配，必须为 {expected}。",
                        c.codec
                    ));
                }
            }
        }
        if !(1..=8).contains(&c.decode_in_flight) {
            errors.push("capture.decode_in_flight 必须是 1..8。".into());
        }
        if !(1024..=16_777_216).contains(&c.max_au_bytes) {
            errors.push("capture.max_au_bytes 必须是 1024..16777216。".into());
        }
        if !(1..=100).contains(&c.jpeg_quality) {
            errors.push("capture.jpeg_quality 必须是 1..100。".into());
        }
        if !(1..=10000).contains(&c.bitrate_mbps) {
            errors.push("capture.bitrate_mbps 必须是 1..10000。".into());
        }
        if !(1..=10000).contains(&c.max_bitrate_mbps) {
            errors.push("capture.max_bitrate_mbps 必须是 1..10000。".into());
        }
        if c.codec == "raw" && c.max_bitrate_mbps < c.bitrate_mbps {
            errors.push("Raw 仍要求最大码率不低于目标码率。".into());
        }
        if !(0..=50).contains(&self.transport.headroom_pct) {
            errors.push("transport.headroom_pct 必须是 0..50。".into());
        }
        errors
    }

    /// Rust R1 运行时门限：先做完整结构校验，再强制只允许 raw。
    /// 配置可以保存编码草案，但 Rust 控制面不得启动编码链路。
    pub fn validate_raw(&self) -> Vec<String> {
        let mut errors = self.validate();
        if self.capture.codec != "raw" {
            errors.push(format!(
                "Rust 控制面第一版只支持 raw，不能启动 codec={}；请使用现有 agent，或把 codec 改回 raw。",
                self.capture.codec
            ));
        }
        errors
    }
}

fn invalid_data(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn temp_path(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "vc-rust-config.json".into());
    let tmp_name = format!(".{name}.{}.{nanos}.tmp", std::process::id());
    path.with_file_name(tmp_name)
}

pub fn valid_peer_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let Some((host, port)) = rest.rsplit_once(':') else {
        return false;
    };
    if host.parse::<std::net::Ipv4Addr>().is_err() {
        return false;
    }
    matches!(port.parse::<u16>(), Ok(port) if port != 0)
}

fn valid_npf_device(value: &str) -> bool {
    let Some(rest) = value.strip_prefix(r"\Device\NPF_") else {
        return false;
    };
    if rest == "Loopback" {
        return true;
    }
    let Some(inner) = rest
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return false;
    };
    inner.len() == 36 && inner.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-')
}

fn valid_adapter(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    if valid_npf_device(value) {
        return true;
    }
    let mut count = 0usize;
    for ch in value.chars() {
        if !(ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '_' | '.' | '(' | ')' | '-')) {
            return false;
        }
        count += 1;
        if count > 64 {
            return false;
        }
    }
    true
}

fn valid_mac(value: &str) -> bool {
    let parts: Vec<&str> = value.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|part| part.len() == 2 && part.chars().all(|ch| ch.is_ascii_hexdigit()))
}

fn valid_shm(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("Local\\")
        .or_else(|| value.strip_prefix("Global\\"))
    else {
        return false;
    };
    let count = rest.chars().count();
    (1..=64).contains(&count)
        && rest
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
}

fn valid_wgc_window(value: &str) -> bool {
    value.chars().count() <= 128 && !value.chars().any(|ch| (ch as u32) < 32)
}

fn merge_value(base: Value, patch: &Value) -> Value {
    match (base, patch) {
        (Value::Object(mut base), Value::Object(patch)) => {
            for (key, value) in patch {
                let slot = base.remove(key).unwrap_or(Value::Null);
                base.insert(key.clone(), merge_value(slot, value));
            }
            Value::Object(base)
        }
        (_, patch) => patch.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "vc-core-config-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_match_product_config() {
        let config = Config::default();
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.capture.width, 640);
        assert_eq!(config.capture.fps, 240);
        assert_eq!(config.capture.transform, "crop");
        assert_eq!(config.capture.codec, "raw");
        assert!(config.validate().is_empty());
        assert!(config.validate_raw().is_empty());
    }

    #[test]
    fn patch_merges_nested_objects_and_drops_unknown_keys() {
        let mut config = Config::default();
        let errors = config.apply_patch(&json!({
            "capture": {"width": 416, "height": 416, "bogus": 1},
            "transport": {"headroom_pct": 30},
            "unknown_top": true
        }));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(config.capture.width, 416);
        assert_eq!(config.capture.height, 416);
        assert_eq!(config.capture.fps, 240);
        assert_eq!(config.transport.headroom_pct, 30);
    }

    #[test]
    fn rejects_bad_geometry_but_preserves_encoded_draft() {
        let mut config = Config::default();
        let errors = config.apply_patch(&json!({
            "capture": {"width": 417, "height": 0}
        }));
        assert!(errors.iter().any(|e| e.contains("width")));
        assert!(errors.iter().any(|e| e.contains("height")));
        assert_eq!(config.capture.width, 640);

        // 编码草案参数合法时可以保存，但 Rust R1 运行时门限拒绝启动。
        let errors = config.apply_patch(&json!({
            "capture": {
                "codec": "mjpeg",
                "encoder_backend": "turbojpeg:cpu",
                "decoder_backend": "turbojpeg:cpu"
            }
        }));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(config.capture.codec, "mjpeg");
        assert!(
            config
                .validate_raw()
                .iter()
                .any(|e| e.contains("只支持 raw"))
        );
    }

    #[test]
    fn peer_url_validation() {
        assert!(valid_peer_url("http://192.168.2.59:5143"));
        assert!(!valid_peer_url("https://192.168.2.59:5143"));
        assert!(!valid_peer_url("http://192.168.2.59"));
        assert!(!valid_peer_url("http://host:5143"));
        assert!(!valid_peer_url("http://192.168.2.59:0"));
        assert!(!valid_peer_url("http://192.168.2.59:5143/"));
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let dir = temp_dir("missing");
        let config = Config::load(&dir.join("nope.json")).unwrap();
        assert_eq!(config, Config::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_corrupt_file_errors_instead_of_defaulting() {
        let dir = temp_dir("corrupt");
        let path = dir.join("vc-rust-config.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let error = Config::load(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("无法解析"));

        std::fs::write(&path, json!({"capture": {"width": "640"}}).to_string()).unwrap();
        let error = Config::load(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_applies_defaults_and_ignores_unknown_fields() {
        let dir = temp_dir("partial");
        let path = dir.join("vc-rust-config.json");
        std::fs::write(
            &path,
            json!({
                "capture": {"width": 416, "height": 416},
                "legacy_role": "capture",
                "gop_frames": 1
            })
            .to_string(),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.capture.width, 416);
        assert_eq!(config.capture.fps, 240);
        assert_eq!(config.capture.mode, "dda");
        assert_eq!(config.capture.bitrate_mbps, 50);
        assert_eq!(config.transport.headroom_pct, 20);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_rejects_unknown_version() {
        let dir = temp_dir("version");
        let path = dir.join("vc-rust-config.json");
        std::fs::write(&path, json!({"version": 2}).to_string()).unwrap();
        let error = Config::load(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("版本"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_is_atomic_and_roundtrips() {
        let dir = temp_dir("save");
        let path = dir.join("vc-rust-config.json");
        let mut config = Config::default();
        config.capture.width = 1280;
        config.capture.height = 720;
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, config);

        // 覆盖已有文件也必须原子替换成功。
        config.capture.width = 640;
        config.capture.height = 640;
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.capture.width, 640);

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validates_out_of_range_fields() {
        let mut config = Config::default();
        config.capture.output = 16;
        config.capture.pattern = "bogus".into();
        config.capture.batch = 0;
        config.receive.mtu = 42;
        config.capture.dst_mac = "gg:00:00:00:00:00".into();
        config.receive.shm = "BadName".into();
        config.capture.wgc_window = "line\nbreak".into();
        let errors = config.validate();
        for needle in [
            "output",
            "pattern",
            "batch",
            "receive.mtu",
            "dst_mac",
            "receive.shm",
            "wgc_window",
        ] {
            assert!(
                errors.iter().any(|error| error.contains(needle)),
                "missing {needle}: {errors:?}"
            );
        }
    }

    #[test]
    fn raw_requires_max_bitrate_at_least_target() {
        let mut config = Config::default();
        config.capture.bitrate_mbps = 80;
        config.capture.max_bitrate_mbps = 50;
        assert!(config.validate().iter().any(|e| e.contains("最大码率")));

        // 编码后端允许目标码率高于最大码率（原生独立限流）。
        config.capture.codec = "mjpeg".into();
        config.capture.encoder_backend = "turbojpeg:cpu".into();
        config.capture.decoder_backend = "turbojpeg:cpu".into();
        assert!(!config.validate().iter().any(|e| e.contains("最大码率")));
    }

    #[test]
    fn encoded_backend_must_match_codec() {
        let mut config = Config::default();
        config.capture.codec = "pyrowave".into();
        config.capture.encoder_backend = "turbojpeg:cpu".into();
        config.capture.decoder_backend = "pyrowave:vulkan".into();
        let errors = config.validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("encoder_backend") && e.contains("不匹配"))
        );
    }

    #[test]
    fn adapter_selector_and_npf_names() {
        assert!(valid_adapter(""));
        assert!(valid_adapter("Intel I226-V (Ethernet)"));
        assert!(valid_adapter(
            r"\Device\NPF_{12345678-1234-1234-1234-1234567890AB}"
        ));
        assert!(valid_adapter(r"\Device\NPF_Loopback"));
        assert!(!valid_adapter("bad|name"));
        assert!(!valid_adapter(r"\Device\NPF_{not-a-guid}"));
        assert!(!valid_adapter(&"x".repeat(65)));
    }
}
