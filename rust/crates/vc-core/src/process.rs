//! 原生进程槽位：拉起/停止 `vc-sender.exe` / `vc-receiver.exe`，读取 stats 与日志尾部。
//!
//! 停止语义：CTRL_BREAK 到进程组，让原生程序刷完统计；2 秒不退再强杀。
//! 两个 Rust 程序启动时都会分配隐藏控制台（`ensure_hidden_console`），
//! 子进程继承它，因此 GUI 宿主也能发送控制台事件而不闪窗。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::config::Config;
use crate::paths;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Sender,
    Receiver,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Sender => "采集",
            Kind::Receiver => "接收",
        }
    }

    pub fn exe_name(self) -> &'static str {
        match self {
            Kind::Sender => "vc-sender.exe",
            Kind::Receiver => "vc-receiver.exe",
        }
    }
}

pub struct NativeSlot {
    pub kind: Kind,
    pub session_id: Option<String>,
    pub stats_path: PathBuf,
    pub hashlog_path: PathBuf,
    pub stdout_path: PathBuf,
    pub adapter: String,
    pub mode: Option<String>,
    pub pattern: Option<String>,
    child: Option<Child>,
    started_at: Option<Instant>,
    exit: Option<i32>,
}

impl NativeSlot {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            session_id: None,
            stats_path: PathBuf::new(),
            hashlog_path: PathBuf::new(),
            stdout_path: PathBuf::new(),
            adapter: String::new(),
            mode: None,
            pattern: None,
            child: None,
            started_at: None,
            exit: None,
        }
    }

    pub fn running(&mut self) -> bool {
        self.poll();
        self.child.is_some()
    }

    fn poll(&mut self) {
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.exit = Some(status.code().unwrap_or(-1));
                    self.child = None;
                }
                Ok(None) => {}
                Err(_) => {
                    self.exit = Some(-1);
                    self.child = None;
                }
            }
        }
    }

    /// 先停旧槽位，再以新会话拉起原生程序。
    pub fn start(
        &mut self,
        args: &[String],
        adapter: &str,
        session_id: &str,
        run_dir: &Path,
    ) -> Result<u32, String> {
        let exe = paths::native_exe(self.kind.exe_name());
        if !exe.exists() {
            return Err(format!(
                "找不到 {}；请先按 docs/runbooks/build.md 构建原生程序（查找位置：{}）",
                self.kind.exe_name(),
                exe.display()
            ));
        }
        let (stats_path, hashlog_path, stdout_path) = session_paths(run_dir, session_id, self.kind);
        if [&stats_path, &hashlog_path, &stdout_path]
            .iter()
            .any(|path| path.exists())
        {
            return Err("会话号已有运行证据，请使用新的 session_id；未覆盖旧文件。".into());
        }
        self.stop();
        std::fs::create_dir_all(run_dir).map_err(|error| format!("无法创建 run 目录：{error}"))?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&hashlog_path)
            .map_err(|error| format!("无法创建 hashlog：{error}"))?;
        let log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&stdout_path)
            .map_err(|error| format!("无法创建日志：{error}"))?;
        let log2 = log
            .try_clone()
            .map_err(|error| format!("无法复制日志句柄：{error}"))?;

        let mut command = Command::new(&exe);
        command
            .args(args)
            .current_dir(paths::root())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("无法启动 {}：{error}", self.kind.exe_name()))?;
        let pid = child.id();
        self.child = Some(child);
        self.started_at = Some(Instant::now());
        self.exit = None;
        self.session_id = Some(session_id.to_string());
        self.stats_path = stats_path;
        self.hashlog_path = hashlog_path;
        self.stdout_path = stdout_path;
        self.adapter = adapter.to_string();
        let deadline = Instant::now() + Duration::from_millis(350);
        while Instant::now() < deadline {
            if !self.running() {
                return Err(format!(
                    "{}程序启动后退出（退出码 {:?}）：{}",
                    self.kind.label(),
                    self.exit,
                    self.log_tail().unwrap_or_default()
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(pid)
    }

    /// 带历史会话信息重启（由调用方传入 args）。
    pub fn mark_mode(&mut self, mode: Option<String>, pattern: Option<String>) {
        self.mode = mode;
        self.pattern = pattern;
    }

    /// 幂等停止：CTRL_BREAK → 2s → kill。
    pub fn stop(&mut self) -> Option<String> {
        self.poll();
        let session = self.session_id.clone();
        if let Some(mut child) = self.child.take() {
            #[cfg(windows)]
            unsafe {
                use windows_sys::Win32::System::Console::{
                    CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent,
                };
                GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id());
            }
            #[cfg(not(windows))]
            {
                let _ = child.kill();
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            let code = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status.code().unwrap_or(-1)),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    _ => {
                        let _ = child.kill();
                        break child.wait().ok().and_then(|status| status.code());
                    }
                }
            };
            self.exit = code;
        }
        self.started_at = None;
        session
    }

    /// 原生 stats 文件 + 进程状态。文件缺失或损坏时仍返回运行状态。
    pub fn stats(&mut self) -> Value {
        self.poll();
        let running = self.child.is_some();
        let mut map = match std::fs::read_to_string(&self.stats_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        map.insert(
            "running".into(),
            Value::Bool(running && map.get("running").and_then(Value::as_bool).unwrap_or(true)),
        );
        map.insert(
            "session_id".into(),
            Value::String(self.session_id.clone().unwrap_or_default()),
        );
        map.insert(
            "uptime_s".into(),
            Value::from(
                self.started_at
                    .map(|start| (start.elapsed().as_millis() as f64) / 1000.0)
                    .unwrap_or(0.0),
            ),
        );
        map.insert("adapter".into(), Value::String(self.adapter.clone()));
        if self.kind == Kind::Sender {
            if let Some(mode) = &self.mode {
                map.insert("mode".into(), Value::String(mode.clone()));
            }
            if let Some(pattern) = &self.pattern {
                map.insert("pattern".into(), Value::String(pattern.clone()));
            }
        }
        if let Some(child) = &self.child {
            map.insert("pid".into(), Value::from(child.id()));
            map.insert("exit_code".into(), Value::Null);
        } else if let Some(code) = self.exit {
            map.insert("exit_code".into(), Value::from(code));
            if code != 0 {
                map.insert(
                    "error".into(),
                    Value::String(if code == 124 {
                        format!(
                            "{}程序编解码操作超过 5 秒，看门狗强制终止（退出码 124）。",
                            self.kind.label()
                        )
                    } else {
                        format!(
                            "{}程序异常退出（退出码 {code}），请检查运行日志。",
                            self.kind.label()
                        )
                    }),
                );
                if let Some(tail) = self.log_tail() {
                    map.insert("log_tail".into(), Value::String(tail));
                }
                if let Ok(rel) = self.stdout_path.strip_prefix(paths::root()) {
                    map.insert(
                        "stdout".into(),
                        Value::String(rel.to_string_lossy().replace('\\', "/")),
                    );
                }
            }
        }
        Value::Object(map)
    }

    fn log_tail(&self) -> Option<String> {
        let mut file = File::open(&self.stdout_path).ok()?;
        let size = file.metadata().ok()?.len();
        file.seek(SeekFrom::Start(size.saturating_sub(4096))).ok()?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).ok()?;
        let text = String::from_utf8_lossy(&buffer);
        let text = text.trim_end();
        let tail: String = text
            .chars()
            .rev()
            .take(2048)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Some(tail)
    }

    /// stats 数值读取助手（UI 用）。
    pub fn value_f64(stats: &Value, keys: &[&str]) -> Option<f64> {
        keys.iter()
            .find_map(|key| stats.get(*key).and_then(Value::as_f64))
    }

    pub fn value_u64(stats: &Value, keys: &[&str]) -> Option<u64> {
        keys.iter().find_map(|key| {
            stats.get(*key).and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_f64().map(|f| f.max(0.0) as u64))
            })
        })
    }
}

