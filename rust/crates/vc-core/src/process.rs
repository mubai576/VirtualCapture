//! 数据面会话槽位：通过进程内 `vc_stream.dll` 启动/停止采集与接收，读取状态与日志尾部。
//!
//! 槽位只持有一个 `Module` 和一个 `StreamSession`。就绪以 DLL 状态中的真实
//! `ready`/`error`/`exit_code` 为准，不靠进程存活猜；有限帧会话可能在首次查询前
//! 就已完成，因此 `ready && 已结束` 仍算成功，`未 ready 且已结束` 才是失败。
//!
//! 停止语义：`Ok(true)` 表示工作线程已 join，可以释放句柄；`Ok(false)` 或错误表示
//! 资源仍被占用，必须保留句柄、置 `cleanup_pending`，由调用方重试停止，并且不得
//! 在 Drop 时丢弃活动句柄。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use vc_transport::{Geometry, packet_count, payload_max_for_mtu};

use crate::config::{Capture, Config};
use crate::dataplane::{Module, StreamSession};
use crate::paths;
use crate::session::parse_mac;

const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// PyroWave 接收端就绪前最多包含 5 个各自受 5 秒看门狗约束的同步操作
/// （解码器创建、临时编码器创建/预热/销毁、解码器预热），单步 10 秒上限
/// 会误杀慢但合法的冷启动；该值覆盖其合法上界，且仍小于 REST start 的
/// 40 秒总预算。
const PYROWAVE_READY_TIMEOUT: Duration = Duration::from_secs(30);
const READY_POLL: Duration = Duration::from_millis(25);
/// `stop` ABI 调用本身非阻塞；先有界轮询，仍为 false 才向调用方报告 Pending。
const STOP_GRACE: Duration = Duration::from_secs(2);

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

    pub fn role(self) -> &'static str {
        match self {
            Kind::Sender => "sender",
            Kind::Receiver => "receiver",
        }
    }
}

