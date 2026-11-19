//! UI 无关的主机控制器。供 GPUI、Rust 集成及后续 C ABI 包装共用。
use crate::{
    Config,
    adapters::{self, AdapterInfo},
    budget::{self, Plan},
    client::PeerClient,
    process::{self, BatchEcho, Kind, NativeSlot, StopOutcome},
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
    /// 枚举成功；`host_adapters` 只含已连接的物理网卡。
    Ready,
    /// 枚举成功但没有已连接的物理网卡。
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
    /// 只含已连接的物理网卡；虚拟、回环、未连接和元数据未知的条目不会出现在这里。
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
    /// 本次预算/准备/启动使用的生效 MTU：自动模式为两端网卡报告值的较小者，
    /// 显式模式为配置值；自动解析失败时为 None。
    pub effective_mtu: Option<u32>,
    pub sender: Value,
    pub receiver: Value,
    pub vcam: VcamStatus,
    pub running: bool,
    pub cleanup_pending: bool,
    /// 远端仍有本机持有或显式 Stop 尚未确认的会话（含只知有会话但读不到
    /// session_id 的情况）。UI 用它决定是否提供“停止/重试停止”。
    pub remote_pending: bool,
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
    /// 只清理本机接收/虚拟卡；绝不探测或停止远端会话。
    CleanupLocal,
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
    /// 只有显式 Stop 可以按 stats 发现远端会话后记下这个精确 session_id；
    /// 后续显式 Stop 在 stats 不可用时可重试它。轮询、关闭、启动失败都不得使用。
    discovered_sender: Option<String>,
    /// 显式 Stop 尚未确认远端已停止；即使还不知道精确 session_id 也要保留，
    /// 以便 UI 可重试并阻止连接/OTA。自动路径只读核对，不得据此停止远端。
    remote_stop_pending: bool,
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
            discovered_sender: None,
            remote_stop_pending: false,
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
        self.state.remote_pending =
            self.sender_pending || self.remote_stop_pending || self.discovered_sender.is_some();
        self.state.cleanup_pending = self.cleanup_pending();
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
    /// 远端发送、本机接收或虚拟卡仍有未确认清理的资源。
    fn cleanup_pending(&self) -> bool {
        self.sender_pending
            || self.remote_stop_pending
            || self.discovered_sender.is_some()
            || self.receiver.cleanup_pending()
            || self.vcam_owned
    }
    fn blocked(&self) -> bool {
        self.state.running || self.cleanup_pending()
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
        if self.config_error
            && !matches!(
                command,
                Command::Stop | Command::CleanupLocal | Command::Shutdown
            )
        {
            self.publish();
            return;
        }
        if self.blocked()
            && !matches!(
                command,
                Command::Stop
                    | Command::CleanupLocal
                    | Command::Start
                    | Command::Shutdown
                    | Command::Refresh
            )
        {
            self.fail("会话运行中或仍待清理，先停止再修改连接或配置。".into());
            return;
        }
        match command {
            Command::Shutdown => {}
            Command::CleanupLocal => self.stop_inner(false),
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
                    self.fail("该网卡未连接、不是物理设备或列表已刷新；请重新选择。".into());
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
                        .any(|item| item.name == name && item.is_connected_physical_nic())
                {
                    self.fail("请选择当前列表中的已连接物理发送网卡。".into());
                    return;
                }
                self.state.peer_adapter = name;
            }
            Command::PatchCapture(patch) => self.apply_patch("capture", patch),
            Command::PatchTransport(patch) => self.apply_patch("transport", patch),
            Command::Start => self.start(),
            Command::Stop => self.stop_explicit(),
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
        // 远端停止未确认时不允许换连接把重试目标冲掉；先重试停止或退出主机。
        if self.sender_pending || self.remote_stop_pending || self.discovered_sender.is_some() {
            self.fail("远端停止尚未确认，先重试停止再修改连接。".into());
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
                self.event(
                    "原发送网卡已失效，已清除选择；仅剩一张已连接物理网卡时自动预览，多张请手动选择。",
                );
            }
        }
        Ok(())
    }
    /// 枚举本机网卡并只保留已连接的物理设备；枚举失败时清空旧列表，
    /// 避免用过期或含虚拟网卡的结果启动。
    fn scan_host_adapters(&mut self) -> Result<(), String> {
        let result = self
            .receiver
            .data_plane_module()
            .and_then(|module| adapters::enumerate(&module));
        let items = match result {
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
                self.event("原接收网卡未连接、不是物理设备或已不存在，已清除；请重新选择。");
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
        match adapters::choose_auto_adapter(&self.state.host_adapters, ip) {
            Ok(item) => self.state.host_adapter = item.name.clone(),
            Err(error) => {
                // 控制连接路由不唯一时不静默挑一张；要求用户显式选择，
                // 后续扫描也不再自动改选。
                self.host_selection_needs_user = true;
                self.event(format!("无法自动选择主机接收网卡：{error}"));
            }
        }
    }
    /// 预算与自动 MTU 使用的对端发送网卡预览。
    ///
    /// 显式选择优先并保持原样；未选择时只有唯一已连接物理网卡可自动预览，
    /// 多张时要求显式选择。没有候选时返回 None：显式 MTU 仍可先准备再由
    /// 采集端按实际网卡复核，但自动 MTU 无法在准备前解析。
    fn peer_preview_adapter(&self) -> Result<Option<AdapterInfo>, String> {
        if !self.state.peer_adapter.is_empty() {
            let adapter = adapters::resolve(&self.state.peer_adapters, &self.state.peer_adapter)?
                .ok_or_else(|| "请选择当前列表中的已连接物理发送网卡。".to_string())?;
            if !adapter.is_connected_physical_nic() {
                return Err("所选发送网卡未连接或不是物理设备，请重新选择。".into());
            }
            return Ok(Some(adapter.clone()));
        }
        Ok(adapters::unique_connected_physical(&self.state.peer_adapters)?.cloned())
    }
    fn recompute(&mut self) {
        let host = adapters::resolve(&self.state.host_adapters, &self.state.host_adapter)
            .ok()
            .flatten();
        let peer = match self.peer_preview_adapter() {
            Ok(item) => item,
            Err(error) => {
                // 对端有多张候选：要求明确选择，不能猜一张用于预算。
                let mut plan = budget::plan(&self.state.config, host, None);
                plan.errors.push(error);
                plan.can_start = false;
                self.state.effective_mtu = None;
                self.state.plan = Some(plan);
                return;
            }
        };
        let effective = budget::host_effective_config(&self.state.config, host, peer.as_ref());
        self.state.effective_mtu = effective.as_ref().ok().map(|config| config.capture.mtu);
        let plan_config = match &effective {
            Ok(config) => config.clone(),
            // 解析失败仍用原配置算展示数值；错误随后并入 can_start=false。
            Err(_) => self.state.config.clone(),
        };
        let mut plan = budget::plan(&plan_config, host, peer.as_ref());
        if let Err(error) = effective {
            plan.errors.push(error);
            plan.can_start = false;
        }
        self.state.plan = Some(plan);
    }
    fn request(&self, host: &AdapterInfo, mut capture: crate::config::Capture) -> SenderRequest {
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
        // 先校验保存配置的结构；生效 MTU 取决于网卡解析，稍后用有效配置再校验。
        let errors = self.state.config.validate();
        if !errors.is_empty() {
            return Err(errors.join("；"));
        }
        let peer = self.peer.clone().ok_or("尚未连接被采集机。")?;
        let health = peer.health()?;
        session::require_peer(&health)?;
        if health["running"] == true {
            return Err("被采集机已有采集会话，拒绝覆盖。".into());
        }
        self.refresh()?;
        let host = adapters::resolve(&self.state.host_adapters, &self.state.host_adapter)?
            .ok_or(if self.state.host_adapters.is_empty() {
                "未找到已连接的物理接收网卡；请检查 Npcap 与网卡连接后刷新。"
            } else {
                "请选择本机接收网卡。"
            })?
            .clone();
        // 自动 MTU 必须在 prepare 前知道两端网卡：显式选择优先；对端只有一张
        // 已连接物理网卡时才自动预览，多张必须显式选择。
        let peer_preview = self.peer_preview_adapter()?;
        let effective =
            budget::host_effective_config(&self.state.config, Some(&host), peer_preview.as_ref())?;
        process::validate_start_config(&effective)?;
        // 能力门禁在向对端准备会话之前完成；模块与本次编码/来源组合必须已声明。
        let module = self.receiver.data_plane_module()?;
        let caps = process::require_capabilities(&module)?;
        process::require_stream_selection(&caps, &effective.capture.codec, None)?;
        let mut request = self.request(&host, effective.capture.clone());
        let body = serde_json::to_value(&request).map_err(|e| e.to_string())?;
        let prepared: PreparedSession = serde_json::from_value(peer.prepare_sender(&body)?)
            .map_err(|e| format!("采集准备响应无效：{e}"))?;
        request.confirm(&prepared)?;
        if !prepared.adapter.is_connected_physical_nic() {
            return Err("被采集机未确认已连接的物理发送网卡，拒绝启动。".into());
        }
        // 响应回显的 MTU 已由 confirm 精确比对；再按实际发送网卡报告复核，
        // 采集端自行自动选卡时也不允许超过其 NlMtu。绝不修改网卡设置。
        budget::verify_mtu_for_adapter(
            effective.capture.mtu,
            Some(&prepared.adapter),
            "被采集机发送网卡",
        )?;
        // 固定本次准备阶段确认的网卡，启动时不再自动换卡。
        request.auto_adapter = false;
        request.adapter = prepared.adapter.name.clone();
        let plan = budget::plan(&effective, Some(&host), Some(&prepared.adapter));
        if !plan.can_start {
            return Err(format!(
                "链路预算拒绝启动：{:?} {}",
                plan.network.status,
                plan.errors.join("；")
            ));
        }
        self.state.effective_mtu = Some(effective.capture.mtu);
        self.state.plan = Some(plan);
        if self.state.config.receive.vcam_autoregister {
            let before = vcam::status();
            // 崩溃重启后注册表可能仍指向本目录 DLL；OTA 后可能指向上一个
            // release。只有可证明属于本运行根且原属主已退出的注册才允许复用/
            // 回收，其他目录或存活主机一律拒绝，绝不静默接管。
            vcam::check_available(&before)?;
            vcam::require_capabilities()?;
            self.vcam_owned = true; // 即使注册部分失败，也尝试回收。
            self.state.vcam = vcam::register(&self.state.config.receive.shm)?;
        }
        let run_dir = crate::paths::run_dir();
        let (stats, hashlog, _) =
            process::session_paths(&run_dir, &request.session_id, Kind::Receiver);
        let request_json = process::receiver_request(
            &effective,
            &host.name,
            &request.session_id,
            &stats,
            &hashlog,
            &request.source_mac,
        )?;
        self.receiver.start(
            &request_json,
            &host.name,
            &request.session_id,
            &run_dir,
            BatchEcho::Ignore,
        )?;
        self.session_id = request.session_id.clone();
        self.sender_pending = true; // HTTP 超时也可能已经开始，必须按会话停止。
        self.discovered_sender = None;
        let response =
            peer.sender_start(&serde_json::to_value(&request).map_err(|e| e.to_string())?)?;
        let confirmed: PreparedSession =
            serde_json::from_value(response).map_err(|e| e.to_string())?;
        request.confirm(&confirmed)?;
        self.state.running = true;
        self.event("采集已开始，等待首帧。");
        Ok(())
    }
    /// 自动回收/窗口关闭路径：只清理本控制器拥有的资源。
    ///
    /// 即使对端仍有采集会话也绝不在这里探测或停止：它可能属于另一个主机或
    /// 用户，只有显式 Stop 才允许发现并停止当前会话。
    fn stop(&mut self) {
        self.stop_inner(false);
    }
    /// 用户显式点击停止：允许查询远端 stats，并在确认后停止其精确 session_id。
    fn stop_explicit(&mut self) {
        self.stop_inner(true);
    }
    fn stop_inner(&mut self, discover_remote: bool) {
        self.state.busy = true;
        self.publish();
        // 顺序保持：先停远端发送，再停本机接收，最后才允许注销虚拟卡。
        let mut remote_joined = false;
        if self.sender_pending {
            if let Some(peer) = self.peer.clone() {
                match peer.sender_stop_for(&self.session_id) {
                    Ok(_) => {
                        self.sender_pending = false;
                        self.remote_stop_pending = false;
                        remote_joined = true;
                        self.event("被采集机已停止。");
                    }
                    Err(e) => {
                        // 409/超时都可能是“远端已被别的会话占用”。只读 stats 对账：
                        // 会话不是本机的就清除本机 claim，绝不继续追着别人的会话停止。
                        match self.probe_sender_claim(&peer) {
                            SenderClaim::Ours => self.event(format!(
                                "被采集机停止未确认；会话仍由本机持有，可重试停止：{e}"
                            )),
                            SenderClaim::Idle => {
                                self.sender_pending = false;
                                self.remote_stop_pending = false;
                                self.discovered_sender = None;
                                remote_joined = true;
                                self.event(format!("远端采集已结束，清除本机待停止标记：{e}"));
                            }
                            SenderClaim::Replaced(id) => {
                                self.sender_pending = false;
                                self.remote_stop_pending = false;
                                self.discovered_sender = None;
                                remote_joined = true;
                                self.event(format!(
                                    "远端会话已更换为 {id}（非本机创建），不再停止该会话；回收本机资源：{e}"
                                ));
                            }
                            SenderClaim::Unknown => self.event(format!(
                                "被采集机停止未确认；保留会话与虚拟卡，可重试停止：{e}"
                            )),
                        }
                    }
                }
            } else {
                self.event("被采集机连接已断开，无法确认远端停止；保留待清理状态。");
            }
        } else if discover_remote {
            match self.stop_discovered_sender() {
                Ok(RemoteStop::Idle) => {
                    remote_joined = true;
                }
                Ok(RemoteStop::Stopped(session)) => {
                    remote_joined = true;
                    self.state.sender = Value::Null;
                    self.event(format!("已停止被采集机的采集会话 {session}。"));
                }
                Err(error) => {
                    remote_joined = false;
                    self.event(format!(
                        "被采集机停止未确认；保留待清理会话，可再次停止重试：{error}"
                    ));
                }
            }
        } else {
            // 自动/本地清理绝不联系远端；若仍有未确认的远端停止，保留虚拟卡。
            remote_joined = self.discovered_sender.is_none() && !self.remote_stop_pending;
        }
        let receiver_outcome = self.receiver.stop();
        let receiver_joined = matches!(receiver_outcome, Ok(StopOutcome::Stopped));
        match receiver_outcome {
            Ok(StopOutcome::Stopped) => {}
            Ok(StopOutcome::Pending) => {
                self.event("本机接收停止未确认；数据面会话句柄已保留，可重试停止。");
            }
            Err(e) => self.event(format!("本机接收停止失败，句柄保留待重试：{e}")),
        }
        if self.vcam_owned {
            if !(remote_joined && receiver_joined) {
                self.event("采集或接收会话尚未确认停止，暂不注销虚拟卡；重试停止后继续清理。");
            } else {
                let status = vcam::status();
                if status.registration_unknown {
                    self.event("虚拟卡注册状态不可读，保留待清理；请稍后重试停止。");
                } else if status.registered.is_empty() {
                    self.vcam_owned = false;
                } else {
                    match vcam::recovery(&status) {
                        vcam::Recovery::Local | vcam::Recovery::StaleOwned => {
                            match vcam::unregister() {
                                Ok(_) => self.vcam_owned = false,
                                Err(e) => self.event(format!("虚拟卡回收失败，可重试停止：{e}")),
                            }
                        }
                        recovery @ (vcam::Recovery::Busy | vcam::Recovery::Foreign) => {
                            self.vcam_owned = false;
                            self.event(format!(
                                "{}；不注销其他注册。",
                                vcam::recovery_error(recovery)
                            ));
                        }
                        vcam::Recovery::Unknown => {
                            // 无法证明归属时保留 vcam_owned，避免悄悄放弃或误删别人的注册。
                            self.event(
                                "无法证明虚拟卡注册属于本运行根，保留待清理；不会注销该注册。",
                            );
                        }
                        vcam::Recovery::Fresh => self.vcam_owned = false,
                    }
                }
            }
        }
        self.state.running = false;
        if remote_joined {
            self.state.sender = Value::Null;
        }
        if receiver_joined {
            self.state.receiver = Value::Null;
        } else {
            self.state.receiver = self.receiver.stats();
        }
        self.state.vcam = vcam::status();
        self.state.busy = false;
        self.publish();
    }
    /// 显式 Stop 专用：先用 stats 发现当前 sender，再只停止其精确 session_id。
    ///
    /// 每次都以最新 stats 为准刷新目标，避免远端换了会话后仍追着旧号重试；
    /// stats/health 暂时不可读时，才重试此前显式 Stop 已确认过的精确会话号。
    fn stop_discovered_sender(&mut self) -> Result<RemoteStop, String> {
        let Some(peer) = self.peer.clone() else {
            self.discovered_sender = None;
            self.remote_stop_pending = false;
            return Ok(RemoteStop::Idle);
        };
        // 只要显式 Stop 尝试后没有确认“远端已无会话/已停止”，就保留 pending；
        // 即使还读不到 session_id，也不能让 UI 退回“开始采集”。
        self.remote_stop_pending = true;
        let (target, discovery_error) = match self.discover_remote_session(&peer) {
            Ok(Some(session)) => {
                self.discovered_sender = Some(session.clone());
                (Some(session), None)
            }
            Ok(None) => {
                self.discovered_sender = None;
                self.remote_stop_pending = false;
                return Ok(RemoteStop::Idle);
            }
            Err(error) => (self.discovered_sender.clone(), Some(error)),
        };
        let Some(session) = target else {
            return Err(
                discovery_error.unwrap_or_else(|| "被采集机状态不可读，未停止任何会话。".into())
            );
        };
        match peer.sender_stop_for(&session) {
            Ok(_) => {
                self.discovered_sender = None;
                self.remote_stop_pending = false;
                Ok(RemoteStop::Stopped(session))
            }
            Err(stop_error) => {
                self.discovered_sender = Some(session.clone());
                Err(match discovery_error {
                    Some(error) => format!("{error}；重试上次会话 {session} 也失败：{stop_error}"),
                    None => stop_error,
                })
            }
        }
    }
    /// 只读 stats，判断本机持有的 sender claim 是否仍是远端当前会话。
    fn probe_sender_claim(&mut self, peer: &PeerClient) -> SenderClaim {
        match peer.sender_stats() {
            Ok(stats) => {
                self.state.sender = stats.clone();
                // running 缺失/类型错误时无法证明会话已结束，保留本机 claim。
                let Some(running) = stats.get("running").and_then(Value::as_bool) else {
                    return SenderClaim::Unknown;
                };
                let id = stats["session_id"].as_str().unwrap_or("").trim();
                if !running {
                    SenderClaim::Idle
                } else if id.is_empty() {
                    SenderClaim::Unknown
                } else if id == self.session_id {
                    SenderClaim::Ours
                } else {
                    SenderClaim::Replaced(id.to_string())
                }
            }
            Err(_) => SenderClaim::Unknown,
        }
    }
    /// 只读取并校验 Rust 采集后台后返回当前运行中的会话号；不产生停止副作用。
    fn discover_remote_session(&mut self, peer: &PeerClient) -> Result<Option<String>, String> {
        let health = peer.health()?;
        session::require_peer(&health)?;
        match health.get("running").and_then(Value::as_bool) {
            Some(true) => {}
            Some(false) => return Ok(None),
            None => {
                return Err("被采集机 health 缺少可确认的 running 布尔值；未停止任何会话。".into());
            }
        }
        let stats = peer.sender_stats()?;
        self.state.sender = stats.clone();
        session_to_stop(&stats)
    }
    /// 只读 stats 对账本机 claim/discovered 目标；绝不在这里停止远端。
    ///
    /// 返回 true 表示至少有一个远端 claim 已被确认结束或被其他会话替代，
    /// 调用方应回收本机接收/虚拟卡资源。stats 缺少 `running` 布尔值时不做
    /// 任何结论，保持 fail-closed。
    fn reconcile_claims(&mut self, stats: &Value) -> bool {
        let Some(running) = stats.get("running").and_then(Value::as_bool) else {
            return false;
        };
        let id = stats["session_id"].as_str().unwrap_or("").trim();
        let mut resolved = false;
        if self.sender_pending {
            if !running {
                self.sender_pending = false;
                resolved = true;
                self.event("远端采集已结束，清除本机待停止标记。");
            } else if !id.is_empty() && id != self.session_id {
                self.sender_pending = false;
                resolved = true;
                self.event(format!(
                    "远端采集会话已更换为 {id}（非本机创建），本机停止重试已取消。"
                ));
            }
        }
        if let Some(target) = self.discovered_sender.clone() {
            if !running {
                self.discovered_sender = None;
                resolved = true;
                self.event("远端目标会话已结束，清除重试标记。");
            } else if !id.is_empty() && id != target {
                // 远端同一时刻只有一个采集会话；精确目标已被别的会话替换，
                // 说明要停的会话已经不在了。只清本机重试目标，绝不停止新会话。
                self.discovered_sender = None;
                self.remote_stop_pending = false;
                resolved = true;
                self.event("远端目标会话已更换，不再按旧 session_id 重试停止。");
            }
        }
        if self.remote_stop_pending && !running {
            self.remote_stop_pending = false;
            resolved = true;
            self.event("远端采集已结束，清除待重试停止标记。");
        }
        resolved
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
                        let was_running = self.state.running;
                        self.state.sender = stats.clone();
                        // running 缺失/类型错误时不做回收结论，保持待清理状态。
                        if let Some(remote_running) = stats.get("running").and_then(Value::as_bool)
                        {
                            let resolved = self.reconcile_claims(&stats);
                            if (was_running && !remote_running) || resolved {
                                if was_running && !remote_running {
                                    self.event("采集会话已结束，回收会话。");
                                }
                                self.stop();
                            }
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
        let receiver_running = self.receiver.running();
        if self.state.running || receiver_running || self.receiver.cleanup_pending() {
            self.state.receiver = self.receiver.stats();
            if self.state.running && !receiver_running {
                self.event("本机接收会话已结束，停止并回收会话。");
                self.stop();
            }
        }
        self.publish();
    }
}

/// 本机 sender claim 与远端当前 stats 的关系。
enum SenderClaim {
    Ours,
    Idle,
    Replaced(String),
    Unknown,
}

/// 显式 Stop 的远端发现结果。
enum RemoteStop {
    /// 对端没有运行中的采集会话。
    Idle,
    /// 已按精确会话号停止。
    Stopped(String),
}

/// stats 中可停止的会话号。运行中却缺少会话号时必须拒绝，不能盲停；
/// `running` 缺失/类型错误同样视为未知，不能当作“已结束”。
fn session_to_stop(stats: &Value) -> Result<Option<String>, String> {
    match stats.get("running").and_then(Value::as_bool) {
        Some(true) => {}
        Some(false) => return Ok(None),
        None => {
            return Err("被采集机 stats 缺少可确认的 running 布尔值；未停止任何会话。".into());
        }
    }
    let session = stats
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if session.is_empty() {
        return Err("被采集机报告采集运行中，但未提供可确认的 session_id；未停止任何会话。".into());
    }
    Ok(Some(session.to_string()))
}

/// Windows canonicalize 在 helper 重启后可能加上扩展路径前缀。
/// 这里只比较远端根字符串，不在本机解析该路径。
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
    /// 原选择未连接、不是物理网卡或已不存在：清除，且不自动改选。
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
            status: Some("Up".into()),
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
        // 没有任何已连接物理网卡时同样无从自动选择。
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

    fn nic_with_mtu(name: &str, mtu: Option<u32>) -> AdapterInfo {
        AdapterInfo {
            label: Some(name.into()),
            mtu,
            ..nic(name)
        }
    }

    #[test]
    fn peer_preview_accepts_any_single_connected_physical_nic_and_refuses_multiple() {
        let (mut worker, _tx) = test_worker("http://127.0.0.1:1");
        let mut intel = nic_with_mtu(r"\Device\NPF_{A}", Some(9000));
        intel.description = "Intel I226-V".into();
        let mut realtek = nic_with_mtu(r"\Device\NPF_{R}", Some(1500));
        realtek.description = "Realtek 2.5GbE".into();
        worker.state.peer_adapters = vec![realtek.clone()];
        assert_eq!(
            worker.peer_preview_adapter().unwrap().unwrap().name,
            realtek.name
        );

        worker.state.peer_adapters = vec![intel, realtek.clone()];
        let error = worker.peer_preview_adapter().unwrap_err();
        assert!(error.contains("2 张") && error.contains("静默"), "{error}");

        // 多卡时保留显式选择，不按厂商或列表顺序改写。
        worker.state.peer_adapter = realtek.name.clone();
        assert_eq!(worker.peer_preview_adapter().unwrap().unwrap(), realtek);
        worker.state.peer_adapters[1].status = Some("Disconnected".into());
        assert!(worker.peer_preview_adapter().is_err());
        worker.state.peer_adapter.clear();
        worker.state.peer_adapters.clear();
        assert!(worker.peer_preview_adapter().unwrap().is_none());
    }

    #[test]
    fn recompute_uses_effective_auto_mtu_and_blocks_without_both_nics() {
        let (mut worker, _tx) = test_worker("http://127.0.0.1:1");
        worker.state.host_adapters = vec![nic_with_mtu(r"\Device\NPF_{H}", Some(9000))];
        worker.state.host_adapter = r"\Device\NPF_{H}".into();
        worker.state.peer_adapters = vec![nic_with_mtu(r"\Device\NPF_{P}", Some(1500))];

        worker.recompute();
        assert_eq!(worker.state.effective_mtu, Some(1500));
        let plan = worker.state.plan.as_ref().unwrap();
        assert!(plan.can_start, "{:?}", plan.errors);
        assert_eq!(plan.network.payload_bytes, 1458);
        // 生效配置只是副本；保存的配置仍保持自动模式。
        assert!(worker.state.config.capture.mtu_auto);
        assert_eq!(worker.state.config.capture.mtu, 9000);

        // 没有可预览的对端网卡时，自动 MTU 无法解析，必须阻止启动。
        worker.state.peer_adapters.clear();
        worker.recompute();
        assert_eq!(worker.state.effective_mtu, None);
        let plan = worker.state.plan.as_ref().unwrap();
        assert!(!plan.can_start);
        assert!(
            plan.errors
                .iter()
                .any(|error| error.contains("自动 MTU") || error.contains("尚未确定")),
            "{:?}",
            plan.errors
        );
    }

    #[test]
    fn recompute_rejects_explicit_mtu_above_reported_host_nic() {
        let (mut worker, _tx) = test_worker("http://127.0.0.1:1");
        worker.state.config.capture.mtu_auto = false;
        worker.state.config.capture.mtu = 9000;
        worker.state.host_adapters = vec![nic_with_mtu(r"\Device\NPF_{H}", Some(1500))];
        worker.state.host_adapter = r"\Device\NPF_{H}".into();
        worker.state.peer_adapters = vec![nic_with_mtu(r"\Device\NPF_{P}", Some(9000))];

        worker.recompute();
        assert_eq!(worker.state.effective_mtu, None);
        let plan = worker.state.plan.as_ref().unwrap();
        assert!(!plan.can_start);
        assert!(
            plan.errors
                .iter()
                .any(|error| error.contains("超过") && error.contains("1500")),
            "{:?}",
            plan.errors
        );
    }

    #[test]
    fn controller_shutdown_joins_without_starting_any_process() {
        let mut controller = spawn();
        controller.shutdown();
        assert!(!controller.shared.snapshot.lock().unwrap().running);
    }

    // ---------------------------------------------------------- 模拟对端
    //
    // 这些测试只运行真实 HostController 工作线程代码和本机回环 HTTP，不启动
    // 数据面、不注册 DShow、不枚举/打开网卡。远端“采集会话”由脚本化响应模拟。

    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn test_worker(peer_url: &str) -> (Worker, Sender<Command>) {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            snapshot: Mutex::new(Snapshot::default()),
        });
        let mut worker = Worker::new(shared, rx);
        worker.config_error = false;
        worker.state.config = Config::default();
        worker.state.config.receive.vcam_autoregister = false;
        worker.state.restart_requested = false;
        worker.state.connected = true;
        worker.state.peer_url = peer_url.into();
        worker.peer = Some(PeerClient::new(peer_url));
        (worker, tx)
    }

    fn health_body(running: bool) -> String {
        json!({
            "ok": true,
            "implementation": session::IMPLEMENTATION,
            "api_revision": session::API_REVISION,
            "role": "capture",
            "running": running
        })
        .to_string()
    }

    fn http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).unwrap_or(0);
            if read == 0 {
                break None;
            }
            request.extend_from_slice(&buffer[..read]);
            if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break Some(position + 4);
            }
        };
        if let Some(header_end) = header_end {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let read = stream.read(&mut buffer).unwrap_or(0);
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
        }
        String::from_utf8_lossy(&request).into_owned()
    }

    /// 脚本化 fake peer：按顺序回放响应，脚本耗尽后对任何多余请求返回 500，
    /// 并把收到的完整请求交给测试断言。
    struct FakePeer {
        base: String,
        requests: Receiver<String>,
        stop: Sender<()>,
        thread: Option<JoinHandle<()>>,
    }

    impl FakePeer {
        fn start(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let (request_tx, requests) = mpsc::channel();
            let (stop, stop_rx) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                let mut remaining = responses.into_iter();
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(5)))
                                .unwrap();
                            let request = read_request(&mut stream);
                            let _ = request_tx.send(request);
                            let response = remaining.next().unwrap_or_else(|| {
                                http_response(
                                    "500 Internal Server Error",
                                    r#"{"error":"unexpected request"}"#,
                                )
                            });
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                base,
                requests,
                stop,
                thread: Some(thread),
            }
        }

        fn take(&self, count: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut out = Vec::new();
            while out.len() < count && Instant::now() < deadline {
                match self.requests.recv_timeout(Duration::from_millis(100)) {
                    Ok(request) => out.push(request),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            out
        }

        fn assert_no_more(&self) {
            assert!(
                self.requests.try_recv().is_err(),
                "unexpected extra request to fake peer"
            );
        }
    }

    impl Drop for FakePeer {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[test]
    fn explicit_stop_discovers_and_stops_the_exact_session() {
        let peer = FakePeer::start(vec![
            http_response("200 OK", &health_body(true)),
            http_response(
                "200 OK",
                &json!({"running":true,"session_id":"s-remote"}).to_string(),
            ),
            http_response(
                "200 OK",
                &json!({"stopped":true,"session_id":"s-remote"}).to_string(),
            ),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.handle(Command::Stop);
        let requests = peer.take(3);
        assert_eq!(requests.len(), 3, "{requests:?}");
        assert!(requests[0].starts_with("GET /health "));
        assert!(requests[1].starts_with("GET /sender/stats "));
        assert!(requests[2].starts_with("POST /sender/stop "));
        assert!(requests[2].contains("\"s-remote\""), "{}", requests[2]);
        assert!(worker.discovered_sender.is_none());
        assert!(worker.state.sender.is_null());
        assert!(!worker.state.running);
        peer.assert_no_more();
    }

    #[test]
    fn explicit_stop_refreshes_the_target_instead_of_chasing_an_old_id() {
        let peer = FakePeer::start(vec![
            http_response("200 OK", &health_body(true)),
            http_response(
                "200 OK",
                &json!({"running":true,"session_id":"s-old"}).to_string(),
            ),
            http_response("409 Conflict", r#"{"error":"session mismatch"}"#),
            http_response("200 OK", &health_body(true)),
            http_response(
                "200 OK",
                &json!({"running":true,"session_id":"s-new"}).to_string(),
            ),
            http_response(
                "200 OK",
                &json!({"stopped":true,"session_id":"s-new"}).to_string(),
            ),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.handle(Command::Stop);
        assert_eq!(worker.discovered_sender.as_deref(), Some("s-old"));
        worker.handle(Command::Stop);
        assert!(worker.discovered_sender.is_none());
        let requests = peer.take(6);
        assert_eq!(requests.len(), 6, "{requests:?}");
        assert!(requests[2].contains("\"s-old\""), "{}", requests[2]);
        assert!(requests[5].contains("\"s-new\""), "{}", requests[5]);
        peer.assert_no_more();
    }

    #[test]
    fn explicit_stop_retries_the_last_exact_session_when_stats_are_unavailable() {
        let peer = FakePeer::start(vec![
            http_response("200 OK", &health_body(true)),
            http_response(
                "200 OK",
                &json!({"running":true,"session_id":"s-remote"}).to_string(),
            ),
            http_response("500 Internal Server Error", r#"{"error":"stop failed"}"#),
            http_response("503 Service Unavailable", r#"{"error":"offline"}"#),
            http_response(
                "200 OK",
                &json!({"stopped":true,"session_id":"s-remote"}).to_string(),
            ),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.handle(Command::Stop);
        assert_eq!(worker.discovered_sender.as_deref(), Some("s-remote"));
        worker.handle(Command::Stop);
        assert!(worker.discovered_sender.is_none());
        let requests = peer.take(5);
        assert_eq!(requests.len(), 5, "{requests:?}");
        assert!(requests[2].starts_with("POST /sender/stop "));
        assert!(requests[2].contains("\"s-remote\""));
        assert!(requests[4].starts_with("POST /sender/stop "));
        assert!(requests[4].contains("\"s-remote\""));
        peer.assert_no_more();
    }

    #[test]
    fn explicit_stop_refuses_a_running_session_without_identity() {
        let peer = FakePeer::start(vec![
            http_response("200 OK", &health_body(true)),
            http_response("200 OK", r#"{"running":true,"session_id":""}"#),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.handle(Command::Stop);
        let requests = peer.take(2);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(worker.discovered_sender.is_none());
        assert!(
            worker
                .state
                .events
                .iter()
                .any(|event| event.contains("session_id")),
            "{:?}",
            worker.state.events
        );
        peer.assert_no_more();
    }

    #[test]
    fn failed_start_never_stops_the_running_remote() {
        let peer = FakePeer::start(vec![http_response("200 OK", &health_body(true))]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.handle(Command::Start);
        assert!(
            worker
                .state
                .error
                .as_deref()
                .is_some_and(|error| error.contains("已有采集会话")),
            "{:?}",
            worker.state.error
        );
        let requests = peer.take(1);
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].starts_with("GET /health "));
        assert!(worker.discovered_sender.is_none());
        peer.assert_no_more();
    }

    #[test]
    fn automatic_stop_and_shutdown_never_contact_the_remote() {
        let peer = FakePeer::start(vec![]);
        let (mut worker, _tx) = test_worker(&peer.base);
        let events = worker.state.events.len();
        // 自动回收路径（窗口关闭、轮询清理、启动失败都走这里）。
        worker.stop();
        assert_eq!(worker.state.events.len(), events);
        assert!(worker.discovered_sender.is_none());
        peer.assert_no_more();
    }

    #[test]
    fn session_to_stop_requires_running_flag_and_exact_identity() {
        assert_eq!(
            session_to_stop(&json!({"running":true,"session_id":"s-1"})).unwrap(),
            Some("s-1".into())
        );
        assert_eq!(
            session_to_stop(&json!({"running":false,"session_id":"s-1"})).unwrap(),
            None
        );
        // 缺少 running 不是“已结束”，必须按未知拒绝。
        assert!(session_to_stop(&json!({})).is_err());
        assert!(session_to_stop(&json!({"running":"yes"})).is_err());
        assert!(session_to_stop(&json!({"running":true})).is_err());
        assert!(session_to_stop(&json!({"running":true,"session_id":"  "})).is_err());
    }

    #[test]
    fn automatic_stop_uses_only_the_exact_own_session() {
        let peer = FakePeer::start(vec![http_response(
            "200 OK",
            r#"{"stopped":true,"session_id":"s-own"}"#,
        )]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.sender_pending = true;
        worker.session_id = "s-own".into();
        worker.state.running = true;
        worker.stop(); // 自动回收路径（关窗/失联/启动失败共用）
        let requests = peer.take(1);
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].starts_with("POST /sender/stop "));
        assert!(requests[0].contains("\"s-own\""), "{}", requests[0]);
        assert!(!worker.sender_pending);
        assert!(!worker.state.running);
        peer.assert_no_more();
    }

    #[test]
    fn stop_conflict_releases_own_claim_without_stopping_the_other_session() {
        let peer = FakePeer::start(vec![
            http_response("409 Conflict", r#"{"error":"session mismatch"}"#),
            http_response("200 OK", r#"{"running":true,"session_id":"s-other"}"#),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.sender_pending = true;
        worker.session_id = "s-own".into();
        worker.state.running = true;
        worker.stop_explicit();
        let requests = peer.take(2);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(requests[0].starts_with("POST /sender/stop "));
        assert!(requests[0].contains("\"s-own\""), "{}", requests[0]);
        assert!(requests[1].starts_with("GET /sender/stats "));
        // 绝不把停止请求转给远端当前（他人的）会话。
        assert!(!requests.iter().any(|request| request.contains("s-other")));
        assert!(!worker.sender_pending);
        assert!(!worker.cleanup_pending());
        assert!(!worker.state.running);
        assert!(
            worker
                .state
                .events
                .iter()
                .any(|event| event.contains("非本机创建")),
            "{:?}",
            worker.state.events
        );
        peer.assert_no_more();
    }

    #[test]
    fn unconfirmed_stop_keeps_pending_and_blocks_connect() {
        let peer = FakePeer::start(vec![
            http_response("409 Conflict", r#"{"error":"session mismatch"}"#),
            http_response(
                "503 Service Unavailable",
                r#"{"error":"stats unavailable"}"#,
            ),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.sender_pending = true;
        worker.session_id = "s-own".into();
        worker.state.running = true;
        worker.stop_explicit();
        let requests = peer.take(2);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(worker.sender_pending);
        assert!(worker.cleanup_pending());
        assert!(worker.state.cleanup_pending);
        assert!(worker.state.remote_pending);
        assert!(!worker.state.running);
        // 连接被待清理拦截，不会把重试目标冲掉。
        worker.handle(Command::Connect("http://127.0.0.1:1".into()));
        assert!(
            worker
                .state
                .error
                .as_deref()
                .is_some_and(|error| error.contains("先停止")),
            "{:?}",
            worker.state.error
        );
        peer.assert_no_more();
    }

    #[test]
    fn unknown_discovery_stop_surfaces_pending_without_an_id() {
        let peer = FakePeer::start(vec![
            http_response("200 OK", &health_body(true)),
            http_response(
                "503 Service Unavailable",
                r#"{"error":"stats unavailable"}"#,
            ),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.handle(Command::Stop);
        let requests = peer.take(2);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(worker.remote_stop_pending);
        assert!(worker.discovered_sender.is_none());
        assert!(worker.state.remote_pending);
        assert!(worker.state.cleanup_pending);
        assert!(
            worker
                .state
                .events
                .iter()
                .any(|event| event.contains("可再次停止重试")),
            "{:?}",
            worker.state.events
        );
        peer.assert_no_more();
    }

    #[test]
    fn poll_clears_pending_only_after_stats_confirm_the_target_is_gone() {
        let peer = FakePeer::start(vec![
            http_response("200 OK", &health_body(true)),
            http_response(
                "200 OK",
                &json!({"running":true,"session_id":"s-remote"}).to_string(),
            ),
            http_response("500 Internal Server Error", r#"{"error":"stop failed"}"#),
            http_response("200 OK", &health_body(false)),
            http_response("200 OK", r#"{"running":false,"session_id":null}"#),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.handle(Command::Stop);
        assert!(worker.discovered_sender.is_some());
        assert!(worker.remote_stop_pending);
        let setup = peer.take(3);
        assert_eq!(setup.len(), 3, "{setup:?}");
        assert!(setup[2].starts_with("POST /sender/stop "));
        assert!(setup[2].contains("\"s-remote\""), "{}", setup[2]);
        worker.poll();
        let requests = peer.take(2);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(requests[0].starts_with("GET /health "));
        assert!(requests[1].starts_with("GET /sender/stats "));
        assert!(worker.discovered_sender.is_none());
        assert!(!worker.remote_stop_pending);
        assert!(!worker.cleanup_pending());
        assert!(!worker.state.remote_pending);
        assert!(!worker.state.running);
        peer.assert_no_more();
    }

    #[test]
    fn poll_never_stops_a_replaced_remote_session() {
        let peer = FakePeer::start(vec![
            http_response("200 OK", &health_body(true)),
            http_response(
                "200 OK",
                &json!({"running":true,"session_id":"s-other"}).to_string(),
            ),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.sender_pending = true;
        worker.session_id = "s-own".into();
        worker.state.running = true;
        worker.poll();
        let requests = peer.take(2);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(requests[0].starts_with("GET /health "));
        assert!(requests[1].starts_with("GET /sender/stats "));
        assert!(!requests.iter().any(|request| request.starts_with("POST ")));
        assert!(!worker.sender_pending);
        assert!(!worker.cleanup_pending());
        assert!(!worker.state.running);
        peer.assert_no_more();
    }

    #[test]
    fn poll_malformed_stats_keeps_the_claim() {
        let peer = FakePeer::start(vec![
            http_response("200 OK", &health_body(true)),
            http_response("200 OK", r#"{"session_id":"s-own"}"#),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.sender_pending = true;
        worker.session_id = "s-own".into();
        worker.poll();
        let requests = peer.take(2);
        assert_eq!(requests.len(), 2, "{requests:?}");
        // running 不可确认时不能当作会话已结束，本机 claim 必须保留。
        assert!(worker.sender_pending);
        assert!(worker.cleanup_pending());
        assert!(worker.state.remote_pending);
        assert!(!requests.iter().any(|request| request.starts_with("POST ")));
        peer.assert_no_more();
    }

    #[test]
    fn poll_releases_a_replaced_explicit_target_without_stopping_the_new_session() {
        let peer = FakePeer::start(vec![
            http_response("200 OK", &health_body(true)),
            http_response(
                "200 OK",
                &json!({"running":true,"session_id":"s-new"}).to_string(),
            ),
        ]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.discovered_sender = Some("s-old".into());
        worker.remote_stop_pending = true;
        worker.poll();
        let requests = peer.take(2);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(requests[0].starts_with("GET /health "));
        assert!(requests[1].starts_with("GET /sender/stats "));
        // 精确目标已被替换就说明它已停止；只回收本机资源，绝不停止新会话。
        assert!(!requests.iter().any(|request| request.starts_with("POST ")));
        assert!(worker.discovered_sender.is_none());
        assert!(!worker.remote_stop_pending);
        assert!(!worker.cleanup_pending());
        assert!(!worker.state.remote_pending);
        peer.assert_no_more();
    }

    #[test]
    fn local_cleanup_never_contacts_the_remote() {
        let peer = FakePeer::start(vec![]);
        let (mut worker, _tx) = test_worker(&peer.base);
        worker.discovered_sender = Some("s-stale".into());
        worker.handle(Command::CleanupLocal);
        // 本地清理保留重试目标，但不发任何远端请求（尤其不停止其他会话）。
        assert_eq!(worker.discovered_sender.as_deref(), Some("s-stale"));
        assert!(worker.cleanup_pending());
        assert!(!worker.state.running);
        peer.assert_no_more();
    }
}