/// 会话文件路径：`<sid>.<kind>.{stats.json,hash.jsonl,stdout.log}`。
pub fn session_paths(run_dir: &Path, session_id: &str, kind: Kind) -> (PathBuf, PathBuf, PathBuf) {
    let tag = match kind {
        Kind::Sender => "sender",
        Kind::Receiver => "receiver",
    };
    (
        run_dir.join(format!("{session_id}.{tag}.stats.json")),
        run_dir.join(format!("{session_id}.{tag}.hash.jsonl")),
        run_dir.join(format!("{session_id}.{tag}.stdout.log")),
    )
}

pub fn new_session_id(kind: Kind) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let tag = match kind {
        Kind::Sender => "s",
        Kind::Receiver => "r",
    };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in nanos.to_le_bytes().iter().chain(pid.to_le_bytes().iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{tag}-{hash:016x}")
}

/// 采集端 sender 参数。仅 raw；编码路径在 Rust 控制面接入前明确拒绝。
pub fn sender_args(
    config: &Config,
    adapter: &str,
    session: &str,
    stats: &Path,
    hashlog: &Path,
    source_mac: Option<&str>,
) -> Result<Vec<String>, String> {
    let c = &config.capture;
    validate_raw_config(config)?;
    let mut args: Vec<String> = vec![
        "--adapter".into(),
        adapter.into(),
        "--mode".into(),
        c.mode.clone(),
        "--pattern".into(),
        c.pattern.clone(),
        "--fps".into(),
        c.fps.to_string(),
        "--frames".into(),
        c.frames.to_string(),
        "--ethertype".into(),
        "0x88B5".into(),
        "--mtu".into(),
        c.mtu.to_string(),
        "--pacing-us".into(),
        "0".into(),
        "--dst-mac".into(),
        c.dst_mac.clone(),
        "--session".into(),
        session.into(),
        "--stats".into(),
        stats.display().to_string(),
        "--hashlog".into(),
        hashlog.display().to_string(),
        "--batch".into(),
        c.batch.to_string(),
        "--width".into(),
        c.width.to_string(),
        "--height".into(),
        c.height.to_string(),
        "--transform".into(),
        c.transform.clone(),
    ];
    if c.gpu_convert {
        args.push("--gpu-convert".into());
    }
    if c.output != 0 {
        args.extend(["--output".into(), c.output.to_string()]);
    }
    if c.mode == "wgc" && !c.wgc_window.is_empty() {
        args.extend(["--wgc-window".into(), c.wgc_window.clone()]);
    }
    if let Some(mac) = source_mac {
        args.extend(["--source-mac".into(), mac.to_string()]);
    }
    Ok(args)
}