/// 数据面就绪时对 `batch` 回显的要求。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchEcho {
    /// 不检查（接收端没有发送 batch 语义）。
    Ignore,
    /// Raw 必须回显精确 batch；编码会话必须回显 null 或缺失。
    Expect(Option<u32>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// 工作线程已 join，句柄可以释放。
    Stopped,
    /// 停止请求已发出但资源仍被占用；句柄保留，必须重试。
    Pending,
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
    module: Option<Module>,
    session: Option<StreamSession>,
    log: Option<File>,
    started_at: Option<Instant>,
    cleanup_pending: bool,
    last_stats: Map<String, Value>,
    last_exit: Option<i64>,
    last_error: Option<String>,
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
            module: None,
            session: None,
            log: None,
            started_at: None,
            cleanup_pending: false,
            last_stats: Map::new(),
            last_exit: None,
            last_error: None,
        }
    }

    /// 按运行布局装载并缓存数据面模块；失败不缓存，下一次仍会明确报错。
    pub fn data_plane_module(&mut self) -> Result<Module, String> {
        if self.module.is_none() {
            self.module = Some(crate::dataplane::load()?);
        }
        Ok(self
            .module
            .as_ref()
            .expect("module was just loaded")
            .clone())
    }

    /// 会话是否仍占用数据面资源。停止未确认时保持 true，供配置/OTA/虚拟卡门禁用。
    pub fn cleanup_pending(&self) -> bool {
        self.cleanup_pending
    }

    /// 无活动会话且无待清理残留时为 true，轮询空闲路径可跳过统计构建。
    pub fn is_idle(&self) -> bool {
        self.session.is_none() && !self.cleanup_pending
    }

    pub fn running(&mut self) -> bool {
        if self.cleanup_pending {
            self.retry_completed_cleanup();
            if self.cleanup_pending {
                return true;
            }
        }
        let status = match self.session.as_ref() {
            Some(session) => session.stats(),
            None => return false,
        };
        match status {
            Ok(status) => {
                if let Some(map) = status.as_object() {
                    self.record_status(map);
                }
                status_running(&status)
            }
            // 查询失败时 fail-closed：不能据此释放或允许修改配置。
            Err(_) => true,
        }
    }

    /// 停旧会话、创建控制日志、通过 DLL 启动新会话，并等待真实就绪。
    ///
    /// 就绪失败时保留并尝试停止句柄；停止未确认则把错误与重试提示一起返回。
    pub fn start(
        &mut self,
        request: &Value,
        adapter: &str,
        session_id: &str,
        run_dir: &Path,
        batch: BatchEcho,
    ) -> Result<u32, String> {
        self.stop_previous()?;
        let module = self.data_plane_module()?;
        let (stats_path, hashlog_path, stdout_path) = session_paths(run_dir, session_id, self.kind);
        if [&stats_path, &hashlog_path, &stdout_path]
            .iter()
            .any(|path| path.exists())
        {
            return Err("会话号已有运行证据，请使用新的 session_id；未覆盖旧文件。".into());
        }
        std::fs::create_dir_all(run_dir).map_err(|error| format!("无法创建 run 目录：{error}"))?;
        // stats/hashlog 由 DLL 以 create_new 方式创建，控制面只写自己的控制日志，
        // 避免在 DLL 拒绝启动时留下伪造的运行证据。
        let log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&stdout_path)
            .map_err(|error| format!("无法创建控制日志：{error}"))?;

        self.session_id = Some(session_id.to_string());
        self.stats_path = stats_path;
        self.hashlog_path = hashlog_path;
        self.stdout_path = stdout_path;
        self.adapter = adapter.to_string();
        self.mode = None;
        self.pattern = None;
        self.started_at = Some(Instant::now());
        self.cleanup_pending = false;
        self.last_stats.clear();
        self.last_exit = None;
        self.last_error = None;
        self.log = Some(log);
        self.log_line(&format!(
            "启动{}会话 {session_id}，网卡 {adapter}",
            self.kind.label()
        ));

        let started = match self.kind {
            Kind::Sender => module.start_sender(request),
            Kind::Receiver => module.start_receiver(request),
        };
        let session = match started {
            Ok(session) => session,
            Err(error) => {
                // 同步拒绝时没有数据面会话：保留错误供 /sender/stats 复盘，
                // 但不保留从未开始的 uptime。
                self.log_line(&format!("DLL 拒绝启动：{error}"));
                self.last_error = Some(error.clone());
                self.started_at = None;
                return Err(error);
            }
        };
        self.session = Some(session);
        match self.wait_ready(adapter, session_id, batch, ready_timeout(request)) {
            Ok(pid) => {
                self.log_line(&format!("数据面已就绪，进程内 pid={pid}"));
                Ok(pid)
            }
            Err(error) => {
                self.log_line(&format!("就绪失败：{error}"));
                Err(self.stop_after_failure(error))
            }
        }
    }

    /// 仅当 DLL 明确报告 worker 已结束时尝试 join/reap；不在此路径发停止信号。
    /// ABI 状态查询失败或仍运行时保留原句柄与 fail-closed 门禁。
    fn retry_completed_cleanup(&mut self) {
        if !self.cleanup_pending {
            return;
        }
        if self.session.is_none() {
            self.cleanup_pending = false;
            return;
        }
        let terminal = match self.session.as_ref().map(StreamSession::stats) {
            Some(Ok(status)) => {
                if let Some(map) = status.as_object() {
                    self.record_status(map);
                }
                !status_running(&status)
            }
            _ => false,
        };
        if !terminal {
            return;
        }
        let _ = self.stop_inner(Duration::ZERO, false, "已回收此前结束的数据面会话");
    }

    fn stop_previous(&mut self) -> Result<(), String> {
        if self.cleanup_pending {
            self.retry_completed_cleanup();
        }
        if self.session.is_none() {
            self.cleanup_pending = false;
            return Ok(());
        }
        match self.stop() {
            Ok(StopOutcome::Stopped) => Ok(()),
            Ok(StopOutcome::Pending) => {
                Err("上一会话停止未确认；句柄已保留，请重试停止后再启动。".into())
            }
            Err(error) => Err(format!("上一会话停止失败且句柄已保留：{error}")),
        }
    }

    fn stop_after_failure(&mut self, error: String) -> String {
        match self.stop() {
            Ok(StopOutcome::Stopped) => error,
            Ok(StopOutcome::Pending) => {
                format!("{error}；数据面会话停止未确认，句柄已保留，请重试停止。")
            }
            Err(stop) => format!("{error}；停止失败且句柄已保留待重试：{stop}"),
        }
    }

    fn wait_ready(
        &mut self,
        adapter: &str,
        session_id: &str,
        batch: BatchEcho,
        timeout: Duration,
    ) -> Result<u32, String> {
        let deadline = Instant::now() + timeout;
        let mut last_query_error: Option<String> = None;
        let mut last_status: Option<String> = None;
        loop {
            let status = match self.session.as_ref() {
                Some(session) => session.stats(),
                None => return Err("数据面会话句柄丢失，无法确认就绪。".into()),
            };
            match status {
                Ok(status) => {
                    if let Some(map) = status.as_object() {
                        self.record_status(map);
                    }
                    last_status = Some(compact_status(&status));
                    match ready_outcome(&status) {
                        ReadyOutcome::Ready => {
                            if let Err(error) =
                                validate_ready_echo(&status, session_id, adapter, batch)
                            {
                                // 回显不符也是启动失败，必须留证据，不能只返回给调用方。
                                self.last_error = Some(error.clone());
                                return Err(error);
                            }
                            let pid = status
                                .get("pid")
                                .and_then(Value::as_u64)
                                .unwrap_or(u64::from(std::process::id()));
                            return Ok(pid.min(u64::from(u32::MAX)) as u32);
                        }
                        ReadyOutcome::Failed(error) => {
                            self.last_error = Some(error.clone());
                            return Err(error);
                        }
                        ReadyOutcome::Pending => {}
                    }
                }
                Err(error) => last_query_error = Some(error),
            }
            if Instant::now() >= deadline {
                let detail = last_query_error
                    .or(last_status)
                    .unwrap_or_else(|| "无状态响应".into());
                let error = format!(
                    "数据面会话在 {} 秒内未确认就绪；句柄已保留待重试停止。最后状态：{detail}",
                    timeout.as_secs()
                );
                self.last_error = Some(error.clone());
                return Err(error);
            }
            std::thread::sleep(READY_POLL);
        }
    }

    /// 带历史会话信息标记模式（UI 展示用）。
    pub fn mark_mode(&mut self, mode: Option<String>, pattern: Option<String>) {
        self.mode = mode;
        self.pattern = pattern;
    }

    /// 请求停止。只有 `Ok(Stopped)` 才释放句柄；Pending/Err 都保留以便重试。
    pub fn stop(&mut self) -> Result<StopOutcome, String> {
        self.stop_inner(STOP_GRACE, true, "数据面会话已停止并完成回收")
    }

    fn stop_inner(
        &mut self,
        grace: Duration,
        log_request: bool,
        stopped_message: &str,
    ) -> Result<StopOutcome, String> {
        let Some(mut session) = self.session.take() else {
            self.cleanup_pending = false;
            self.started_at = None;
            self.log = None;
            return Ok(StopOutcome::Stopped);
        };
        // 停止前抓一次最终状态，保留计数与错误证据。
        if let Ok(Value::Object(status)) = session.stats() {
            self.record_status(&status);
        }
        if log_request {
            self.log_line("请求停止数据面会话");
        }
        let mut outcome = session.stop();
        if matches!(outcome, Ok(false)) && !grace.is_zero() {
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                std::thread::sleep(READY_POLL);
                outcome = session.stop();
                if !matches!(outcome, Ok(false)) {
                    break;
                }
            }
        }
        match outcome {
            Ok(true) => {
                // 工作线程已 join，句柄仍有效：补抓一次终态，让 last_stats/
                // last_exit/last_error 与落盘 stats 文件一致，而不是停在停止前的
                // 中间状态。
                if let Ok(Value::Object(status)) = session.stats() {
                    self.record_status(&status);
                }
                self.log_line(stopped_message);
                self.cleanup_pending = false;
                self.started_at = None;
                if self.last_exit.is_none() {
                    self.last_exit = Some(0);
                }
                self.log = None;
                Ok(StopOutcome::Stopped)
            }
            Ok(false) => {
                if log_request {
                    self.log_line("停止未完成：保留会话句柄等待重试");
                }
                self.session = Some(session);
                self.cleanup_pending = true;
                Ok(StopOutcome::Pending)
            }
            Err(error) => {
                if log_request {
                    self.log_line(&format!("停止失败：保留会话句柄等待重试：{error}"));
                } else {
                    self.log_line(&format!("回收已结束的数据面会话失败：{error}"));
                }
                self.last_error = Some(error.clone());
                self.session = Some(session);
                self.cleanup_pending = true;
                Err(error)
            }
        }
    }

    /// DLL 状态 + 控制面字段。会话停止后继续返回最后一次状态，便于 UI 展示。
    pub fn stats(&mut self) -> Value {
        self.poll_status().1
    }

    /// 单次 FFI 状态轮询，同时返回 `(running, stats)`，避免同一轮询周期重复查询与克隆。
    pub fn poll_status(&mut self) -> (bool, Value) {
        if self.cleanup_pending {
            self.retry_completed_cleanup();
        }
        let status = self.session.as_ref().map(StreamSession::stats);
        let mut query_error: Option<String> = None;
        let mut map = match status {
            Some(Ok(Value::Object(status))) => {
                self.record_status(&status);
                status
            }
            Some(Ok(_)) => {
                query_error = Some("数据面状态不是 JSON 对象。".into());
                self.last_stats.clone()
            }
            Some(Err(error)) => {
                query_error = Some(error);
                self.last_stats.clone()
            }
            None => self.last_stats.clone(),
        };
        let live = self.session.is_some();
        let running = if self.cleanup_pending {
            true
        } else if !live {
            false
        } else if query_error.is_some() {
            true
        } else {
            status_running_map(&map)
        };
        map.insert(
            "session_id".into(),
            Value::String(self.session_id.clone().unwrap_or_default()),
        );
        map.insert("adapter".into(), Value::String(self.adapter.clone()));
        map.insert(
            "uptime_s".into(),
            Value::from(
                self.started_at
                    .map(|start| (start.elapsed().as_millis() as f64) / 1000.0)
                    .unwrap_or(0.0),
            ),
        );
        if self.kind == Kind::Sender {
            if let Some(mode) = &self.mode {
                map.insert("mode".into(), Value::String(mode.clone()));
            }
            if let Some(pattern) = &self.pattern {
                map.insert("pattern".into(), Value::String(pattern.clone()));
            }
        }
        if live {
            if !map.contains_key("running") {
                // 查询异常时按仍在运行处理（fail-closed），不据此放行配置或启动。
                map.insert("running".into(), Value::Bool(true));
            }
        } else {
            map.insert("running".into(), Value::Bool(false));
        }
        if self.cleanup_pending {
            map.insert("running".into(), Value::Bool(true));
        }
        if live && !map.contains_key("pid") {
            map.insert("pid".into(), Value::from(std::process::id()));
        }
        if !live
            && let Some(code) = self.last_exit
            && map.get("exit_code").is_none_or(Value::is_null)
        {
            map.insert("exit_code".into(), Value::from(code));
        }
        attach_error(&mut map, query_error, self.last_error.as_deref());
        let has_error = map
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| !error.is_empty());
        let failed_exit = map
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0);
        if (has_error || failed_exit) && !self.stdout_path.as_os_str().is_empty() {
            if !map.contains_key("log_tail")
                && let Some(tail) = self.log_tail()
            {
                map.insert("log_tail".into(), Value::String(tail));
            }
            if let Ok(rel) = self.stdout_path.strip_prefix(paths::root()) {
                map.insert(
                    "stdout".into(),
                    Value::String(rel.to_string_lossy().replace('\\', "/")),
                );
            }
        }
        (running, Value::Object(map))
    }

    fn record_status(&mut self, status: &Map<String, Value>) {
        self.last_stats = status.clone();
        if let Some(code) = status.get("exit_code").and_then(Value::as_i64) {
            self.last_exit = Some(code);
        }
        if let Some(error) = status
            .get("error")
            .and_then(Value::as_str)
            .filter(|error| !error.is_empty())
        {
            self.last_error = Some(error.to_string());
        }
    }

    fn log_line(&mut self, message: &str) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        if let Some(log) = self.log.as_mut() {
            let _ = writeln!(log, "{secs} {message}");
            let _ = log.flush();
        }
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

