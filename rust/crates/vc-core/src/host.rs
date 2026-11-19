//! UI 无关的主机控制器。供 GPUI、Rust 集成及后续 C ABI 包装共用。
use crate::{
    Config,
    adapters::{self, AdapterInfo},
    budget::{self, Plan},
    client::PeerClient,
    process::{self, Kind, NativeSlot},
    session::{self, PreparedSession, SenderRequest},
    vcam::{self, VcamStatus},
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, Sender},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostScan {
    /// 尚未枚举网卡。
    #[default]
    NotScanned,
    /// 枚举成功；`host_adapters` 只含可确认的物理网卡。
    Ready,
    /// 枚举成功但没有可确认的物理网卡。
    NoPhysical,
    /// 枚举失败；旧列表已清空，不能用于启动。
    Failed,
}
#[derive(Clone, Default)]
pub struct Snapshot {
    pub peer_url: String,
    pub connected: bool,
    pub peer_online: bool,
    pub peer_version: String,
    pub error: Option<String>,
    /// 只含可确认的物理网卡；虚拟网卡、回环和元数据未知的条目不会出现在这里。
    pub host_adapters: Vec<AdapterInfo>,
    /// 最近一次本机网卡枚举的结果，供 UI 区分“未扫描/无物理网卡/扫描失败”。
    pub host_scan: HostScan,
    /// 扫描失败时的具体原因；成功时为空。
    pub host_scan_detail: Option<String>,
    pub peer_adapters: Vec<AdapterInfo>,
    pub host_adapter: String,
    pub peer_adapter: String,
    pub config: Config,
    pub plan: Option<Plan>,
    pub sender: Value,
    pub receiver: Value,
    pub vcam: VcamStatus,
    pub running: bool,
    pub cleanup_pending: bool,
    pub busy: bool,
    pub peer_update: Value,
    pub local_update: Value,
    pub update_message: Option<String>,
    pub update_token_paired: bool,
    pub restart_requested: bool,
    pub events: Vec<String>,
}
impl Snapshot {
    pub fn event_text(&self) -> Vec<String> {
        self.events.iter().rev().cloned().collect()
    }
}
pub struct Shared {
    pub snapshot: Mutex<Snapshot>,
}
pub enum Command {
    Connect(String),
    Refresh,
    SelectHostAdapter(String),
    SelectPeerAdapter(String),
    PatchCapture(Value),
    PatchTransport(Value),
    Start,
    Stop,
    SetPeerUpdateToken(String),
    RefreshUpdates,
    StagePeerUpdate(std::path::PathBuf),
    ActivatePeerUpdate(String),
    RollbackPeerUpdate(String),
    StageLocalUpdate(std::path::PathBuf),
    ActivateLocalUpdate(String),
    RollbackLocalUpdate(String),
    Shutdown,
}