impl Drop for NativeSlot {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 两端启动前共同校验，不允许把编码流当成 Raw。
pub fn validate_raw_config(config: &Config) -> Result<(), String> {
    let errors = config.validate_raw();
    if !errors.is_empty() {
        return Err(errors.join("；"));
    }
    if config.capture.mtu != config.receive.mtu {
        return Err("采集与接收 MTU 必须一致。".into());
    }
    Ok(())
}

/// 验证实际原生程序能力，不只相信控制面版本。
pub fn require_raw_capabilities(kind: Kind) -> Result<(), String> {
    let exe = paths::native_exe(kind.exe_name());
    let output = run_bounded(
        Command::new(&exe).arg("--capabilities"),
        Duration::from_secs(5),
    )?;
    if !output.status.success() {
        return Err(format!("{} 能力探测失败", exe.display()));
    }
    let caps: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("原生能力响应无效：{error}"))?;
    if caps["stream_revision"] != 2
        || caps["variable_resolution"] != true
        || caps["shm_version"] != 3
    {
        return Err("原生程序缺少 stream_revision=2 / 可变尺寸 / SHM v3 能力。".into());
    }
    Ok(())
}

/// 有界执行诊断命令，每路输出保留至多 1 MiB，超时终止并回收。
pub fn run_bounded(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("无法启动诊断程序：{e}"))?;
    fn reader(stream: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let mut stream = stream;
            let mut chunk = [0u8; 8192];
            while let Ok(n) = stream.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let keep = n.min(1_048_576usize.saturating_sub(data.len()));
                data.extend_from_slice(&chunk[..keep]);
            }
            data
        })
    }
    let stdout = reader(child.stdout.take().ok_or("无法读取标准输出")?);
    let stderr = reader(child.stderr.take().ok_or("无法读取标准错误")?);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("诊断程序执行超时或无法读取进程状态。".into());
            }
        }
    };
    let drain_deadline = Instant::now() + Duration::from_secs(1);
    while !stdout.is_finished() || !stderr.is_finished() {
        if Instant::now() >= drain_deadline {
            return Err("诊断程序已退出，但输出管道未关闭。".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(Output {
        status,
        stdout: stdout.join().map_err(|_| "读取诊断标准输出失败")?,
        stderr: stderr.join().map_err(|_| "读取诊断标准错误失败")?,
    })
}

/// 主机 receiver 参数。
pub fn receiver_args(
    config: &Config,
    adapter: &str,
    session: &str,
    stats: &Path,
    hashlog: &Path,
    source_mac: Option<&str>,
) -> Vec<String> {
    let c = &config.capture;
    let mut args: Vec<String> = vec![
        "--adapter".into(),
        adapter.into(),
        "--ethertype".into(),
        "0x88B5".into(),
        "--mtu".into(),
        config.receive.mtu.to_string(),
        "--session".into(),
        session.into(),
        "--stats".into(),
        stats.display().to_string(),
        "--hashlog".into(),
        hashlog.display().to_string(),
        "--timeout-s".into(),
        "0".into(),
        "--shm".into(),
        config.receive.shm.clone(),
        "--width".into(),
        c.width.to_string(),
        "--height".into(),
        c.height.to_string(),
        "--fps".into(),
        c.fps.to_string(),
    ];
    if let Some(mac) = source_mac {
        args.extend(["--source-mac".into(), mac.to_string()]);
    }
    args
}

/// 随机本地管理源 MAC（流隔离标识，不是认证）。
pub fn random_source_mac() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut value = nanos as u64 ^ (u64::from(std::process::id()) << 32);
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    let bytes = value.to_be_bytes();
    format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
    )
}