impl Drop for NativeSlot {
    fn drop(&mut self) {
        if self.session.is_none() {
            return;
        }
        if matches!(self.stop(), Ok(StopOutcome::Stopped)) {
            return;
        }
        // 停止未确认：不能触发 StreamSession 的 Drop 去销毁活动句柄，
        // 也不能卸载仍在执行回调的 DLL。保留两者，交由进程退出回收。
        if let Some(session) = self.session.take() {
            std::mem::forget(session);
        }
        if let Some(module) = self.module.take() {
            std::mem::forget(module);
        }
        self.cleanup_pending = true;
    }
}

#[derive(Debug)]
enum ReadyOutcome {
    Pending,
    Ready,
    Failed(String),
}

fn ready_outcome(status: &Value) -> ReadyOutcome {
    if let Some(error) = status
        .get("error")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
    {
        return ReadyOutcome::Failed(error.to_string());
    }
    let exit = status.get("exit_code").and_then(Value::as_i64);
    if let Some(code) = exit
        && code != 0
    {
        return ReadyOutcome::Failed(format!("数据面会话退出（exit_code={code}）"));
    }
    if status.get("ready").and_then(Value::as_bool) == Some(true) {
        return ReadyOutcome::Ready;
    }
    if exit.is_some() && status.get("running").and_then(Value::as_bool) == Some(false) {
        return ReadyOutcome::Failed("数据面会话在就绪前结束，未报告 ready。".into());
    }
    ReadyOutcome::Pending
}

