use serde_json::{Value, json};
use std::time::{Duration, Instant};
use vc_core::{
    Config,
    adapters::AdapterInfo,
    process::{self, Kind, NativeSlot},
    session::{PreparedSession, SenderRequest},
};

pub struct Agent {
    pub config: Config,
    pub slot: NativeSlot,
    pub active_request: Option<SenderRequest>,
    pub shutting_down: bool,
    adapters_cache: Option<(Instant, Vec<AdapterInfo>)>,
    /// 只有显式 `--allow-loopback-diagnostics` 才允许本机回环采集；默认关闭。
    loopback_diagnostics: bool,
}
impl Agent {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            slot: NativeSlot::new(Kind::Sender),
            active_request: None,
            shutting_down: false,
            adapters_cache: None,
            loopback_diagnostics: false,
        }
    }
    /// 启动参数设置：仅显式诊断构建/本地回环测试可打开。
    pub fn with_loopback_diagnostics(mut self, allow: bool) -> Self {
        self.set_loopback_diagnostics(allow);
        self
    }
    /// 运行期切换诊断开关（清缓存，避免旧列表残留）。
    pub fn set_loopback_diagnostics(&mut self, allow: bool) {
        if self.loopback_diagnostics != allow {
            self.loopback_diagnostics = allow;
            self.adapters_cache = None;
        }
    }
    pub fn running(&mut self) -> bool {
        self.slot.running() || self.slot.cleanup_pending()
    }
    /// 对外列表只含可确认的物理网卡；诊断模式额外列出回环。
    pub fn adapters(&mut self, refresh: bool) -> Result<Vec<AdapterInfo>, String> {
        if !refresh
            && let Some((time, items)) = &self.adapters_cache
            && time.elapsed() < Duration::from_secs(10)
        {
            return Ok(items.clone());
        }
        let module = self.slot.data_plane_module()?;
        let all = vc_core::adapters::enumerate(&module)?;
        let items = selectable_adapters(&all, self.loopback_diagnostics);
        self.adapters_cache = Some((Instant::now(), items.clone()));
        Ok(items)
    }
    pub fn stats(&mut self) -> Value {
        self.slot.stats()
    }
    pub fn prepare(
        &mut self,
        request: &SenderRequest,
        controller_ip: &str,
    ) -> Result<PreparedSession, String> {
        if self.shutting_down {
            return Err("后台正在退出。".into());
        }
        request.validate_identity()?;
        let mut config = self.config.clone();
        config.capture = request.capture.clone();
        config.receive.mtu = request.capture.mtu;
        process::validate_start_config(&config)?;
        // 先做网卡门禁再探测能力：虚拟/未知/未授权的回环不能进入启动路径。
        let items = self.adapters(true)?;
        if items.is_empty() {
            return Err("被采集机未发现可确认的物理网卡；请检查 Npcap 与网卡连接。".into());
        }
        let adapter = if request.auto_adapter {
            match vc_core::adapters::choose_local_adapter(&items, controller_ip) {
                Ok(item) => item.clone(),
                Err(error)
                    if !self.loopback_diagnostics && is_loopback_controller(controller_ip) =>
                {
                    return Err(format!(
                        "{error} 本机回环采集只用于显式诊断，后台需以 --allow-loopback-diagnostics 启动。"
                    ));
                }
                Err(error) => return Err(format!("{error} 采集端只接受已确认的物理网卡。")),
            }
        } else {
            vc_core::adapters::resolve(&items, &request.adapter)?
                .ok_or("请指定 adapter 或 auto_adapter。")?
                .clone()
        };
        if !adapter_permitted(&adapter, self.loopback_diagnostics, controller_ip) {
            return Err("回环网卡只允许在显式本地诊断时使用。".into());
        }
        let module = self.slot.data_plane_module()?;
        let caps = process::require_capabilities(&module)?;
        process::require_stream_selection(
            &caps,
            &config.capture.codec,
            Some(&config.capture.mode),
        )?;
        let plan = vc_core::budget::plan(&config, None, Some(&adapter));
        if !plan.can_start {
            return Err(format!("发送网卡预算拒绝启动：{:?}", plan.network.status));
        }
        Ok(PreparedSession {
            capture: request.capture.clone(),
            batch_packets: process::expected_batch_packets(&request.capture, &adapter.name)?,
            session_id: request.session_id.clone(),
            source_mac: request.source_mac.clone(),
            adapter,
        })
    }
    pub fn start(&mut self, request: SenderRequest, controller_ip: &str) -> Result<Value, String> {
        if self.running() {
            return Err("已有采集会话运行中；先按 session_id 停止，不覆盖旧会话。".into());
        }
        let prepared = self.prepare(&request, controller_ip)?;
        let mut config = self.config.clone();
        config.capture = request.capture.clone();
        config.receive.mtu = config.capture.mtu;
        let run_dir = vc_core::paths::run_dir();
        let (stats, hashlog, _) =
            process::session_paths(&run_dir, &request.session_id, Kind::Sender);
        let request_json = process::sender_request(
            &config,
            &prepared.adapter.name,
            &request.session_id,
            &stats,
            &hashlog,
            &request.source_mac,
        )?;
        self.slot.start(
            &request_json,
            &prepared.adapter.name,
            &request.session_id,
            &run_dir,
            process::BatchEcho::Expect(prepared.batch_packets),
        )?;
        self.slot.mark_mode(
            Some(request.capture.mode.clone()),
            Some(request.capture.pattern.clone()),
        );
        self.active_request = Some(request);
        serde_json::to_value(prepared).map_err(|e| e.to_string())
    }
    pub fn stop(&mut self, session_id: Option<&str>) -> Result<Value, String> {
        if let Some(id) = session_id
            && self.running()
            && self.slot.session_id.as_deref() != Some(id)
        {
            return Err("session_id 与正在运行的采集会话不符，未停止其他会话。".into());
        }
        let session = self.slot.session_id.clone();
        match self.slot.stop() {
            Ok(process::StopOutcome::Stopped) => {
                self.active_request = None;
                Ok(json!({"stopped":true,"session_id":session}))
            }
            Ok(process::StopOutcome::Pending) => {
                Err("停止未确认：数据面会话句柄已保留，未释放；请重试停止。".into())
            }
            Err(error) => Err(format!("停止失败，数据面会话句柄已保留待重试：{error}")),
        }
    }
}