pub struct HostController {
    pub tx: Sender<Command>,
    pub shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
impl HostController {
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for HostController {
    fn drop(&mut self) {
        self.shutdown();
    }
}
pub fn spawn() -> HostController {
    let (tx, rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        snapshot: Mutex::new(Snapshot::default()),
    });
    let worker_shared = shared.clone();
    let thread = std::thread::Builder::new()
        .name("vc-host-worker".into())
        .spawn(move || Worker::new(worker_shared, rx).run())
        .expect("无法启动主机工作线程");
    HostController {
        tx,
        shared,
        thread: Some(thread),
    }
}

struct Worker {
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    state: Snapshot,
    peer: Option<PeerClient>,
    receiver: NativeSlot,
    sender_pending: bool,
    session_id: String,
    vcam_owned: bool,
    config_error: bool,
    /// 已保存的接收网卡因不再合格而被清除后置位：后续扫描不再自动改选，
    /// 必须由用户显式重新选择，避免静默换到另一张网卡后直接启动。
    host_selection_needs_user: bool,
}
impl Worker {
    fn new(shared: Arc<Shared>, commands: Receiver<Command>) -> Self {
        let (config, error) = match Config::load_root() {
            Ok(c) => (c, None),
            Err(e) => (
                Config::default(),
                Some(format!("配置读取失败：{e}；修复文件后重启。")),
            ),
        };
        let config_error = error.is_some();
        let state = Snapshot {
            peer_url: config.peer_url.clone(),
            host_adapter: config.adapter.clone(),
            config,
            error,
            vcam: vcam::status(),
            local_update: crate::update::status().unwrap_or_else(|e| json!({"error": e})),
            update_token_paired: crate::update::load_peer_token().is_some(),
            ..Default::default()
        };
        Self {
            shared,
            commands,
            state,
            peer: None,
            receiver: NativeSlot::new(Kind::Receiver),
            sender_pending: false,
            session_id: String::new(),
            vcam_owned: false,
            config_error,
            host_selection_needs_user: false,
        }
    }
    fn run(mut self) {
        self.publish();
        let mut last_poll = Instant::now();
        loop {
            match self.commands.recv_timeout(Duration::from_millis(200)) {
                Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.stop();
                    break;
                }
                Ok(command) => self.handle(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if last_poll.elapsed() >= Duration::from_secs(1) {
                self.poll();
                last_poll = Instant::now();
            }
        }
    }
    fn publish(&mut self) {
        self.state.cleanup_pending = self.sender_pending || self.vcam_owned;
        if let Ok(mut state) = self.shared.snapshot.lock() {
            *state = self.state.clone();
        }
    }
    fn event(&mut self, message: impl Into<String>) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            % 86400;
        self.state.events.push(format!(
            "{:02}:{:02}:{:02}  {}",
            secs / 3600,
            (secs / 60) % 60,
            secs % 60,
            message.into()
        ));
        if self.state.events.len() > 100 {
            self.state.events.remove(0);
        }
    }
    fn fail(&mut self, message: String) {
        self.event(message.clone());
        self.state.error = Some(message);
        self.state.busy = false;
        self.publish();
    }
    fn blocked(&self) -> bool {
        self.state.running || self.sender_pending || self.vcam_owned
    }
    fn save(&mut self) {
        if let Err(e) = self.state.config.save_root() {
            self.fail(format!("配置保存失败：{e}"));
        }
    }
    fn handle(&mut self, command: Command) {
        if self.state.restart_requested && !matches!(command, Command::Stop | Command::Shutdown) {
            return;
        }
        if self.config_error && !matches!(command, Command::Stop | Command::Shutdown) {
            self.publish();
            return;
        }
        if self.blocked()
            && !matches!(
                command,
                Command::Stop | Command::Start | Command::Shutdown | Command::Refresh
            )
        {
            self.fail("会话运行中或仍待清理，先停止再修改连接或配置。".into());
            return;
        }
        match command {
            Command::Shutdown => {}
            Command::Connect(url) => self.connect(url),
            Command::Refresh => {
                if let Err(e) = self.refresh() {
                    self.fail(e);
                }
            }
            Command::SelectHostAdapter(name) => {
                // 只接受当前合格物理列表里的设备；UI 之外也不允许把选择改回虚拟网卡。
                if !name.is_empty()
                    && !self
                        .state
                        .host_adapters
                        .iter()
                        .any(|item| item.name == name)
                {
                    self.fail("该网卡不是可确认的物理设备，或列表已刷新；请重新选择。".into());
                    return;
                }
                self.state.host_adapter = name.clone();
                self.state.config.adapter = name;
                self.host_selection_needs_user = false;
                self.save();
            }
            Command::SelectPeerAdapter(name) => {
                if !name.is_empty()
                    && !self
                        .state
                        .peer_adapters
                        .iter()
                        .any(|item| item.name == name && item.is_physical_nic())
                {
                    self.fail("请选择当前列表中的物理发送网卡。".into());
                    return;
                }
                self.state.peer_adapter = name;
            }
            Command::PatchCapture(patch) => self.apply_patch("capture", patch),
            Command::PatchTransport(patch) => self.apply_patch("transport", patch),
            Command::Start => self.start(),
            Command::Stop => self.stop(),
            Command::SetPeerUpdateToken(token) => {
                match crate::update::save_peer_token(token.trim()) {
                    Ok(()) => {
                        self.state.update_token_paired = true;
                        if let Some(peer) = self.peer.take() {
                            self.peer =
                                Some(peer.with_update_token(crate::update::load_peer_token()));
                        }
                        self.state.update_message = Some("更新令牌已保存。".into());
                    }
                    Err(error) => self.state.update_message = Some(error),
                }
            }
            Command::RefreshUpdates => self.refresh_updates(),
            Command::StagePeerUpdate(path) => self.stage_peer_update(&path),
            Command::ActivatePeerUpdate(build) => self.activate_peer_update(&build, false),
            Command::RollbackPeerUpdate(build) => self.activate_peer_update(&build, true),
            Command::StageLocalUpdate(path) => self.stage_local_update(&path),
            Command::ActivateLocalUpdate(build) | Command::RollbackLocalUpdate(build) => {
                self.activate_local_update(&build)
            }
        }
        self.recompute();
        self.publish();
    }
    fn update_busy(&mut self, message: &str) {
        self.state.busy = true;
        self.state.update_message = Some(message.into());
        self.publish();
    }