fn validate_ready_echo(
    status: &Value,
    session_id: &str,
    adapter: &str,
    batch: BatchEcho,
) -> Result<(), String> {
    let reported_session = status
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("数据面就绪状态缺少 session_id。")?;
    if reported_session != session_id {
        return Err(format!(
            "数据面就绪会话号不符：期望 {session_id}，实际 {reported_session}。"
        ));
    }
    let reported_adapter = status
        .get("adapter")
        .and_then(Value::as_str)
        .ok_or("数据面就绪状态缺少 adapter。")?;
    if !reported_adapter.eq_ignore_ascii_case(adapter) {
        return Err(format!(
            "数据面就绪网卡不符：期望 {adapter}，实际 {reported_adapter}。"
        ));
    }
    let BatchEcho::Expect(expected) = batch else {
        return Ok(());
    };
    match (expected, status.get("batch")) {
        (Some(expected), Some(value)) => {
            if value.as_u64() != Some(u64::from(expected)) {
                return Err(format!(
                    "数据面 Raw batch 回显不符：期望 {expected}，实际 {value}。"
                ));
            }
        }
        (Some(_), None) => {
            return Err("数据面就绪状态缺少 Raw batch 回显，拒绝按未确认参数启动。".into());
        }
        (None, Some(value)) if !value.is_null() => {
            return Err(format!("编码会话不应回显固定 batch：{value}。"));
        }
        _ => {}
    }
    Ok(())
}

fn has_error_map(status: &Map<String, Value>) -> bool {
    status
        .get("error")
        .and_then(Value::as_str)
        .is_some_and(|error| !error.is_empty())
}

/// 从 DLL 状态判断会话是否仍占用资源。
///
/// 显式 `running` 优先；缺失时按终态字段兜底。`exit_code: null` 是 ABI 表示
/// “运行中”的合法值，不能被当成终态，因此缺字段时一律 fail-closed。
fn status_running_map(status: &Map<String, Value>) -> bool {
    if let Some(running) = status.get("running").and_then(Value::as_bool) {
        return running;
    }
    if has_error_map(status) {
        return false;
    }
    status.get("exit_code").is_none_or(Value::is_null)
}

fn status_running(status: &Value) -> bool {
    status.as_object().is_none_or(status_running_map)
}

/// 数据面就绪等待上限。PyroWave 预热包含多个各自受 5 秒看门狗约束的同步
/// 步骤，不能沿用单步操作的 10 秒上限；其余后端保持原值。
fn ready_timeout(request: &Value) -> Duration {
    if request
        .get("codec")
        .and_then(|codec| codec.get("codec"))
        .and_then(Value::as_str)
        == Some("pyrowave")
    {
        PYROWAVE_READY_TIMEOUT
    } else {
        READY_TIMEOUT
    }
}

/// 把查询错误或控制面记录的错误并入状态。DLL 已给出 error 时以 DLL 为准；
/// 会话仍存活但已判定失败（未就绪/停止未确认）时也必须暴露原因，否则
/// /stats 会显示 running=true 却没有任何错误线索。
fn attach_error(
    map: &mut Map<String, Value>,
    query_error: Option<String>,
    last_error: Option<&str>,
) {
    if let Some(error) = query_error {
        map.insert("error".into(), Value::String(error));
    } else if !map.contains_key("error")
        && let Some(error) = last_error
    {
        map.insert("error".into(), Value::String(error.to_string()));
    }
}

fn compact_status(status: &Value) -> String {
    let text = serde_json::to_string(status).unwrap_or_default();
    text.chars().take(512).collect()
}