fn is_loopback_controller(controller_ip: &str) -> bool {
    controller_ip
        .trim()
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// 采集端可见网卡：系统确认的物理设备；只有显式诊断开关才额外列出 Npcap 回环。
/// 虚拟网卡、元数据未知设备与回环默认都不进入列表。
fn selectable_adapters(items: &[AdapterInfo], loopback_diagnostics: bool) -> Vec<AdapterInfo> {
    let mut selected = vc_core::adapters::physical_nics(items);
    if loopback_diagnostics {
        selected.extend(
            items
                .iter()
                .filter(|item| vc_core::adapters::is_loopback_device(item))
                .cloned(),
        );
    }
    selected
}

/// 回环只允许在显式诊断且由本机回环控制器发起时使用；物理网卡不受限。
fn adapter_permitted(
    adapter: &AdapterInfo,
    loopback_diagnostics: bool,
    controller_ip: &str,
) -> bool {
    !vc_core::adapters::is_loopback_device(adapter)
        || (loopback_diagnostics && is_loopback_controller(controller_ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(name: &str, hardware: Option<bool>, virtual_nic: Option<bool>) -> AdapterInfo {
        AdapterInfo {
            name: name.into(),
            description: "test nic".into(),
            hardware_interface: hardware,
            virtual_nic,
            ..Default::default()
        }
    }

    #[test]
    fn capture_list_excludes_virtual_unknown_and_loopback_by_default() {
        let raw = vec![
            nic(r"\Device\NPF_{P}", Some(true), Some(false)),
            nic(r"\Device\NPF_{V}", Some(true), Some(true)),
            nic(r"\Device\NPF_{U}", None, None),
            nic(r"\Device\NPF_Loopback", None, None),
        ];
        let visible = selectable_adapters(&raw, false);
        let names: Vec<&str> = visible.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec![r"\Device\NPF_{P}"]);
        // 隐藏的虚拟/未知网卡不能被手动选择器重新启用。
        assert!(vc_core::adapters::resolve(&visible, r"\Device\NPF_{V}").is_err());
        assert!(vc_core::adapters::resolve(&visible, r"\Device\NPF_{U}").is_err());
        assert!(vc_core::adapters::resolve(&visible, r"\Device\NPF_Loopback").is_err());
    }

    #[test]
    fn loopback_appears_only_with_explicit_diagnostics() {
        let raw = vec![
            nic(r"\Device\NPF_{P}", Some(true), Some(false)),
            nic(r"\Device\NPF_Loopback", None, None),
        ];
        let names: Vec<String> = selectable_adapters(&raw, true)
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(
            names,
            vec![
                r"\Device\NPF_{P}".to_string(),
                r"\Device\NPF_Loopback".to_string()
            ]
        );
    }

    #[test]
    fn loopback_requires_both_diagnostics_and_local_controller() {
        let loopback = nic(r"\Device\NPF_Loopback", None, None);
        let physical = nic(r"\Device\NPF_{P}", Some(true), Some(false));
        assert!(adapter_permitted(&physical, false, "192.168.2.100"));
        assert!(!adapter_permitted(&loopback, false, "127.0.0.1"));
        assert!(!adapter_permitted(&loopback, true, "192.168.2.100"));
        assert!(adapter_permitted(&loopback, true, "127.0.0.1"));
        assert!(adapter_permitted(&loopback, true, "::1"));
    }

    #[test]
    fn diagnostics_flag_toggles_and_clears_cache() {
        let mut agent = Agent::new(Config::default()).with_loopback_diagnostics(true);
        assert!(agent.loopback_diagnostics);
        agent.adapters_cache = Some((Instant::now(), vec![]));
        agent.set_loopback_diagnostics(false);
        assert!(!agent.loopback_diagnostics);
        assert!(agent.adapters_cache.is_none());
    }

    #[test]
    fn malformed_requests_do_not_replace_slot() {
        let mut agent = Agent::new(Config::default());
        let request = SenderRequest {
            capture: Default::default(),
            session_id: "../bad".into(),
            source_mac: "02:00:00:00:00:01".into(),
            auto_adapter: true,
            adapter: String::new(),
        };
        assert!(agent.start(request, "127.0.0.1").is_err());
        assert!(agent.slot.session_id.is_none());
    }
    #[test]
    fn encoded_start_requires_explicit_experimental_opt_in() {
        let mut agent = Agent::new(Config::default());
        let capture = vc_core::config::Capture {
            codec: "mjpeg".into(),
            ..Default::default()
        };
        let request = SenderRequest {
            capture,
            session_id: "s-test".into(),
            source_mac: "02:00:00:00:00:01".into(),
            auto_adapter: true,
            adapter: String::new(),
        };
        let error = agent.prepare(&request, "127.0.0.1").unwrap_err();
        assert!(error.contains("codec_experimental"), "{error}");
        assert!(agent.slot.session_id.is_none());
    }
}
