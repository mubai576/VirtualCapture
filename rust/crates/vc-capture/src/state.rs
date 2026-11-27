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
        let active = self.slot.running() || self.slot.cleanup_pending();
        if !active {
            self.active_request = None;
        }
        active
    }
    /// 对外列表只含已连接的物理网卡；诊断模式额外列出回环。
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
    /// 网卡参数（如巨帧/MTU）变化后清除枚举缓存，避免 `/adapters` 返回过期 `NlMtu`。
    pub fn invalidate_adapters_cache(&mut self) {
        self.adapters_cache = None;
    }
    pub fn stats(&mut self) -> Value {
        let (running, stats) = self.slot.poll_status();
        if !running {
            self.active_request = None;
        }
        stats
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
        // 回环自动路由必须先获显式诊断许可，不依赖当前物理网卡数量。
        if request.auto_adapter
            && !self.loopback_diagnostics
            && is_loopback_controller(controller_ip)
        {
            return Err(
                "本机回环采集只用于显式诊断，后台需以 --allow-loopback-diagnostics 启动。".into(),
            );
        }
        // 先做网卡门禁再探测能力：虚拟、未连接与未知设备不能进入启动路径。
        let items = self.adapters(true)?;
        if items.is_empty() {
            return Err("被采集机未发现已连接的物理网卡；请检查 Npcap 与网卡连接。".into());
        }
        let adapter = if request.auto_adapter {
            vc_core::adapters::choose_auto_adapter(&items, controller_ip)
                .map_err(|error| format!("{error} 采集端只接受已连接的物理网卡。"))?
                .clone()
        } else {
            vc_core::adapters::resolve(&items, &request.adapter)?
                .ok_or("请指定 adapter 或 auto_adapter。")?
                .clone()
        };
        if !adapter_permitted(&adapter, self.loopback_diagnostics, controller_ip) {
            return Err("回环网卡只允许在显式本地诊断时使用。".into());
        }
        // 先确定实际发送网卡，再核对请求携带的生效 MTU；自动 MTU 已由主机
        // 解析成显式值，采集端只读取 NlMtu 复核，绝不修改网卡设置。
        // Npcap 回环没有 NIC MTU，只在显式本地诊断中跳过。
        vc_core::budget::verify_mtu_for_adapter(
            request.capture.mtu,
            Some(&adapter),
            "被采集机发送网卡",
        )?;
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

/// 采集端可见网卡：系统确认且已连接的物理设备；只有显式诊断开关才额外列出 Npcap 回环。
/// 虚拟网卡、未连接、元数据未知设备与回环默认都不进入列表。
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
            status: Some("Up".into()),
            hardware_interface: hardware,
            virtual_nic,
            ..Default::default()
        }
    }

    #[test]
    fn capture_list_excludes_virtual_unknown_disconnected_and_loopback_by_default() {
        let mut down = nic(r"\Device\NPF_{D}", Some(true), Some(false));
        down.status = Some("Disconnected".into());
        let raw = vec![
            nic(r"\Device\NPF_{P}", Some(true), Some(false)),
            nic(r"\Device\NPF_{V}", Some(true), Some(true)),
            nic(r"\Device\NPF_{U}", None, None),
            down,
            nic(r"\Device\NPF_Loopback", None, None),
        ];
        let visible = selectable_adapters(&raw, false);
        let names: Vec<&str> = visible.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec![r"\Device\NPF_{P}"]);
        // 隐藏设备不能经手动选择重新启用。
        for name in [
            r"\Device\NPF_{V}",
            r"\Device\NPF_{U}",
            r"\Device\NPF_{D}",
            r"\Device\NPF_Loopback",
        ] {
            assert!(vc_core::adapters::resolve(&visible, name).is_err());
        }
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
    fn invalidate_adapters_cache_clears_cached_entries() {
        let mut agent = Agent::new(Config::default());
        agent.adapters_cache = Some((Instant::now(), vec![]));
        agent.invalidate_adapters_cache();
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
            codec: "pyrowave".into(),
            encoder_backend: "pyrowave:vulkan".into(),
            decoder_backend: "pyrowave:vulkan".into(),
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

    #[test]
    fn mjpeg_start_is_rejected_as_cpu_codec() {
        let mut agent = Agent::new(Config::default());
        let capture = vc_core::config::Capture {
            codec: "mjpeg".into(),
            codec_experimental: true,
            encoder_backend: "turbojpeg:cpu".into(),
            decoder_backend: "turbojpeg:cpu".into(),
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
        assert!(
            error.contains("mjpeg") && error.contains("GPU-only"),
            "{error}"
        );
        assert!(agent.slot.session_id.is_none());
    }
}