/// 会话文件路径：`<sid>.<kind>.{stats.json,hash.jsonl,stdout.log}`。
pub fn session_paths(run_dir: &Path, session_id: &str, kind: Kind) -> (PathBuf, PathBuf, PathBuf) {
    let tag = kind.role();
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

/// Raw 每次 sendqueue 提交覆盖一整帧，避免配置尺寸变化留下过期 batch。
pub fn raw_frame_batch(config: &Config) -> Result<u32, String> {
    raw_frame_batch_from_capture(&config.capture)
}

pub fn raw_frame_batch_from_capture(capture: &Capture) -> Result<u32, String> {
    let geometry =
        Geometry::new(capture.width, capture.height).map_err(|error| error.to_string())?;
    let payload = payload_max_for_mtu(capture.mtu as usize).map_err(|error| error.to_string())?;
    let packets =
        packet_count(geometry.frame_bytes(), payload).map_err(|error| error.to_string())?;
    Ok(u32::from(packets))
}

/// Npcap 回环不支持大 sendqueue；它只用于诊断，按包提交（batch=1）。
pub fn raw_send_batch_for_adapter(capture: &Capture, adapter: &str) -> Result<u32, String> {
    if adapter.eq_ignore_ascii_case(r"\Device\NPF_Loopback") {
        Ok(1)
    } else {
        raw_frame_batch_from_capture(capture)
    }
}

/// 准备响应/启动就绪应回显的 batch：Raw 为精确几何值，编码为 None。
pub fn expected_batch_packets(capture: &Capture, adapter: &str) -> Result<Option<u32>, String> {
    if capture.codec == "raw" {
        raw_send_batch_for_adapter(capture, adapter).map(Some)
    } else {
        Ok(None)
    }
}

/// 采集端 sender 请求。`batch` 由 DLL 按实际选择后的网卡自动计算，控制面不覆盖。
pub fn sender_request(
    config: &Config,
    adapter: &str,
    session: &str,
    stats: &Path,
    hashlog: &Path,
    source_mac: &str,
) -> Result<Value, String> {
    let c = &config.capture;
    Ok(json!({
        "adapter": adapter,
        "width": c.width,
        "height": c.height,
        "fps": c.fps,
        "mtu": c.mtu,
        "session": session,
        "frames": c.frames,
        "pacing_us": 0u64,
        "source_mac": mac_array(source_mac, "source_mac")?,
        "dst_mac": mac_array(&c.dst_mac, "capture.dst_mac")?,
        "stats": absolute(stats),
        "hashlog": absolute(hashlog),
        "source": source_json(c),
        "codec": codec_json(c),
    }))
}

/// 主机 receiver 请求；帧数与超时 0 表示不设上限（与历史 receiver 一致）。
pub fn receiver_request(
    config: &Config,
    adapter: &str,
    session: &str,
    stats: &Path,
    hashlog: &Path,
    source_mac: &str,
) -> Result<Value, String> {
    let c = &config.capture;
    Ok(json!({
        "adapter": adapter,
        "width": c.width,
        "height": c.height,
        "fps": c.fps,
        "mtu": config.receive.mtu,
        "session": session,
        "source_mac": mac_array(source_mac, "source_mac")?,
        "expect_frames": 0u64,
        "timeout_s": 0u64,
        "shm": config.receive.shm,
        "stats": absolute(stats),
        "hashlog": absolute(hashlog),
        "codec": codec_json(c),
    }))
}

fn mac_array(text: &str, field: &str) -> Result<[u8; 6], String> {
    parse_mac(text).ok_or_else(|| format!("{field} 必须是完整 MAC 地址：{text}"))
}

fn absolute(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn source_json(c: &Capture) -> Value {
    json!({
        "mode": c.mode,
        "pattern": c.pattern,
        "output": c.output,
        "window": c.wgc_window,
        "transform": c.transform,
        "gpu_convert": c.gpu_convert,
    })
}

fn codec_json(c: &Capture) -> Value {
    json!({
        "codec": c.codec,
        "experimental": c.codec_experimental,
        "encoder_backend": c.encoder_backend,
        "decoder_backend": c.decoder_backend,
        "jpeg_quality": c.jpeg_quality,
        "max_au_bytes": c.max_au_bytes,
        "max_bitrate_bps": u64::from(c.max_bitrate_mbps) * 1_000_000,
        "decode_in_flight": c.decode_in_flight,
    })
}

/// 两端启动前共同校验：结构、MTU 一致、编码显式启用且后端精确匹配。
pub fn validate_start_config(config: &Config) -> Result<(), String> {
    let errors = config.validate_start();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
}

/// 查询并校验数据面模块的公开能力；返回 capabilities 供本次启动继续判断组合。
pub fn require_capabilities(module: &Module) -> Result<Value, String> {
    let caps = module.capabilities()?;
    validate_capabilities(&caps)?;
    Ok(caps)
}

pub fn validate_capabilities(caps: &Value) -> Result<(), String> {
    let mut missing = Vec::new();
    if caps.get("stream_revision").and_then(Value::as_u64) != Some(2) {
        missing.push("stream_revision=2");
    }
    if caps.get("shm_version").and_then(Value::as_u64) != Some(3) {
        missing.push("shm_version=3");
    }
    if caps.get("variable_resolution").and_then(Value::as_bool) != Some(true) {
        missing.push("variable_resolution=true");
    }
    if caps.get("sendqueue").and_then(Value::as_str) != Some("frame") {
        missing.push("sendqueue=frame");
    }
    // 只声明 Raw 与 Vulkan GPU 计算的 PyroWave；MJPEG 的 CPU/SIMD 后端已被移除，
    // 能力表不能继续声称支持它，否则会误导宿主选择不可启动的组合。
    if !declares(caps, "codecs", &["raw", "pyrowave"]) {
        missing.push("codecs=[raw,pyrowave]");
    }
    if !declares(caps, "sources", &["pattern", "dda", "wgc"]) {
        missing.push("sources=[pattern,dda,wgc]");
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "数据面模块缺少能力：{}；请部署匹配的 vc_stream.dll，不回退旧程序。",
            missing.join("、")
        ))
    }
}

