//! Standalone driver for `rust/tests/host_update_smoke.py`.
//!
//! It is not part of the rust workspace; the Python smoke test copies this
//! package to `run/`, points the `vc-core` path at the checkout, and builds it
//! with an independent target directory. The process then drives the real
//! `HostController` worker and exits after local activation requests a restart,
//! letting `vc-updater` install and start the new GPUI host.
//!
//! No NIC capture, DShow registration or PC2 connection is performed. The peer
//! is a private `vc-capture.exe` root on a random loopback port.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vc_core::host::{self, Command, HostController, Snapshot};
use vc_core::paths;

struct Args {
    host_root: PathBuf,
    bin_dir: PathBuf,
    peer_url: String,
    peer_token: String,
    peer_zip: PathBuf,
    peer_build: String,
    local_zip: PathBuf,
    local_build: String,
    result: PathBuf,
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(message) => {
            eprintln!("host-update-fixture: {message}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    paths::set_root_override(args.host_root.clone());
    paths::set_native_dir_override(args.bin_dir.clone());
    let controller = host::spawn();
    let mut harness = Harness {
        controller,
        evidence: Vec::new(),
    };

    // --- 令牌格式与本地/对端命令的无效状态拒绝 -----------------------------
    // SetPeerUpdateToken reports format errors in update_message without an event.
    harness.send(Command::SetPeerUpdateToken("not-a-token".into()));
    let snapshot = harness.wait_message(
        "无效令牌被拒绝",
        Duration::from_secs(10),
        |m| m.contains("格式无效"),
    )?;
    if snapshot.update_token_paired {
        return Err("无效令牌竟被标记为已配对".into());
    }
    if paths::peer_update_token_path().exists() {
        return Err("无效令牌被写入了令牌文件".into());
    }

    let missing = args.host_root.join("missing-package.zip");
    harness.send(Command::ActivatePeerUpdate("missing-build".into()));
    harness.wait_message(
        "未连接时拒绝激活对端更新",
        Duration::from_secs(10),
        |m| m.contains("请先连接被采集机"),
    )?;

    let before = harness.event_count();
    harness.send(Command::Connect("not-an-address".into()));
    harness.wait_event(
        before,
        "无效对端地址被拒绝",
        Duration::from_secs(10),
        |s| {
            s.error
                .as_deref()
                .is_some_and(|m| m.contains("有效的被采集机 IPv4 地址"))
        },
    )?;

    let before = harness.event_count();
    harness.send(Command::StageLocalUpdate(missing));
    harness.wait_message(
        "缺少本机更新包时报错",
        Duration::from_secs(10),
        |m| m.contains("无法读取上传文件"),
    )?;
    if before != harness.event_count() {
        return Err("本机暂存失败不应产生外部事件".into());
    }

    harness.send(Command::ActivateLocalUpdate("missing-local-build".into()));
    harness.wait_message(
        "未暂存的本机版本被拒绝",
        Duration::from_secs(10),
        |m| m.contains("尚未暂存"),
    )?;
    if harness.snapshot().restart_requested {
        return Err("未暂存版本触发了重启".into());
    }

    // --- 有效配对、连接与对端更新 -----------------------------------------
    harness.send(Command::SetPeerUpdateToken(args.peer_token.clone()));
    harness.wait_for(
        "有效令牌保存",
        Duration::from_secs(10),
        |s| {
            s.update_token_paired
                && s.update_message
                    .as_deref()
                    .is_some_and(|m| m.contains("已保存"))
        },
    )?;
    if vc_core::update::load_peer_token().as_deref() != Some(args.peer_token.as_str()) {
        return Err("保存后的对端令牌无法读回".into());
    }

    harness.send(Command::Connect(args.peer_url.clone()));
    let snapshot = harness.wait_for(
        "连接私有 vc-capture",
        Duration::from_secs(40),
        |s| s.connected && s.peer_online && !s.busy,
    )?;
    harness.evidence.push(format!(
        "connected peer version={} url={}",
        snapshot.peer_version, snapshot.peer_url
    ));

    let junk = args.host_root.join("not-a-zip.zip");
    std::fs::write(&junk, b"this is not a zip archive")
        .map_err(|e| format!("写入无效 ZIP 失败：{e}"))?;
    harness.send(Command::StagePeerUpdate(junk));
    harness.wait_message(
        "无效对端更新包被拒绝",
        Duration::from_secs(20),
        |m| m.contains("ZIP"),
    )?;

    harness.send(Command::ActivatePeerUpdate(
        "missing-peer-build".into(),
    ));
    harness.wait_message(
        "未暂存的对端版本被拒绝",
        Duration::from_secs(20),
        |m| m.contains("尚未暂存"),
    )?;

    harness.send(Command::StagePeerUpdate(args.peer_zip.clone()));
    harness.wait_for(
        "对端更新暂存",
        Duration::from_secs(60),
        |s| {
            !s.busy
                && s.update_message
                    .as_deref()
                    .is_some_and(|m| m.contains("被采集机更新已暂存"))
                && s.peer_update
                    .get("staged")
                    .and_then(|v| v.get("build_id"))
                    .and_then(|v| v.as_str())
                    == Some(args.peer_build.as_str())
        },
    )?;
    harness
        .evidence
        .push(format!("peer staged build={}", args.peer_build));

    harness.send(Command::ActivatePeerUpdate(args.peer_build.clone()));
    let snapshot = harness.wait_for(
        "对端更新切换完成",
        Duration::from_secs(150),
        |s| {
            !s.busy
                && s.peer_update
                    .get("active")
                    .and_then(|v| v.get("build_id"))
                    .and_then(|v| v.as_str())
                    == Some(args.peer_build.as_str())
        },
    )?;
    if !snapshot
        .update_message
        .as_deref()
        .is_some_and(|m| m.contains("被采集机更新就绪"))
    {
        return Err(format!(
            "对端更新已切换但 HostController 报错：{}",
            snapshot.update_message.unwrap_or_default()
        ));
    }
    harness.evidence.push(format!(
        "peer active build={} version={}",
        args.peer_build, snapshot.peer_version
    ));

    // --- 本机更新：暂存 → 激活 → 收到 restart_requested ---------------------
    harness.send(Command::StageLocalUpdate(args.local_zip.clone()));
    harness.wait_for(
        "本机更新暂存",
        Duration::from_secs(60),
        |s| {
            !s.busy
                && s.update_message
                    .as_deref()
                    .is_some_and(|m| m.contains("本机更新已暂存"))
        },
    )?;

    harness.send(Command::ActivateLocalUpdate(args.local_build.clone()));
    let snapshot = harness.wait_for(
        "本机更新请求重启",
        Duration::from_secs(60),
        |s| {
            s.restart_requested
                && s.update_message
                    .as_deref()
                    .is_some_and(|m| m.contains("正在退出工作台"))
        },
    )?;

    let result = format!(
        concat!(
            "{{\"ok\":true,\"peer_build\":{},\"local_build\":{},\"peer_version\":{},",
            "\"peer_url\":{},\"peer_active_build\":{},\"restart_requested\":{},",
            "\"evidence\":[{}]}}\n"
        ),
        json_string(&args.peer_build),
        json_string(&args.local_build),
        json_string(&snapshot.peer_version),
        json_string(&snapshot.peer_url),
        json_string(&args.peer_build),
        snapshot.restart_requested,
        harness
            .evidence
            .iter()
            .map(|line| json_string(line))
            .collect::<Vec<_>>()
            .join(",")
    );
    std::fs::write(&args.result, result)
        .map_err(|e| format!("写入结果 {} 失败：{e}", args.result.display()))?;
    Ok(())
}

struct Harness {
    controller: HostController,
    evidence: Vec<String>,
}

impl Harness {
    fn send(&self, command: Command) {
        self.controller
            .tx
            .send(command)
            .expect("主机工作线程已退出");
    }

    fn snapshot(&self) -> Snapshot {
        self.controller
            .shared
            .snapshot
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default()
    }

    fn event_count(&self) -> usize {
        self.snapshot().events.len()
    }

    fn wait_for(
        &self,
        label: &str,
        timeout: Duration,
        predicate: impl Fn(&Snapshot) -> bool,
    ) -> Result<Snapshot, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.snapshot();
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "等待“{label}”超时；connected={} paired={} busy={} restart={} message={:?} error={:?}",
                    snapshot.connected,
                    snapshot.update_token_paired,
                    snapshot.busy,
                    snapshot.restart_requested,
                    snapshot.update_message,
                    snapshot.error
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_event(
        &self,
        before: usize,
        label: &str,
        timeout: Duration,
        predicate: impl Fn(&Snapshot) -> bool,
    ) -> Result<Snapshot, String> {
        self.wait_for(label, timeout, |s| s.events.len() > before && predicate(s))
    }

    fn wait_message(
        &self,
        label: &str,
        timeout: Duration,
        predicate: impl Fn(&str) -> bool,
    ) -> Result<Snapshot, String> {
        self.wait_for(label, timeout, |s| {
            s.update_message.as_deref().is_some_and(&predicate)
        })
    }
}

fn need<T>(value: Option<T>, name: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("缺少 {name}"))
}

fn parse_args() -> Result<Args, String> {
    let mut host_root = None;
    let mut bin_dir = None;
    let mut peer_url = None;
    let mut peer_token = None;
    let mut peer_zip = None;
    let mut peer_build = None;
    let mut local_zip = None;
    let mut local_build = None;
    let mut result = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next();
        match flag.as_str() {
            "--host-root" => host_root = value(),
            "--bin" => bin_dir = value(),
            "--peer-url" => peer_url = value(),
            "--peer-token" => peer_token = value(),
            "--peer-zip" => peer_zip = value().map(PathBuf::from),
            "--peer-build" => peer_build = value(),
            "--local-zip" => local_zip = value().map(PathBuf::from),
            "--local-build" => local_build = value(),
            "--result" => result = value().map(PathBuf::from),
            other => return Err(format!("未知参数 {other}")),
        }
    }
    Ok(Args {
        host_root: PathBuf::from(need(host_root, "--host-root")?),
        bin_dir: PathBuf::from(need(bin_dir, "--bin")?),
        peer_url: need(peer_url, "--peer-url")?,
        peer_token: need(peer_token, "--peer-token")?,
        peer_zip: need(peer_zip, "--peer-zip")?,
        peer_build: need(peer_build, "--peer-build")?,
        local_zip: need(local_zip, "--local-zip")?,
        local_build: need(local_build, "--local-build")?,
        result: need(result, "--result")?,
    })
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