    fn refresh_updates(&mut self) {
        self.update_busy("正在读取更新状态…");
        self.state.local_update = crate::update::status().unwrap_or_else(|e| json!({"error":e}));
        if let Some(peer) = &self.peer {
            self.state.peer_update = peer.update_status().unwrap_or_else(|e| json!({"error":e}));
        }
        self.state.update_message = None;
        self.state.busy = false;
    }

    fn stage_local_update(&mut self, path: &std::path::Path) {
        self.update_busy("正在校验本机更新包…");
        self.state.update_message = Some(
            match crate::update::stage_file(&crate::paths::root(), path) {
                Ok(_) => "本机更新已暂存，应用后将重启工作台。".into(),
                Err(error) => error,
            },
        );
        self.state.local_update = crate::update::status().unwrap_or_else(|e| json!({"error": e}));
        self.state.busy = false;
    }

    fn stage_peer_update(&mut self, path: &std::path::Path) {
        self.update_busy("正在校验并上传被采集机更新…");
        let result = self
            .peer
            .as_ref()
            .ok_or_else(|| "请先连接被采集机。".to_string())
            .and_then(|peer| peer.stage_update(path));
        self.state.update_message = Some(match result {
            Ok(_) => "被采集机更新已暂存，可应用更新。".into(),
            Err(error) => error,
        });
        if let Some(peer) = &self.peer {
            self.state.peer_update = peer.update_status().unwrap_or_else(|e| json!({"error":e}));
        }
        self.state.busy = false;
    }