/// 本次请求的编码/来源是否在模块声明范围内；未声明组合必须明确拒绝。
pub fn require_stream_selection(
    caps: &Value,
    codec: &str,
    source: Option<&str>,
) -> Result<(), String> {
    if !declares(caps, "codecs", &[codec]) {
        return Err(format!("数据面模块未声明支持 codec={codec}，拒绝启动。"));
    }
    if let Some(source) = source
        && !declares(caps, "sources", &[source])
    {
        return Err(format!(
            "数据面模块未声明支持 source.mode={source}，拒绝启动。"
        ));
    }
    Ok(())
}

fn declares(caps: &Value, key: &str, wanted: &[&str]) -> bool {
    caps.get(key)
        .and_then(Value::as_array)
        .is_some_and(|values| {
            wanted
                .iter()
                .all(|required| values.iter().any(|value| value.as_str() == Some(*required)))
        })
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

/// 为当前进程分配隐藏控制台（regsvr32/诊断工具与旧日志行为兼容）。
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
    use serde_json::json;

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
    fn raw_batch_tracks_frame_geometry_and_mtu() {
        let mut config = Config::default();
        assert_eq!(raw_frame_batch(&config).unwrap(), 69);

        config.capture.width = 416;
        config.capture.height = 416;
        assert_eq!(raw_frame_batch(&config).unwrap(), 29);

        config.capture.width = 640;
        config.capture.height = 640;
        config.capture.mtu = 1500;
        assert_eq!(raw_frame_batch(&config).unwrap(), 422);

        config.capture.width = 1280;
        config.capture.height = 720;
        config.capture.mtu = 9000;
        assert_eq!(raw_frame_batch(&config).unwrap(), 155);
    }

    #[test]
    fn expected_batch_is_exact_for_raw_and_null_for_encoded() {
        let raw = Capture::default();
        assert_eq!(
            expected_batch_packets(&raw, r"\Device\NPF_Loopback").unwrap(),
            Some(1)
        );
        assert_eq!(expected_batch_packets(&raw, "physical").unwrap(), Some(69));
        let encoded = Capture {
            codec: "pyrowave".into(),
            ..Default::default()
        };
        assert_eq!(expected_batch_packets(&encoded, "physical").unwrap(), None);
    }

    #[test]
    fn sender_request_carries_geometry_source_codec_and_no_batch_override() {
        let config = Config::default();
        let request = sender_request(
            &config,
            r"\Device\NPF_Loopback",
            "s-1",
            Path::new("run/s.json"),
            Path::new("run/h.jsonl"),
            "02:00:00:00:00:01",
        )
        .unwrap();
        assert_eq!(request["adapter"], json!(r"\Device\NPF_Loopback"));
        assert_eq!(request["width"], json!(640));
        assert_eq!(request["height"], json!(640));
        assert_eq!(request["fps"], json!(240));
        assert_eq!(request["mtu"], json!(9000));
        assert_eq!(request["session"], json!("s-1"));
        assert_eq!(request["frames"], json!(0));
        assert_eq!(request["pacing_us"], json!(0));
        assert_eq!(request["source_mac"], json!([2, 0, 0, 0, 0, 1]));
        assert_eq!(request["dst_mac"], json!([255, 255, 255, 255, 255, 255]));
        assert_eq!(request["source"]["mode"], json!("dda"));
        assert_eq!(request["source"]["pattern"], json!("motion"));
        assert_eq!(request["source"]["transform"], json!("crop"));
        assert_eq!(request["source"]["gpu_convert"], json!(true));
        assert_eq!(request["codec"]["codec"], json!("raw"));
        assert_eq!(request["codec"]["experimental"], json!(false));
        assert_eq!(request["codec"]["max_bitrate_bps"], json!(80_000_000u64));
        assert!(request.get("batch").is_none());
        assert!(
            request["stats"]
                .as_str()
                .is_some_and(|path| path.ends_with("s.json"))
        );
        assert!(
            request["hashlog"]
                .as_str()
                .is_some_and(|path| path.ends_with("h.jsonl"))
        );
    }

    #[test]
    fn receiver_request_is_unlimited_and_carries_shm() {
        let config = Config::default();
        let request = receiver_request(
            &config,
            r"\Device\NPF_{X}",
            "r-1",
            Path::new("run/r.json"),
            Path::new("run/h.jsonl"),
            "02:00:00:00:00:02",
        )
        .unwrap();
        assert_eq!(request["expect_frames"], json!(0));
        assert_eq!(request["timeout_s"], json!(0));
        assert_eq!(request["shm"], json!(r"Local\VirtualCaptureVcamFrame"));
        assert_eq!(request["source_mac"], json!([2, 0, 0, 0, 0, 2]));
        assert_eq!(request["codec"]["codec"], json!("raw"));
        assert!(request.get("source").is_none());
    }

    #[test]
    fn request_builders_reject_malformed_macs() {
        let config = Config::default();
        assert!(
            sender_request(
                &config,
                "a",
                "s-1",
                Path::new("s.json"),
                Path::new("h.jsonl"),
                "not-a-mac"
            )
            .is_err()
        );
        let mut bad = config;
        bad.capture.dst_mac = "gg:00:00:00:00:00".into();
        assert!(
            sender_request(
                &bad,
                "a",
                "s-1",
                Path::new("s.json"),
                Path::new("h.jsonl"),
                "02:00:00:00:00:01"
            )
            .is_err()
        );
    }

    #[test]
    fn capabilities_require_exact_product_contract() {
        let good = json!({
            "stream_revision": 2,
            "shm_version": 3,
            "variable_resolution": true,
            "sendqueue": "frame",
            "codecs": ["raw", "pyrowave"],
            "sources": ["pattern", "dda", "wgc"]
        });
        validate_capabilities(&good).unwrap();
        require_stream_selection(&good, "pyrowave", Some("dda")).unwrap();
        // 旧 DLL 仍多声明 mjpeg 不影响 Raw/PyroWave 契约；能力表只是声明层，
        // 真正拒绝 mjpeg 启动的是配置启动门限（validate_start），二者不互相代替。
        let mut with_removed = good.clone();
        with_removed["codecs"] = json!(["raw", "mjpeg", "pyrowave"]);
        validate_capabilities(&with_removed).unwrap();
        assert!(require_stream_selection(&with_removed, "mjpeg", None).is_ok());

        let mut missing_codec = good.clone();
        missing_codec["codecs"] = json!(["raw"]);
        let error = validate_capabilities(&missing_codec).unwrap_err();
        assert!(error.contains("codecs") && error.contains("pyrowave"));

        // 只有 Raw/MJPEG、没有 PyroWave 的旧能力也不满足契约。
        let mut legacy = good.clone();
        legacy["codecs"] = json!(["raw", "mjpeg"]);
        assert!(
            validate_capabilities(&legacy)
                .unwrap_err()
                .contains("pyrowave")
        );

        let mut wrong_queue = good.clone();
        wrong_queue["sendqueue"] = json!("packet");
        assert!(
            validate_capabilities(&wrong_queue)
                .unwrap_err()
                .contains("sendqueue")
        );

        let mut missing_source = good.clone();
        missing_source["sources"] = json!(["pattern"]);
        assert!(
            require_stream_selection(&missing_source, "raw", Some("wgc"))
                .unwrap_err()
                .contains("wgc")
        );
        assert!(
            require_stream_selection(&good, "h264", None)
                .unwrap_err()
                .contains("h264")
        );
    }

    #[test]
    fn ready_status_distinguishes_finite_success_from_failure() {
        let finished_ok = json!({
            "ready": true,
            "running": false,
            "exit_code": 0,
            "session_id": "s-1",
            "adapter": "a",
            "batch": 69,
            "pid": 42
        });
        assert!(matches!(ready_outcome(&finished_ok), ReadyOutcome::Ready));
        validate_ready_echo(&finished_ok, "s-1", "a", BatchEcho::Expect(Some(69))).unwrap();

        let failed = json!({
            "ready": false,
            "running": false,
            "exit_code": 1,
            "error": "boom"
        });
        assert!(matches!(ready_outcome(&failed), ReadyOutcome::Failed(_)));

        let ended_without_ready = json!({"ready": false, "running": false, "exit_code": 0});
        assert!(matches!(
            ready_outcome(&ended_without_ready),
            ReadyOutcome::Failed(_)
        ));

        let starting = json!({"ready": false, "running": true, "exit_code": null});
        assert!(matches!(ready_outcome(&starting), ReadyOutcome::Pending));
    }

    #[test]
    fn raw_ready_requires_exact_batch_and_encoded_requires_null() {
        let raw = json!({
            "session_id": "s-1", "adapter": "a", "batch": 69
        });
        validate_ready_echo(&raw, "s-1", "a", BatchEcho::Expect(Some(69))).unwrap();
        assert!(validate_ready_echo(&raw, "s-1", "a", BatchEcho::Expect(Some(68))).is_err());
        assert!(validate_ready_echo(&raw, "s-2", "a", BatchEcho::Expect(Some(69))).is_err());

        let encoded = json!({
            "session_id": "s-1", "adapter": "a", "batch": null
        });
        validate_ready_echo(&encoded, "s-1", "a", BatchEcho::Expect(None)).unwrap();
        assert!(validate_ready_echo(&raw, "s-1", "a", BatchEcho::Expect(None)).is_err());

        let no_batch = json!({"session_id": "s-1", "adapter": "a"});
        assert!(validate_ready_echo(&no_batch, "s-1", "a", BatchEcho::Expect(Some(69))).is_err());
    }

    #[test]
    fn running_fallback_treats_null_exit_code_as_live() {
        // ABI 运行中输出 exit_code:null；缺少 running 时不能把它当成已结束。
        assert!(status_running(&json!({"exit_code": null})));
        assert!(status_running(&json!({})));
        assert!(!status_running(&json!({"exit_code": 0})));
        assert!(!status_running(&json!({"exit_code": 1})));
        assert!(!status_running(
            &json!({"exit_code": null, "error": "boom"})
        ));
        // 显式 running 始终优先。
        assert!(status_running(&json!({"running": true, "exit_code": 1})));
        assert!(!status_running(
            &json!({"running": false, "exit_code": null})
        ));
    }

    #[test]
    fn failed_start_stats_surface_error_without_fabricated_exit_code() {
        let mut slot = NativeSlot::new(Kind::Sender);
        slot.session_id = Some("s-failed".into());
        slot.adapter = "adapter-a".into();
        slot.last_error = Some("数据面会话在 10 秒内未确认就绪".into());

        let stats = slot.stats();
        assert_eq!(stats["running"], json!(false));
        assert_eq!(stats["session_id"], json!("s-failed"));
        assert_eq!(stats["error"], json!("数据面会话在 10 秒内未确认就绪"));
        assert!(
            stats.get("exit_code").is_none(),
            "从未结束的失败启动不应伪造 exit_code"
        );
    }

    #[test]
    fn terminal_status_keeps_worker_error_and_exit_code() {
        let mut slot = NativeSlot::new(Kind::Sender);
        slot.session_id = Some("s-failed".into());
        let terminal = json!({
            "ready": false,
            "running": false,
            "exit_code": 1,
            "error": "sender cancelled during initialization"
        });
        slot.record_status(terminal.as_object().unwrap());

        let stats = slot.stats();
        assert_eq!(stats["running"], json!(false));
        assert_eq!(stats["exit_code"], json!(1));
        assert_eq!(
            stats["error"],
            json!("sender cancelled during initialization")
        );
    }

    #[test]
    fn pyrowave_ready_timeout_covers_multistep_warmup_inside_rest_budget() {
        // Raw 就绪仍是单步操作，保持 10 秒。
        assert_eq!(
            ready_timeout(&json!({"codec": {"codec": "raw"}})),
            READY_TIMEOUT
        );
        assert_eq!(ready_timeout(&json!({})), READY_TIMEOUT);
        // PyroWave 接收端就绪前最多 5 个各受 5 秒看门狗约束的操作：25 秒上界。
        assert_eq!(
            ready_timeout(&json!({"codec": {"codec": "pyrowave"}})),
            PYROWAVE_READY_TIMEOUT
        );
        assert!(PYROWAVE_READY_TIMEOUT >= Duration::from_secs(25));
        // 仍须小于 REST start 的 40 秒总预算（另需留出停止重试余量）。
        assert!(PYROWAVE_READY_TIMEOUT + STOP_GRACE < Duration::from_secs(40));
    }

    #[test]
    fn live_status_surfaces_control_plane_error_without_overwriting_dll_error() {
        let object = |value: Value| value.as_object().unwrap().clone();

        // 会话仍在（running=true, exit_code=null）但停止未确认：错误必须可见。
        let mut live = object(json!({"running": true, "exit_code": null}));
        attach_error(&mut live, None, Some("停止失败且句柄已保留"));
        assert_eq!(live.get("running").and_then(Value::as_bool), Some(true));
        assert_eq!(
            live.get("error").and_then(Value::as_str),
            Some("停止失败且句柄已保留")
        );

        // DLL 自己的错误优先于控制面记录。
        let mut from_dll = object(json!({"running": false, "error": "worker error"}));
        attach_error(&mut from_dll, None, Some("control error"));
        assert_eq!(
            from_dll.get("error").and_then(Value::as_str),
            Some("worker error")
        );

        // 状态查询失败时报告查询错误。
        let mut queried = object(json!({"running": true}));
        attach_error(
            &mut queried,
            Some("query failed".into()),
            Some("control error"),
        );
        assert_eq!(
            queried.get("error").and_then(Value::as_str),
            Some("query failed")
        );

        // 无错误时不注入空字段。
        let mut healthy = object(json!({"running": true}));
        attach_error(&mut healthy, None, None);
        assert!(healthy.get("error").is_none());
    }

    #[test]
    fn retry_completed_cleanup_clears_stale_pending_without_session_and_nonblocking_stop_releases_empty_slot()
     {
        let mut slot = NativeSlot::new(Kind::Sender);
        slot.cleanup_pending = true;
        assert!(!slot.running());
        assert!(!slot.cleanup_pending());

        let mut empty = NativeSlot::new(Kind::Receiver);
        empty.cleanup_pending = true;
        let started = Instant::now();
        assert_eq!(
            empty
                .stop_inner(Duration::ZERO, false, "已回收此前结束的数据面会话")
                .unwrap(),
            StopOutcome::Stopped
        );
        assert!(!empty.cleanup_pending());
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn poll_status_matches_running_and_stats_for_idle_and_terminal_slots() {
        let mut slot = NativeSlot::new(Kind::Receiver);
        assert!(slot.is_idle());
        let (running, stats) = slot.poll_status();
        assert!(!running);
        assert_eq!(stats["running"], json!(false));

        slot.session_id = Some("r-term".into());
        slot.record_status(
            json!({"running": false, "exit_code": 0, "complete": 42})
                .as_object()
                .unwrap(),
        );
        let (running, stats) = slot.poll_status();
        assert!(!running);
        assert_eq!(stats["complete"], json!(42));
        assert_eq!(stats["exit_code"], json!(0));

        slot.cleanup_pending = true;
        assert!(!slot.is_idle());
        let (running, stats) = slot.poll_status();
        assert!(!running);
        assert!(!slot.cleanup_pending());
        assert_eq!(stats["running"], json!(false));
    }
}
