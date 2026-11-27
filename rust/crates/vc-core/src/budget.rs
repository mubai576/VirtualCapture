//! 链路预算：只看采集/接收尺寸、MTU 与两端网卡速率，不改配置、不启停进程。
//!
//! 公式与 `references/rust-control.md` 一致：`P = mtu - 42`，
//! `wire = B + N*66 + max(0, 18 - last)`，`usable = min(rx, tx) * (100 - headroom) / 100`。
//! 状态沿用历史 agent：`exceeds` 只在超过物理线速时成立，超过 `usable` 才是 `margin`；
//! 速率未知只告警，明确断开的物理网卡才是 `down`。

use serde::{Deserialize, Serialize};

use crate::adapters::AdapterInfo;
use crate::config::Config;
use crate::process::{raw_frame_batch_from_capture, raw_send_batch_for_adapter};

const NPF_LOOPBACK: &str = r"\Device\NPF_Loopback";
const MAX_PLAUSIBLE_LINK_BPS: u64 = 1_000_000_000_000_000;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    Up,
    Down,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Link {
    pub state: LinkState,
    pub bps: Option<u64>,
    pub name: String,
}

impl Link {
    pub fn unknown(name: impl Into<String>) -> Self {
        Self {
            state: LinkState::Unknown,
            bps: None,
            name: name.into(),
        }
    }