/// 为当前进程分配隐藏控制台，使子进程继承并支持 CTRL_BREAK。
pub fn ensure_hidden_console() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{AllocConsole, GetConsoleWindow};
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
        if GetConsoleWindow().is_null() {
            let _ = AllocConsole();
            let window = GetConsoleWindow();
            if !window.is_null() {
                ShowWindow(window, SW_HIDE);
            }
        }
    }
}

/// 写一行到 stderr 并忽略错误（CLI 提示用）。
pub fn note(message: &str) {
    let _ = writeln!(std::io::stderr(), "{message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_unique_and_tagged() {
        let a = new_session_id(Kind::Sender);
        let b = new_session_id(Kind::Sender);
        assert!(a.starts_with("s-"));
        assert_ne!(a, b);
        assert!(new_session_id(Kind::Receiver).starts_with("r-"));
    }

    #[test]
    fn source_mac_is_locally_administered() {
        let mac = random_source_mac();
        assert!(mac.starts_with("02:"));
        assert_eq!(mac.split(':').count(), 6);
    }

    #[test]
    fn sender_args_reject_encoded_codec() {
        let mut config = Config::default();
        config.capture.codec = "mjpeg".into();
        let error = sender_args(
            &config,
            "x",
            "s-1",
            Path::new("s.json"),
            Path::new("h.jsonl"),
            None,
        )
        .unwrap_err();
        assert!(error.contains("只支持 raw"));
    }

    #[test]
    fn sender_args_carry_explicit_geometry() {
        let config = Config::default();
        let args = sender_args(
            &config,
            r"\Device\NPF_Loopback",
            "s-1",
            Path::new("run/s.json"),
            Path::new("run/h.jsonl"),
            Some("02:00:00:00:00:01"),
        )
        .unwrap();
        assert!(args.windows(2).any(|w| w[0] == "--width" && w[1] == "640"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--transform" && w[1] == "crop")
        );
        assert!(args.iter().any(|a| a == "--gpu-convert"));
        assert!(args.windows(2).any(|w| w[0] == "--source-mac"));
    }
}