    fn activate_peer_update(&mut self, build: &str, rollback: bool) {
        self.update_busy("正在切换被采集机版本…");
        let result = (|| {
            let peer = self.peer.clone().ok_or("请先连接被采集机。")?;
            let before = peer.health()?;
            session::require_peer(&before)?;
            if before["running"] == true {
                return Err("被采集机仍在采集，请先停止。".to_string());
            }
            if rollback {
                peer.rollback_update(build)?;
            } else {
                peer.activate_update(build)?;
            }
            self.state.peer_online = false;
            self.publish();
            let deadline = Instant::now() + Duration::from_secs(90);
            loop {
                if let Ok(health) = peer.health()
                    && session::require_peer(&health).is_ok()
                    && health["build"].as_str() == Some(build)
                    && health["pid"].as_u64().is_some_and(|pid| pid > 0)
                    && health["pid"] != before["pid"]
                    && peer_root_matches(&health["root"], &before["root"])
                    && health["running"] == false
                {
                    self.state.peer_version = health["version"].as_str().unwrap_or("?").into();
                    self.state.peer_online = true;
                    self.state.peer_update = peer.update_status()?;
                    self.state.peer_adapters = adapters::physical_nics(&peer.refresh_adapters()?);
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    self.state.peer_update = peer.update_status().unwrap_or(Value::Null);
                    return Err("更新就绪未确认，请查看更新历史与后台日志后重试连接。".into());
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        })();
        self.state.update_message = Some(match result {
            Ok(()) => "被采集机更新就绪。".into(),
            Err(error) => error,
        });
        self.state.busy = false;
    }

    fn activate_local_update(&mut self, build: &str) {
        self.update_busy("正在准备工作台更新…");
        let request = crate::update::ActivationRequest {
            role: crate::update::Role::Host,
            target: build.into(),
            parent_pid: std::process::id(),
            port: 0,
            bind: String::new(),
            wait_ms: 120_000,
            ready_ms: 30_000,
            require_ready: true,
        };
        match crate::update::prepare_activation(request) {
            Ok(_) => {
                self.state.restart_requested = true;
                self.state.update_message = Some("正在退出工作台并应用更新…".into());
            }
            Err(error) => self.state.update_message = Some(error),
        }
        self.state.busy = false;
    }

    fn apply_patch(&mut self, section: &str, patch: Value) {
        let patch = json!({section: patch});
        let errors = self.state.config.apply_patch(&patch);
        if errors.is_empty() {
            self.save();
        } else {
            self.fail(errors.join("；"));
        }
    }
    fn connect(&mut self, url: String) {
        if !crate::config::valid_peer_url(&url) {
            self.fail("请输入有效的被采集机 IPv4 地址。".into());
            return;
        }
        self.state.busy = true;
        self.state.error = None;
        self.publish();
        let peer = PeerClient::new(&url).with_update_token(crate::update::load_peer_token());
        let result = peer.health().and_then(|h| {
            session::require_peer(&h)?;
            Ok(h)
        });
        match result {
            Ok(h) => {
                self.state.peer_version = h["version"].as_str().unwrap_or("?").into();
                self.state.peer_online = true;
                self.state.connected = true;
                self.state.peer_url = url.clone();
                self.state.config.peer_url = url;
                self.state.peer_adapter.clear();
                self.peer = Some(peer);
                self.save();
                if let Err(e) = self.refresh() {
                    self.event(e);
                }
                self.event("已连接被采集机。");
            }
            Err(e) => {
                self.peer = None;
                self.state.connected = false;
                self.state.peer_online = false;
                self.fail(e);
            }
        }
        self.state.busy = false;
    }
    fn refresh(&mut self) -> Result<(), String> {
        self.scan_host_adapters()?;
        if let Some(peer) = &self.peer {
            self.state.peer_adapters = adapters::physical_nics(&peer.refresh_adapters()?);
            if !self.state.peer_adapter.is_empty()
                && !self
                    .state
                    .peer_adapters
                    .iter()
                    .any(|item| item.name == self.state.peer_adapter)
            {
                self.state.peer_adapter.clear();
                self.event("原发送网卡已失效，已恢复按物理控制路由选择。下次启动会重新准备确认。");
            }
        }
        Ok(())
    }
    /// 枚举本机网卡并只保留可确认的物理设备；枚举失败时清空旧列表，
    /// 避免用过期或含虚拟网卡的结果启动。
    fn scan_host_adapters(&mut self) -> Result<(), String> {
        let items = match adapters::enumerate(&crate::paths::native_exe("vc-sender.exe")) {
            Ok(items) => items,
            Err(error) => {
                self.state.host_adapters.clear();
                self.state.host_scan = HostScan::Failed;
                self.state.host_scan_detail = Some(error.clone());
                return Err(error);
            }
        };
        self.state.host_adapters = adapters::physical_host_nics(&items);
        self.state.host_scan = if self.state.host_adapters.is_empty() {
            HostScan::NoPhysical
        } else {
            HostScan::Ready
        };
        self.state.host_scan_detail = None;
        let previous = self.state.host_adapter.clone();
        match reconcile_host_selection(
            &previous,
            &self.state.host_adapters,
            self.host_selection_needs_user,
        ) {
            HostSelection::Keep => {}
            HostSelection::Auto => self.auto_select_host_adapter(),
            HostSelection::Clear => {
                self.state.host_adapter.clear();
                self.state.config.adapter.clear();
                self.host_selection_needs_user = true;
                self.save();
                self.event("原接收网卡不是可确认的物理设备或已不存在，已清除；请重新选择。");
            }
        }
        Ok(())
    }
    fn auto_select_host_adapter(&mut self) {
        let Some(ip) = self
            .state
            .peer_url
            .strip_prefix("http://")
            .and_then(|s| s.rsplit_once(':'))
            .map(|(h, _)| h)
        else {
            return;
        };
        if let Ok(item) = adapters::choose_local_adapter(&self.state.host_adapters, ip) {
            self.state.host_adapter = item.name.clone();
        }
    }
    fn recompute(&mut self) {
        let host = adapters::resolve(&self.state.host_adapters, &self.state.host_adapter)
            .ok()
            .flatten();
        let peer = adapters::resolve(&self.state.peer_adapters, &self.state.peer_adapter)
            .ok()
            .flatten();
        self.state.plan = Some(budget::plan(&self.state.config, host, peer));
    }
    fn request(&self, host: &AdapterInfo) -> SenderRequest {
        let mut capture = self.state.config.capture.clone();
        if let Some(mac) = &host.mac {
            capture.dst_mac = mac.replace('-', ":").to_ascii_lowercase();
        }
        SenderRequest {
            capture,
            session_id: process::new_session_id(Kind::Sender),
            source_mac: process::random_source_mac(),
            auto_adapter: self.state.peer_adapter.is_empty(),
            adapter: self.state.peer_adapter.clone(),
        }
    }
    fn start(&mut self) {
        if self.blocked() {
            self.fail("会话正在运行或有资源待清理，先停止。".into());
            return;
        }
        self.state.busy = true;
        self.state.error = None;
        self.publish();
        if let Err(e) = self.start_inner() {
            self.event(format!("启动失败：{e}"));
            self.stop();
            self.state.error = Some(e);
        }
        self.state.busy = false;
    }
    fn start_inner(&mut self) -> Result<(), String> {
        process::validate_raw_config(&self.state.config)?;
        let peer = self.peer.clone().ok_or("尚未连接被采集机。")?;
        let health = peer.health()?;
        session::require_peer(&health)?;
        if health["running"] == true {
            return Err("被采集机已有采集会话，拒绝覆盖。".into());
        }
        self.refresh()?;
        let host = adapters::resolve(&self.state.host_adapters, &self.state.host_adapter)?
            .ok_or(if self.state.host_adapters.is_empty() {
                "未找到可确认的物理接收网卡；请检查 Npcap 与网卡连接后刷新。"
            } else {
                "请选择本机接收网卡。"
            })?
            .clone();
        process::require_raw_capabilities(Kind::Receiver)?;
        let mut request = self.request(&host);
        let body = serde_json::to_value(&request).map_err(|e| e.to_string())?;
        let prepared: PreparedSession = serde_json::from_value(peer.prepare_sender(&body)?)
            .map_err(|e| format!("采集准备响应无效：{e}"))?;
        request.confirm(&prepared)?;
        if !prepared.adapter.is_physical_nic() {
            return Err("被采集机未确认物理发送网卡，拒绝启动。".into());
        }
        // 固定本次准备阶段确认的网卡，启动时不再自动换卡。
        request.auto_adapter = false;
        request.adapter = prepared.adapter.name.clone();
        let plan = budget::plan(&self.state.config, Some(&host), Some(&prepared.adapter));
        if !plan.can_start {
            return Err(format!(
                "链路预算拒绝启动：{:?} {}",
                plan.network.status,
                plan.errors.join("；")
            ));
        }
        self.state.plan = Some(plan);
        if self.state.config.receive.vcam_autoregister {
            let before = vcam::status();
            if !before.registered.is_empty() {
                return Err("已有虚拟卡注册；先停止现有主机，避免接管其会话。".into());
            }
            vcam::require_capabilities()?;
            self.vcam_owned = true; // 即使注册部分失败，也尝试回收。
            self.state.vcam = vcam::register(&self.state.config.receive.shm)?;
        }
        let run_dir = crate::paths::run_dir();
        let (stats, hashlog, _) =
            process::session_paths(&run_dir, &request.session_id, Kind::Receiver);
        let args = process::receiver_args(
            &self.state.config,
            &host.name,
            &request.session_id,
            &stats,
            &hashlog,
            Some(&request.source_mac),
        );
        self.receiver
            .start(&args, &host.name, &request.session_id, &run_dir)?;
        self.session_id = request.session_id.clone();
        self.sender_pending = true; // HTTP 超时也可能已经开始，必须按会话停止。
        let response =
            peer.sender_start(&serde_json::to_value(&request).map_err(|e| e.to_string())?)?;
        let confirmed: PreparedSession =
            serde_json::from_value(response).map_err(|e| e.to_string())?;
        request.confirm(&confirmed)?;
        self.state.running = true;
        self.event("采集已开始，等待首帧。");
        Ok(())
    }
    fn stop(&mut self) {
        self.state.busy = true;
        self.publish();
        if self.sender_pending
            && let Some(peer) = &self.peer
        {
            match peer.sender_stop_for(&self.session_id) {
                Ok(_) => {
                    self.sender_pending = false;
                    self.event("被采集机已停止。");
                }
                Err(e) => self.event(format!("被采集机停止未确认；继续清理本机，可重试：{e}")),
            }
        }
        self.receiver.stop();
        if self.vcam_owned {
            let status = vcam::status();
            if status.registered.is_empty() {
                self.vcam_owned = false;
            } else if status.match_local {
                match vcam::unregister() {
                    Ok(_) => self.vcam_owned = false,
                    Err(e) => self.event(format!("虚拟卡回收失败，可重试停止：{e}")),
                }
            } else {
                self.vcam_owned = false;
                self.event("虚拟卡已被其他目录接管，不注销其他注册。");
            }
        }
        self.state.running = false;
        self.state.sender = Value::Null;
        self.state.receiver = Value::Null;
        self.state.vcam = vcam::status();
        self.state.busy = false;
        self.publish();
    }
    fn poll(&mut self) {
        let Some(peer) = self.peer.clone() else {
            return;
        };
        match peer.health().and_then(|h| {
            session::require_peer(&h)?;
            Ok(h)
        }) {
            Ok(_) => {
                self.state.peer_online = true;
                match peer.sender_stats() {
                    Ok(stats) => {
                        let stopped = self.state.running && stats["running"] != true;
                        self.state.sender = stats;
                        if stopped {
                            self.event("采集进程已退出，回收会话。");
                            self.stop();
                        }
                    }
                    Err(e) => {
                        self.state.sender = Value::Null;
                        self.event(e);
                    }
                }
            }
            Err(e) => {
                self.state.peer_online = false;
                self.state.sender = Value::Null;
                if self.state.running {
                    self.event(format!("被采集机失联，回收本机会话：{e}"));
                    self.stop();
                }
            }
        }
        if self.state.running {
            self.state.receiver = self.receiver.stats();
            if !self.receiver.running() {
                self.event("本机接收进程已退出，停止并回收会话。");
                self.stop();
            }
        }
        self.publish();
    }
}

/// Windows canonicalize adds the extended path prefix after helper restart.
/// Compare remote root strings without trying to resolve them on this machine.
fn peer_root_matches(actual: &Value, previous: &Value) -> bool {
    fn normalized(value: &Value) -> Option<String> {
        let path = value.as_str()?.replace('/', "\\");
        let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
        Some(path.trim_end_matches('\\').to_ascii_lowercase())
    }
    match (normalized(actual), normalized(previous)) {
        (Some(a), Some(b)) => !a.is_empty() && a == b,
        _ => false,
    }
}

/// 扫描后如何处置已保存的本机接收网卡选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostSelection {
    /// 保持不变：原选择仍合格，或已被清除、等待用户重新选择。
    Keep,
    /// 原本没有选择且无需用户介入，可按控制连接自动选卡。
    Auto,
    /// 原选择不是合格物理网卡或已不存在：清除，且不自动改选。
    Clear,
}

fn reconcile_host_selection(
    previous: &str,
    physical: &[AdapterInfo],
    needs_user: bool,
) -> HostSelection {
    if previous.is_empty() {
        if needs_user {
            HostSelection::Keep
        } else {
            HostSelection::Auto
        }
    } else if physical.iter().any(|item| item.name == previous) {
        HostSelection::Keep
    } else {
        HostSelection::Clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(name: &str) -> AdapterInfo {
        AdapterInfo {
            name: name.into(),
            description: "test nic".into(),
            hardware_interface: Some(true),
            virtual_nic: Some(false),
            ..Default::default()
        }
    }

    #[test]
    fn reconcile_keeps_only_listed_physical_selection() {
        let physical = vec![nic(r"\Device\NPF_{A}")];
        assert_eq!(
            reconcile_host_selection(r"\Device\NPF_{A}", &physical, false),
            HostSelection::Keep
        );
        // 已保存的虚拟网卡、回环或已拔出的设备都不在合格列表里。
        assert_eq!(
            reconcile_host_selection(r"\Device\NPF_{V}", &physical, false),
            HostSelection::Clear
        );
        assert_eq!(
            reconcile_host_selection(r"\Device\NPF_Loopback", &physical, false),
            HostSelection::Clear
        );
        assert_eq!(
            reconcile_host_selection(r"\Device\NPF_{OLD}", &physical, false),
            HostSelection::Clear
        );
    }

    #[test]
    fn reconcile_auto_selects_only_without_a_cleared_selection() {
        let physical = vec![nic(r"\Device\NPF_{A}")];
        assert_eq!(
            reconcile_host_selection("", &physical, false),
            HostSelection::Auto
        );
        // 刚刚因不合格被清除的选择，后续扫描不得静默改选另一张卡。
        assert_eq!(
            reconcile_host_selection("", &physical, true),
            HostSelection::Keep
        );
        // 没有任何物理网卡时同样无从自动选择。
        assert_eq!(
            reconcile_host_selection("", &[], false),
            HostSelection::Auto
        );
    }

    #[test]
    fn peer_root_survives_windows_canonicalization() {
        assert!(peer_root_matches(
            &json!(r"\\?\D:\Capture\"),
            &json!("d:/Capture")
        ));
        assert!(!peer_root_matches(
            &json!(r"D:\other"),
            &json!(r"D:\Capture")
        ));
        assert!(!peer_root_matches(&Value::Null, &Value::Null));
    }

    #[test]
    fn controller_shutdown_joins_without_starting_any_process() {
        let mut controller = spawn();
        controller.shutdown();
        assert!(!controller.shared.snapshot.lock().unwrap().running);
    }
}