    /// 与历史 agent 的 `_stream_link` 一致：
    /// - 明确 Down/Disconnected/Disabled/Not Present → down；
    /// - 回环/虚拟网卡、硬件标记为 false、速率缺失/0/超出合理范围 → unknown；
    /// - 其余有正速率的物理网卡 → up。缺失元数据不能冒充物理容量。
    pub fn from_adapter(adapter: Option<&AdapterInfo>, direction: Direction) -> Self {
        let Some(adapter) = adapter else {
            return Self::unknown("");
        };
        let name = adapter
            .label
            .clone()
            .filter(|label| !label.is_empty())
            .or_else(|| (!adapter.description.is_empty()).then(|| adapter.description.clone()))
            .unwrap_or_else(|| adapter.name.clone());
        let status = adapter.status.as_deref().unwrap_or("").to_ascii_lowercase();
        if matches!(
            status.as_str(),
            "down" | "disconnected" | "disabled" | "not present" | "lowerlayerdown"
        ) {
            return Self {
                state: LinkState::Down,
                bps: None,
                name,
            };
        }
        let bps = match direction {
            Direction::Receive => adapter.receive_link_bps,
            Direction::Transmit => adapter.transmit_link_bps,
        };
        let virtual_or_incomplete = adapter.name == NPF_LOOPBACK
            || adapter.virtual_nic == Some(true)
            || adapter.hardware_interface != Some(true);
        match bps {
            Some(bps) if !virtual_or_incomplete && (1..=MAX_PLAUSIBLE_LINK_BPS).contains(&bps) => {
                Self {
                    state: LinkState::Up,
                    bps: Some(bps),
                    name,
                }
            }
            _ => Self::unknown(name),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Receive,
    Transmit,
}

/// 自动 MTU 所需的网卡 NlMtu；缺失、低于 1500 或回环都明确报错。
fn reported_mtu(adapter: Option<&AdapterInfo>, role: &str) -> Result<u32, String> {
    let Some(adapter) = adapter else {
        return Err(format!(
            "{role}尚未确定，无法读取 IPv4 NlMtu；自动 MTU 需要先选定物理网卡。"
        ));
    };
    if crate::adapters::is_loopback_device(adapter) {
        return Err(format!(
            "{role}是本机 Npcap 回环，没有 NIC MTU；自动 MTU 只用于物理网卡，请显式指定 MTU。"
        ));
    }
    match adapter.mtu {
        Some(mtu) if mtu >= 1500 => Ok(mtu.min(vc_transport::MAX_MTU as u32)),
        Some(mtu) => Err(format!(
            "{role}（{}）报告 IPv4 NlMtu={mtu}，低于冻结传输最小值 1500；请确认网卡配置，显式 MTU 也不能超过报告值（程序不会修改网卡）。",
            adapter.display()
        )),
        None => Err(format!(
            "{role}（{}）未报告 IPv4 NlMtu，无法自动解析或校验显式 MTU；请确认网卡已启用 IPv4。",
            adapter.display()
        )),
    }
}

/// 自动 MTU：`min(主机接收网卡, 被采集机发送网卡, 9000)`。
///
/// 两端都必须报告有效的 IPv4 NlMtu；只知道一端时无法保证另一端能重组大包，
/// 因此明确报错，不猜 1500 或 9000。Npcap 回环没有 NIC MTU，不能参与自动解析。
pub fn resolve_auto_mtu(
    host: Option<&AdapterInfo>,
    capture: Option<&AdapterInfo>,
) -> Result<u32, String> {
    let host_mtu = reported_mtu(host, "主机接收网卡")?;
    let capture_mtu = reported_mtu(capture, "被采集机发送网卡")?;
    Ok(host_mtu.min(capture_mtu).min(vc_transport::MAX_MTU as u32))
}

/// 显式 MTU 必须不超过网卡报告的 IPv4 NlMtu。
///
/// 只读校验，绝不调用 Set cmdlet 或修改网卡。Npcap 回环没有 NIC MTU，
/// 只在显式本地诊断中跳过检查；物理网卡缺报告同样拒绝，不能假设可发送。
pub fn verify_mtu_for_adapter(
    mtu: u32,
    adapter: Option<&AdapterInfo>,
    role: &str,
) -> Result<(), String> {
    let Some(adapter) = adapter else {
        return Err(format!(
            "{role}尚未确定，无法确认显式 MTU {mtu} 不超过网卡报告值。"
        ));
    };
    if crate::adapters::is_loopback_device(adapter) {
        return Ok(());
    }
    if !(1500..=vc_transport::MAX_MTU as u32).contains(&mtu) {
        return Err(format!(
            "显式 MTU {mtu} 不在 1500..9000，无法确认{role}可用。"
        ));
    }
    match adapter.mtu {
        Some(reported) if mtu <= reported => Ok(()),
        Some(reported) => Err(format!(
            "显式 MTU {mtu} 超过{role}（{}）报告的 IPv4 NlMtu={reported}；请降低 MTU 或调整网卡设置（程序不会修改网卡）。",
            adapter.display()
        )),
        None => Err(format!(
            "{role}（{}）未报告 IPv4 NlMtu，无法确认显式 MTU {mtu} 可用；请确认网卡已启用 IPv4。",
            adapter.display()
        )),
    }
}

/// 产生本次预算/准备/启动共用的有效配置。
///
/// - `mtu_auto=true`：解析两端所选网卡的报告值并冻结为显式 MTU；
/// - `mtu_auto=false`：校验显式 MTU 不超过已知网卡报告值（对端尚未确定时由
///   采集端在 prepare 时按实际网卡复核）；
/// - 无论哪种模式，都把 `receive.mtu` 镜像为同一个生效值，避免两端用不同
///   切包参数启动。返回的是副本，保存的配置与 `mtu_auto` 标志不变。
pub fn host_effective_config(
    config: &Config,
    host: Option<&AdapterInfo>,
    capture: Option<&AdapterInfo>,
) -> Result<Config, String> {
    let mut effective = config.clone();
    if effective.capture.mtu_auto {
        effective.capture.mtu = resolve_auto_mtu(host, capture)?;
        // 生效请求必须携带冻结值；采集端把它当作有效 MTU 复核，不再自行解析。
        effective.capture.mtu_auto = false;
    } else {
        verify_mtu_for_adapter(effective.capture.mtu, host, "主机接收网卡")?;
        if let Some(capture) = capture {
            verify_mtu_for_adapter(effective.capture.mtu, Some(capture), "被采集机发送网卡")?;
        }
    }
    effective.receive.mtu = effective.capture.mtu;
    Ok(effective)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Fits,
    Margin,
    Exceeds,
    Down,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoBudget {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: String,
    pub transform: String,
    pub raw_frame_bytes: u64,
    pub frame_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkBudget {
    pub status: PlanStatus,
    pub is_estimate: bool,
    pub host: Link,
    pub capture: Link,
    pub payload_bytes: u64,
    pub packets_per_frame: u64,
    /// Raw 固定几何时一帧一批（Npcap 回环为 1）；编码流在每帧编码完成后按实际包数确定。
    pub batch_packets: Option<u32>,
    pub wire_bps: u64,
    /// 线速需求（含预留余量），仅用于展示；状态判定用 `wire_bps` 对物理线速/usable。
    pub required_bps: u64,
    /// 两端都已知才是 `min(rx, tx)`；一端未知时为 `None`，不能据此宣称可行。
    pub link_bps: Option<u64>,
    /// 已知端中的较慢者；一端未知时仍可用它证明 `exceeds`。
    pub known_upper_bound_bps: Option<u64>,
    pub usable_bps: Option<u64>,
    pub headroom_pct: u32,
    pub max_raw_fps: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub video: VideoBudget,
    pub network: NetworkBudget,
    pub can_start: bool,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// 主机侧预算。`host_adapter` 是本机接收网卡，`capture_adapter` 是被采集机发送网卡。
pub fn plan(
    config: &Config,
    host_adapter: Option<&AdapterInfo>,
    capture_adapter: Option<&AdapterInfo>,
) -> Plan {
    let c = &config.capture;
    let mut errors = config.validate_start();
    let mut warnings = Vec::new();

    // 校验错误已记录在 errors；这里的 max(1) 只防止畸形配置让预算数学除零/溢出。
    let fps = u64::from(c.fps).max(1);
    let payload = u64::from(c.mtu.saturating_sub(42)).max(1);
    let raw_frame_bytes = u64::from(c.width)
        .saturating_mul(u64::from(c.height))
        .saturating_mul(3)
        / 2;
    let encoded = c.codec != "raw";
    let frame_bytes = if encoded {
        48u64.saturating_add(
            u64::from(c.max_bitrate_mbps)
                .saturating_mul(1_000_000)
                .saturating_add(8 * fps - 1)
                / (8 * fps),
        )
    } else {
        raw_frame_bytes
    };
    let packets = frame_bytes.div_ceil(payload);
    let last = frame_bytes.saturating_sub(packets.saturating_sub(1).saturating_mul(payload));
    let wire_frame_bytes = frame_bytes
        .saturating_add(packets.saturating_mul(66))
        .saturating_add(18u64.saturating_sub(last));
    let wire_bps = wire_frame_bytes.saturating_mul(8).saturating_mul(fps);
    let headroom = config.transport.headroom_pct.min(100);
    let required_bps = wire_bps.saturating_mul(100 + u64::from(headroom)) / 100;

    let host = Link::from_adapter(host_adapter, Direction::Receive);
    let capture = Link::from_adapter(capture_adapter, Direction::Transmit);
    let link_bps = match (host.bps, capture.bps) {
        (Some(host), Some(capture)) => Some(host.min(capture)),
        _ => None,
    };
    let known_upper_bound_bps = [host.bps, capture.bps].into_iter().flatten().min();
    let usable_bps = link_bps.map(|bps| bps.saturating_mul(100 - u64::from(headroom)) / 100);

    let status =
        if matches!(host.state, LinkState::Down) || matches!(capture.state, LinkState::Down) {
            PlanStatus::Down
        } else if known_upper_bound_bps.is_some_and(|bound| wire_bps > bound) {
            // 一端未知时，另一端的已知速率仍足以证明超限。
            PlanStatus::Exceeds
        } else if link_bps.is_none() {
            PlanStatus::Unknown
        } else if wire_bps > usable_bps.unwrap_or(0) {
            PlanStatus::Margin
        } else {
            PlanStatus::Fits
        };

    if status == PlanStatus::Unknown {
        if host.state == LinkState::Unknown {
            warnings.push("主机接收网卡速率未知，不能断言预算可容纳。".into());
        }
        if capture.state == LinkState::Unknown {
            warnings.push("被采集机发送网卡速率未知，不能断言预算可容纳。".into());
        }
    }
    if status == PlanStatus::Margin {
        warnings.push("未超过物理线速，但侵入预留余量；建议降低分辨率/帧率或使用编码模式。".into());
    }
    if packets > u64::from(u16::MAX) {
        errors.push("单帧包数超过冻结包头 uint16 上限，接收端无法重组。".into());
    }
    if encoded {
        // 编码链路必须显式启用并由 validate_start 校验后端；这里只保留估算边界。
        warnings.push("编码链路为估算：最大码率均摊不保证单帧突发、编码吞吐或画质。".into());
    }
    if c.mode == "dda" {
        warnings.push("DDA 只发送新桌面图像并受 fps 上限约束，静态桌面可能低于目标帧率。".into());
    }
    if c.mode == "wgc" {
        warnings.push("WGC 仍经 DWM 合成；窗口模式包含光标，独占全屏无法采集。".into());
    }

    let can_start = errors.is_empty() && !matches!(status, PlanStatus::Exceeds | PlanStatus::Down);

    let max_raw_fps = if encoded {
        None
    } else {
        usable_bps
            .filter(|usable| *usable > 0)
            .map(|usable| usable / (wire_frame_bytes.saturating_mul(8)).max(1))
    };
    // 必须按 Npcap 设备名（AdapterInfo.name）而不是 Link 显示名判断回环：
    // 真实回环没有 GUID 元数据，label 为空时显示名来自 description，
    // 与启动时 `expected_batch_packets` 的判定不一致。畸形配置已在 errors 中；
    // 这里退回饱和估算，保证纯展示的预算计算不 panic。
    let batch_packets = if encoded {
        None
    } else {
        let batch = match capture_adapter {
            Some(adapter) => raw_send_batch_for_adapter(c, &adapter.name),
            None => raw_frame_batch_from_capture(c),
        };
        Some(batch.unwrap_or_else(|_| u32::try_from(packets).unwrap_or(u32::MAX)))
    };

    Plan {
        video: VideoBudget {
            width: c.width,
            height: c.height,
            fps: c.fps,
            codec: c.codec.clone(),
            transform: c.transform.clone(),
            raw_frame_bytes,
            frame_bytes,
        },
        network: NetworkBudget {
            status,
            is_estimate: encoded,
            host,
            capture,
            payload_bytes: payload,
            packets_per_frame: packets,
            batch_packets,
            wire_bps,
            required_bps,
            link_bps,
            known_upper_bound_bps,
            usable_bps,
            headroom_pct: config.transport.headroom_pct,
            max_raw_fps,
        },
        can_start,
        warnings,
        errors,
    }
}

pub fn human_bps(bps: u64) -> String {
    let bits = bps as f64;
    if bits >= 1e9 {
        format!("{:.2} Gbps", bits / 1e9)
    } else if bits >= 1e6 {
        format!("{:.0} Mbps", bits / 1e6)
    } else if bits >= 1e3 {
        format!("{:.0} kbps", bits / 1e3)
    } else {
        format!("{bps} bps")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AdapterInfo;

    fn adapter(rx: u64, tx: u64) -> AdapterInfo {
        AdapterInfo {
            name: r"\Device\NPF_{X}".into(),
            description: "test".into(),
            label: Some("test".into()),
            mac: None,
            status: Some("Up".into()),
            receive_link_bps: Some(rx),
            transmit_link_bps: Some(tx),
            hardware_interface: Some(true),
            virtual_nic: Some(false),
            mtu: Some(9000),
            addresses: vec![],
        }
    }

    fn both(bps: u64) -> (AdapterInfo, AdapterInfo) {
        (adapter(bps, bps), adapter(bps, bps))
    }

    #[test]
    fn default_640_is_about_1_19_gbps() {
        let config = Config::default();
        let (host, capture) = both(2_500_000_000);
        let plan = plan(&config, Some(&host), Some(&capture));
        assert_eq!(plan.network.packets_per_frame, 69);
        assert_eq!(plan.network.batch_packets, Some(69));
        assert_eq!(plan.network.wire_bps, 1_188_391_680);
        assert_eq!(plan.video.raw_frame_bytes, 614_400);
        assert_eq!(plan.network.status, PlanStatus::Fits);
        assert!(plan.can_start, "{:?}", plan.errors);
        assert_eq!(plan.network.max_raw_fps, Some(403));
    }

    #[test]
    fn gigabit_link_exceeds_240fps() {
        let config = Config::default();
        let (host, capture) = both(1_000_000_000);
        let plan = plan(&config, Some(&host), Some(&capture));
        assert_eq!(plan.network.status, PlanStatus::Exceeds);
        assert!(!plan.can_start);
    }

    #[test]
    fn margin_is_above_usable_but_not_above_physical() {
        let config = Config::default();
        // 1.4G -> usable 1.12G; wire 1.188G 侵入余量但未超物理线速。
        let (host, capture) = both(1_400_000_000);
        let plan = plan(&config, Some(&host), Some(&capture));
        assert_eq!(plan.network.usable_bps, Some(1_120_000_000));
        assert_eq!(plan.network.status, PlanStatus::Margin);
        assert!(plan.can_start);
        assert!(plan.warnings.iter().any(|w| w.contains("预留余量")));
    }

    #[test]
    fn fits_when_below_usable_even_if_close() {
        let config = Config::default();
        // 1.5G -> usable 1.2G > wire 1.188G；历史语义是 fits，不是 margin。
        let (host, capture) = both(1_500_000_000);
        let plan = plan(&config, Some(&host), Some(&capture));
        assert_eq!(plan.network.status, PlanStatus::Fits);
    }

    #[test]
    fn missing_speed_only_warns() {
        let config = Config::default();
        let mut host = adapter(2_500_000_000, 2_500_000_000);
        host.receive_link_bps = None;
        let plan = plan(
            &config,
            Some(&host),
            Some(&adapter(2_500_000_000, 2_500_000_000)),
        );
        assert_eq!(plan.network.status, PlanStatus::Unknown);
        assert_eq!(plan.network.link_bps, None);
        assert_eq!(plan.network.known_upper_bound_bps, Some(2_500_000_000));
        assert!(plan.can_start);
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("主机接收网卡速率未知"))
        );
    }

    #[test]
    fn one_known_slow_link_can_prove_exceeds() {
        let config = Config::default();
        let mut host = adapter(2_500_000_000, 2_500_000_000);
        host.receive_link_bps = None;
        let plan = plan(
            &config,
            Some(&host),
            Some(&adapter(1_000_000_000, 1_000_000_000)),
        );
        assert_eq!(plan.network.status, PlanStatus::Exceeds);
        assert!(!plan.can_start);
    }

    #[test]
    fn virtual_loopback_and_missing_metadata_are_unknown_not_down() {
        let config = Config::default();

        let mut virtual_nic = adapter(2_500_000_000, 2_500_000_000);
        virtual_nic.virtual_nic = Some(true);
        let loopback = AdapterInfo {
            name: NPF_LOOPBACK.into(),
            description: "Adapter for loopback".into(),
            ..Default::default()
        };
        let missing = AdapterInfo::default();

        // 回归：真实 Npcap 回环没有 GUID 元数据，label 为空时显示名来自
        // description，不能用 Link.name 判定回环。预算的 batch 必须与启动时的
        // expected_batch_packets 一致，按 AdapterInfo.name 算成 1。
        let loopback_link = Link::from_adapter(Some(&loopback), Direction::Transmit);
        assert_ne!(loopback_link.name, NPF_LOOPBACK);
        let loopback_plan = plan(
            &config,
            Some(&adapter(2_500_000_000, 2_500_000_000)),
            Some(&loopback),
        );
        assert_eq!(loopback_plan.network.batch_packets, Some(1));

        for item in [virtual_nic, loopback.clone(), missing] {
            let link = Link::from_adapter(Some(&item), Direction::Receive);
            assert_eq!(link.state, LinkState::Unknown, "{item:?}");
            let plan = plan(
                &config,
                Some(&item),
                Some(&adapter(2_500_000_000, 2_500_000_000)),
            );
            assert_eq!(plan.network.status, PlanStatus::Unknown, "{item:?}");
        }
    }

    #[test]
    fn speed_without_hardware_metadata_is_unknown() {
        let mut item = adapter(2_500_000_000, 2_500_000_000);
        item.hardware_interface = None;
        let link = Link::from_adapter(Some(&item), Direction::Receive);
        assert_eq!(link.state, LinkState::Unknown);
        assert_eq!(link.bps, None);
    }

    #[test]
    fn explicitly_disconnected_physical_nic_is_down() {
        let config = Config::default();
        let mut host = adapter(0, 0);
        host.status = Some("Disconnected".into());
        let plan = plan(
            &config,
            Some(&host),
            Some(&adapter(2_500_000_000, 2_500_000_000)),
        );
        assert_eq!(plan.network.host.state, LinkState::Down);
        assert_eq!(plan.network.status, PlanStatus::Down);
        assert!(!plan.can_start);
    }

    #[test]
    fn validation_errors_are_errors_not_warnings() {
        let mut config = Config::default();
        config.capture.width = 641;
        let (host, capture) = both(2_500_000_000);
        let plan = plan(&config, Some(&host), Some(&capture));
        assert!(plan.errors.iter().any(|e| e.contains("width")));
        assert!(!plan.warnings.iter().any(|w| w.contains("width")));
        assert!(!plan.can_start);
    }

    #[test]
    fn encoded_draft_is_rejected_until_explicitly_enabled() {
        let mut config = Config::default();
        config.capture.codec = "pyrowave".into();
        config.capture.encoder_backend = "pyrowave:vulkan".into();
        config.capture.decoder_backend = "pyrowave:vulkan".into();
        let (host, capture) = both(2_500_000_000);
        let draft = plan(&config, Some(&host), Some(&capture));
        assert!(draft.network.is_estimate);
        assert!(
            draft
                .errors
                .iter()
                .any(|e| e.contains("codec_experimental"))
        );
        assert!(!draft.can_start);
        assert_eq!(draft.network.max_raw_fps, None);
        assert_eq!(draft.network.batch_packets, None);

        // 显式启用后预算允许启动，但仍标注为估算且不伪造固定 batch。
        config.capture.codec_experimental = true;
        let enabled = plan(&config, Some(&host), Some(&capture));
        assert!(enabled.errors.is_empty(), "{:?}", enabled.errors);
        assert!(enabled.can_start);
        assert!(enabled.network.is_estimate);
        assert_eq!(enabled.network.batch_packets, None);
        assert!(
            enabled
                .warnings
                .iter()
                .any(|w| w.contains("编码链路为估算"))
        );
    }

    #[test]
    fn mtu_mismatch_and_packet_overflow_are_errors() {
        let mut config = Config::default();
        config.receive.mtu = 1500;
        let (host, capture) = both(2_500_000_000);
        let first = plan(&config, Some(&host), Some(&capture));
        assert!(first.errors.iter().any(|e| e.contains("MTU")));

        let mut config = Config::default();
        config.capture.width = 8192;
        config.capture.height = 8192;
        config.capture.mtu = 1500;
        config.receive.mtu = 1500;
        config.capture.fps = 1;
        let overflow = plan(&config, Some(&host), Some(&capture));
        assert!(overflow.network.packets_per_frame > u64::from(u16::MAX));
        assert!(overflow.errors.iter().any(|e| e.contains("uint16")));
        assert!(!overflow.can_start);
    }

    #[test]
    fn auto_mtu_uses_min_of_both_nics_and_caps_at_9000() {
        let host = adapter(2_500_000_000, 2_500_000_000);
        let mut peer = adapter(2_500_000_000, 2_500_000_000);
        peer.mtu = Some(1500);
        assert_eq!(resolve_auto_mtu(Some(&host), Some(&peer)).unwrap(), 1500);
        peer.mtu = Some(4000);
        assert_eq!(resolve_auto_mtu(Some(&host), Some(&peer)).unwrap(), 4000);
        peer.mtu = Some(9216);
        assert_eq!(resolve_auto_mtu(Some(&host), Some(&peer)).unwrap(), 9000);

        peer.mtu = Some(1400);
        let error = resolve_auto_mtu(Some(&host), Some(&peer)).unwrap_err();
        assert!(error.contains("低于") && error.contains("1500"), "{error}");
        peer.mtu = None;
        let error = resolve_auto_mtu(Some(&host), Some(&peer)).unwrap_err();
        assert!(
            error.contains("未报告") && error.contains("NlMtu"),
            "{error}"
        );
        assert!(resolve_auto_mtu(None, Some(&peer)).is_err());
        assert!(resolve_auto_mtu(Some(&host), None).is_err());

        // Npcap 回环没有 NIC MTU，不能参与自动解析（只用于显式诊断）。
        let loopback = AdapterInfo {
            name: NPF_LOOPBACK.into(),
            description: "Adapter for loopback".into(),
            ..Default::default()
        };
        assert!(resolve_auto_mtu(Some(&host), Some(&loopback)).is_err());
    }

    #[test]
    fn explicit_mtu_must_fit_reported_nic_mtu() {
        let mut host = adapter(2_500_000_000, 2_500_000_000);
        assert!(verify_mtu_for_adapter(9000, Some(&host), "主机接收网卡").is_ok());
        host.mtu = Some(1500);
        let error = verify_mtu_for_adapter(9000, Some(&host), "主机接收网卡").unwrap_err();
        assert!(error.contains("超过") && error.contains("1500"), "{error}");
        assert!(verify_mtu_for_adapter(1500, Some(&host), "主机接收网卡").is_ok());
        host.mtu = None;
        let error = verify_mtu_for_adapter(9000, Some(&host), "主机接收网卡").unwrap_err();
        assert!(error.contains("未报告"), "{error}");
        assert!(verify_mtu_for_adapter(9000, None, "主机接收网卡").is_err());
        assert!(verify_mtu_for_adapter(1499, Some(&adapter(1, 1)), "网卡").is_err());

        // 回环只在显式本地诊断中使用，没有 NIC MTU 可核对。
        let loopback = AdapterInfo {
            name: NPF_LOOPBACK.into(),
            ..Default::default()
        };
        assert!(verify_mtu_for_adapter(9000, Some(&loopback), "诊断回环").is_ok());
    }

    #[test]
    fn effective_config_freezes_auto_mtu_and_mirrors_receiver() {
        let mut config = Config::default();
        config.capture.mtu = 9000;
        config.receive.mtu = 1500;
        let mut host = adapter(2_500_000_000, 2_500_000_000);
        host.mtu = Some(9000);
        let mut peer = adapter(2_500_000_000, 2_500_000_000);
        peer.mtu = Some(4000);

        let effective = host_effective_config(&config, Some(&host), Some(&peer)).unwrap();
        assert_eq!(effective.capture.mtu, 4000);
        assert!(!effective.capture.mtu_auto);
        assert_eq!(effective.receive.mtu, 4000);
        // 保存的配置不被改写，自动标志仍保留。
        assert!(config.capture.mtu_auto);
        assert_eq!(config.capture.mtu, 9000);
        assert_eq!(config.receive.mtu, 1500);

        // 自动 MTU 必须有对端网卡报告，否则明确失败。
        assert!(host_effective_config(&config, Some(&host), None).is_err());
        assert!(host_effective_config(&config, None, Some(&peer)).is_err());

        // 显式 MTU：已知的两端报告都必须容得下；对端尚未确定时留给采集端复核。
        let mut explicit = config.clone();
        explicit.capture.mtu_auto = false;
        explicit.capture.mtu = 9000;
        let mut peer_small = peer.clone();
        peer_small.mtu = Some(1500);
        assert!(host_effective_config(&explicit, Some(&host), Some(&peer_small)).is_err());
        let effective = host_effective_config(&explicit, Some(&host), None).unwrap();
        assert_eq!(effective.capture.mtu, 9000);
        assert_eq!(effective.receive.mtu, 9000);
        assert!(!effective.capture.mtu_auto);
    }

    #[test]
    fn malformed_config_does_not_divide_by_zero_or_overflow() {
        let mut config = Config::default();
        config.capture.fps = 0;
        config.transport.headroom_pct = u32::MAX;
        config.capture.width = u32::MAX;
        config.capture.height = u32::MAX;
        config.capture.mtu = 0;
        config.receive.mtu = 0;
        config.capture.max_bitrate_mbps = u32::MAX;
        config.capture.bitrate_mbps = u32::MAX;
        config.capture.codec = "mjpeg".into();
        let (host, capture) = both(2_500_000_000);
        let plan = plan(&config, Some(&host), Some(&capture));
        assert!(!plan.can_start);
        assert!(!plan.errors.is_empty());
        assert_eq!(plan.network.batch_packets, None);
        // 只要求不 panic；数值可能因饱和而很大。
        assert!(plan.network.packets_per_frame > 0);
    }
}
