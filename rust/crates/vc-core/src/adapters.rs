//! 网卡枚举：Npcap 设备列表（进程内 `vc_stream.dll` 查询）+ Windows 网卡元数据（PowerShell）。
//!
//! 不启动采集。速率单位为 bit/s；虚拟网卡、回环与未知速率不冒充物理容量。
//! PowerShell 子进程必须有超时与输出上限，避免卡死或输出失控拖住控制面。

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::dataplane::Module;

const METADATA_TIMEOUT: Duration = Duration::from_secs(15);
const OUTPUT_CAP: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct AdapterAddress {
    pub ip: String,
    pub prefix: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct AdapterInfo {
    pub name: String,
    pub description: String,
    pub label: Option<String>,
    pub mac: Option<String>,
    pub status: Option<String>,
    pub receive_link_bps: Option<u64>,
    pub transmit_link_bps: Option<u64>,
    pub hardware_interface: Option<bool>,
    #[serde(rename = "virtual")]
    pub virtual_nic: Option<bool>,
    /// Windows IPv4 接口的 NlMtu；元数据缺失或非 Windows 时为 None。
    /// 这是网卡当前生效的 L3 MTU，不做任何提升或回写。
    #[serde(default)]
    pub mtu: Option<u32>,
    pub addresses: Vec<AdapterAddress>,
}

impl AdapterInfo {
    pub fn display(&self) -> String {
        let label = self.label.clone().unwrap_or_default();
        if label.is_empty() {
            format!("{} ({})", self.description, self.name)
        } else {
            format!("{label} — {}", self.description)
        }
    }

    /// 可确认的物理网卡：系统明确标记为硬件接口、且明确不是虚拟网卡。
    /// 元数据缺失（PowerShell 探测失败）不能当作物理设备，Npcap 回环也永远排除。
    pub fn is_physical_nic(&self) -> bool {
        self.hardware_interface == Some(true)
            && self.virtual_nic == Some(false)
            && !is_loopback_device(self)
    }

    /// 产品候选必须是已连接的物理网卡；未知状态不能当作已连接。
    pub fn is_connected_physical_nic(&self) -> bool {
        self.is_physical_nic()
            && self
                .status
                .as_deref()
                .is_some_and(|status| status.trim().eq_ignore_ascii_case("up"))
    }

    /// 兼容旧名：只判断物理身份，不包含连接状态。
    pub fn is_physical_host_nic(&self) -> bool {
        self.is_physical_nic()
    }
}

/// Npcap 回环设备名，大小写无关。
pub fn is_loopback_device(item: &AdapterInfo) -> bool {
    item.name.eq_ignore_ascii_case(NPF_LOOPBACK)
}

const NPF_LOOPBACK: &str = r"\Device\NPF_Loopback";

/// 过滤出已连接的物理网卡，保持枚举顺序；不按厂商或型号优先。
/// 虚拟、回环、未连接与元数据未知设备都不进入产品列表。
pub fn physical_nics(items: &[AdapterInfo]) -> Vec<AdapterInfo> {
    items
        .iter()
        .filter(|item| item.is_connected_physical_nic())
        .cloned()
        .collect()
}

/// 兼容旧名：主机接收端使用的物理网卡列表。
pub fn physical_host_nics(items: &[AdapterInfo]) -> Vec<AdapterInfo> {
    physical_nics(items)
}

/// 通过已装载的数据面模块枚举 Npcap 设备，并与系统网卡元数据合并。
pub fn enumerate(module: &Module) -> Result<Vec<AdapterInfo>, String> {
    let mut items = from_listing(module.list_adapters()?);
    let metadata = windows_nic_metadata();
    for item in &mut items {
        if let Some(guid) = npf_guid(&item.name)
            && let Some(meta) = metadata.get(&guid)
        {
            item.label = meta.label.clone();
            item.mac = meta.mac.clone();
            item.status = meta.status.clone();
            item.receive_link_bps = meta.receive_link_bps;
            item.transmit_link_bps = meta.transmit_link_bps;
            item.hardware_interface = meta.hardware_interface;
            item.virtual_nic = meta.virtual_nic;
            item.mtu = meta.mtu;
            item.addresses = meta.addresses.clone();
        }
    }
    Ok(items)
}

/// 只从 `\Device\NPF_{GUID}` 提取 GUID；`\Device\NPF_Loopback` 与畸形名返回 None，
/// 不会把描述或其他字段误当成 GUID。
fn npf_guid(name: &str) -> Option<String> {
    let rest = name.strip_prefix(r"\Device\NPF_")?;
    let inner = rest.strip_prefix('{')?.strip_suffix('}')?;
    if inner.len() != 36 || !inner.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-') {
        return None;
    }
    Some(inner.to_ascii_lowercase())
}

fn from_listing(entries: Vec<(String, String)>) -> Vec<AdapterInfo> {
    let mut items = Vec::new();
    for (name, description) in entries {
        let name = name.trim();
        let description = description.trim();
        if name.is_empty() || name == "(null)" {
            continue;
        }
        if items.iter().any(|item: &AdapterInfo| item.name == name) {
            continue;
        }
        items.push(AdapterInfo {
            name: name.to_string(),
            description: description.to_string(),
            ..Default::default()
        });
    }
    items
}

#[derive(Clone, Debug, Deserialize, Default)]
struct NicMeta {
    label: Option<String>,
    mac: Option<String>,
    status: Option<String>,
    receive_link_bps: Option<u64>,
    transmit_link_bps: Option<u64>,
    hardware_interface: Option<bool>,
    #[serde(rename = "virtual")]
    virtual_nic: Option<bool>,
    /// 只读取 `Get-NetIPInterface` 的 NlMtu，不调用任何 Set cmdlet。
    mtu: Option<u32>,
    #[serde(default)]
    addresses: Vec<AdapterAddress>,
    #[allow(dead_code)]
    guid: String,
}

fn windows_nic_metadata() -> std::collections::HashMap<String, NicMeta> {
    let mut map = std::collections::HashMap::new();
    if !cfg!(windows) {
        return map;
    }
    const SCRIPT: &str = r#"[Console]::OutputEncoding = [Text.UTF8Encoding]::new()
$ips = @(Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue)
$ifs = @(Get-NetIPInterface -AddressFamily IPv4 -ErrorAction SilentlyContinue)
@(Get-NetAdapter -IncludeHidden | ForEach-Object {
  $nic = $_
  $iface = @($ifs | Where-Object ifIndex -eq $nic.ifIndex) | Select-Object -First 1
  [pscustomobject]@{guid=[string]$nic.InterfaceGuid; label=$nic.Name; mac=$nic.MacAddress;
    status=[string]$nic.Status; receive_link_bps=[uint64]$nic.ReceiveLinkSpeed;
    transmit_link_bps=[uint64]$nic.TransmitLinkSpeed;
    hardware_interface=[bool]$nic.HardwareInterface; virtual=[bool]$nic.Virtual;
    mtu=$(if ($null -ne $iface.NlMtu) { [uint32]$iface.NlMtu } else { $null });
    addresses=@($ips | Where-Object InterfaceIndex -eq $nic.ifIndex |
      ForEach-Object { [pscustomobject]@{ip=$_.IPAddress; prefix=$_.PrefixLength} })}
}) | ConvertTo-Json -Depth 4 -Compress"#;
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT]);
    let Ok(output) = run_bounded(command, METADATA_TIMEOUT) else {
        return map;
    };
    if output.timed_out || output.truncated {
        return map;
    }
    // 只接受成功退出的输出；PowerShell 失败时的半截文本不能当作网卡元数据。
    if !output.status.is_some_and(|status| status.success()) {
        return map;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return map;
    };
    let rows: Vec<NicMeta> = match value {
        serde_json::Value::Array(rows) => rows
            .into_iter()
            .filter_map(|row| serde_json::from_value(row).ok())
            .collect(),
        other => serde_json::from_value(other).into_iter().collect(),
    };
    for row in rows {
        let key = row
            .guid
            .trim_matches(|ch| ch == '{' || ch == '}')
            .to_ascii_lowercase();
        if !key.is_empty() {
            map.insert(key, row);
        }
    }
    map
}

pub(crate) struct BoundedOutput {
    pub(crate) status: Option<std::process::ExitStatus>,
    pub(crate) stdout: Vec<u8>,
    /// 保留子进程错误输出，便于诊断时查看；当前没有消费方。
    #[allow(dead_code)]
    pub(crate) stderr: Vec<u8>,
    pub(crate) timed_out: bool,
    pub(crate) truncated: bool,
}

/// 带超时与输出上限地运行子进程。超时后强杀并等待回收，避免僵尸进程。
pub(crate) fn run_bounded(
    mut command: Command,
    timeout: Duration,
) -> Result<BoundedOutput, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动子进程：{error}"))?;
    let stdout_reader = child
        .stdout
        .take()
        .map(|stdout| std::thread::spawn(move || read_capped(stdout, OUTPUT_CAP)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|stderr| std::thread::spawn(move || read_capped(stderr, OUTPUT_CAP)));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("等待子进程失败：{error}"));
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    let (stdout, stdout_truncated) = finish_reader(stdout_reader);
    let (stderr, stderr_truncated) = finish_reader(stderr_reader);
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    })
}

/// 只等待 reader 线程一小段时间。若子进程的后代仍持有管道写端，宁可丢弃输出
/// 也不能让控制面无限阻塞；线程被 detach，进程退出时随之结束。
fn finish_reader(handle: Option<std::thread::JoinHandle<(Vec<u8>, bool)>>) -> (Vec<u8>, bool) {
    let Some(handle) = handle else {
        return (Vec::new(), false);
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            // 没读完就 detach：把输出标记为截断，调用方不能把不完整结果当真。
            return (Vec::new(), true);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    handle.join().unwrap_or_default()
}

/// 保存前 `cap` 字节，其余继续读取丢弃，保证子进程不会因管道写满而阻塞。
fn read_capped(mut reader: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let room = cap.saturating_sub(kept.len());
                let take = read.min(room);
                kept.extend_from_slice(&buffer[..take]);
                if read > room {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (kept, truncated)
}

/// 精确设备名 → 唯一描述子串 → 无匹配。匹配不唯一返回错误，不静默选第一张。
pub fn resolve<'a>(
    items: &'a [AdapterInfo],
    selector: &str,
) -> Result<Option<&'a AdapterInfo>, String> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Ok(None);
    }
    if let Some(exact) = items
        .iter()
        .find(|item| item.name.eq_ignore_ascii_case(selector))
    {
        return Ok(Some(exact));
    }
    let needle = selector.to_ascii_lowercase();
    let matches: Vec<&AdapterInfo> = items
        .iter()
        .filter(|item| {
            item.name.to_ascii_lowercase().contains(&needle)
                || item.description.to_ascii_lowercase().contains(&needle)
                || item
                    .label
                    .as_deref()
                    .is_some_and(|label| label.to_ascii_lowercase().contains(&needle))
        })
        .collect();
    match matches.len() {
        0 => Err(format!("找不到网卡：{selector}")),
        1 => Ok(Some(matches[0])),
        _ => Err(format!(
            "网卡选择不唯一：{selector} 匹配到 {} 张网卡",
            matches.len()
        )),
    }
}

/// 按控制连接的本机出站地址精确匹配被采集机网卡；匹配不唯一报错。
/// 回环地址返回 Npcap 回环设备（其速率为未知，不伪装成物理链路）。
pub fn choose_local_adapter<'a>(
    items: &'a [AdapterInfo],
    controller_ip: &str,
) -> Result<&'a AdapterInfo, String> {
    let ip: std::net::Ipv4Addr = controller_ip
        .trim()
        .parse()
        .map_err(|_| format!("无效的主机 IPv4 地址：{controller_ip}"))?;
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")
        .map_err(|error| format!("无法创建路由探测套接字：{error}"))?;
    socket
        .connect((ip, 5143))
        .map_err(|error| format!("无法确定连接主机的网口：{error}"))?;
    let local_ip = socket
        .local_addr()
        .map_err(|error| format!("无法读取本机出站地址：{error}"))?
        .ip();
    adapter_for_local_ip(items, local_ip)
}

/// 路由匹配只接受已连接物理网卡；回环仅供上层显式诊断门禁使用。
fn adapter_for_local_ip(
    items: &[AdapterInfo],
    local_ip: std::net::IpAddr,
) -> Result<&AdapterInfo, String> {
    if local_ip.is_loopback() {
        return items
            .iter()
            .find(|item| is_loopback_device(item))
            .ok_or_else(|| "找不到 Npcap 回环设备（\\Device\\NPF_Loopback）。".to_string());
    }
    let local = local_ip.to_string();
    let matches: Vec<&AdapterInfo> = items
        .iter()
        .filter(|item| {
            item.is_connected_physical_nic()
                && item.addresses.iter().any(|address| address.ip == local)
        })
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!(
            "无法把本机出站地址 {local} 匹配到已连接的物理网卡，请检查网口 IPv4 配置或明确选择网卡。"
        )),
        _ => Err(format!(
            "本机出站地址 {local} 匹配到 {} 张已连接的物理网卡，无法唯一选择。",
            matches.len()
        )),
    }
}

/// 对端预览无法从本机查询远端路由，只在已连接物理网卡唯一时自动确认。
/// 没有候选返回 None；多张要求手动选择，任何厂商/型号都一视同仁。
pub fn unique_connected_physical(items: &[AdapterInfo]) -> Result<Option<&AdapterInfo>, String> {
    let mut candidates = items.iter().filter(|item| item.is_connected_physical_nic());
    let first = candidates.next();
    let count = usize::from(first.is_some()) + candidates.count();
    match count {
        0 | 1 => Ok(first),
        count => Err(format!(
            "检测到 {count} 张已连接的物理网卡，请手动选择发送网卡；不会按厂商或型号静默挑选。"
        )),
    }
}

/// 自动选择按控制连接出站 IPv4 精确匹配，不优先任何厂商或型号。
/// 回环控制器仅返回 Npcap 回环，上层仍须核对显式诊断授权。
pub fn choose_auto_adapter<'a>(
    items: &'a [AdapterInfo],
    controller_ip: &str,
) -> Result<&'a AdapterInfo, String> {
    choose_local_adapter(items, controller_ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_module_listing_and_skips_null_and_duplicates() {
        let items = from_listing(vec![
            (r"\Device\NPF_{AAA}".into(), "Intel I226-V".into()),
            (
                r"\Device\NPF_Loopback".into(),
                "Adapter for loopback".into(),
            ),
            ("(null)".into(), "bad".into()),
            (r"\Device\NPF_{AAA}".into(), "second".into()),
            ("  ".into(), "blank".into()),
        ]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].description, "Intel I226-V");
        assert_eq!(items[1].name, r"\Device\NPF_Loopback");
    }

    #[test]
    fn npf_guid_only_accepts_guid_shaped_device_names() {
        assert_eq!(
            npf_guid(r"\Device\NPF_{12345678-1234-1234-1234-1234567890AB}"),
            Some("12345678-1234-1234-1234-1234567890ab".into())
        );
        assert_eq!(npf_guid(r"\Device\NPF_Loopback"), None);
        assert_eq!(npf_guid(r"\Device\NPF_{not-a-guid}"), None);
        assert_eq!(
            npf_guid(r"\Device\NPF_12345678-1234-1234-1234-1234567890AB"),
            None
        );
        assert_eq!(npf_guid("no-prefix"), None);
    }

    #[test]
    fn resolve_requires_unique_substring() {
        let items = vec![
            AdapterInfo {
                name: "a".into(),
                description: "Intel I226-V".into(),
                ..Default::default()
            },
            AdapterInfo {
                name: "b".into(),
                description: "Intel I226-LM".into(),
                ..Default::default()
            },
        ];
        assert_eq!(resolve(&items, "a").unwrap().unwrap().name, "a");
        assert_eq!(resolve(&items, "A").unwrap().unwrap().name, "a");
        assert_eq!(resolve(&items, "I226-V").unwrap().unwrap().name, "a");
        assert!(resolve(&items, "Intel I226").is_err());
        assert!(resolve(&items, "Realtek").is_err());
        assert!(resolve(&items, "").unwrap().is_none());
    }

    #[test]
    fn resolve_matches_label_and_reports_cross_field_ambiguity() {
        let distinctive = vec![
            AdapterInfo {
                name: r"\Device\NPF_{A}".into(),
                description: "Ethernet".into(),
                label: Some("Corp LAN".into()),
                ..Default::default()
            },
            AdapterInfo {
                name: r"\Device\NPF_{B}".into(),
                description: "Wi-Fi".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            resolve(&distinctive, "corp lan").unwrap().unwrap().name,
            r"\Device\NPF_{A}"
        );

        // 一个命中 label、另一个命中 description，必须报不唯一而不是随便选。
        let ambiguous = vec![
            AdapterInfo {
                name: r"\Device\NPF_{A}".into(),
                description: "Ethernet".into(),
                label: Some("Corp LAN".into()),
                ..Default::default()
            },
            AdapterInfo {
                name: r"\Device\NPF_{B}".into(),
                description: "Corp LAN adapter".into(),
                ..Default::default()
            },
        ];
        assert!(resolve(&ambiguous, "corp lan").is_err());
    }

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
    fn physical_host_nics_require_explicit_hardware_and_nonvirtual_metadata() {
        let guid = r"\Device\NPF_{AAA}";
        assert!(nic(guid, Some(true), Some(false)).is_physical_host_nic());
        assert!(!nic(guid, Some(true), Some(true)).is_physical_host_nic());
        assert!(!nic(guid, Some(true), None).is_physical_host_nic());
        assert!(!nic(guid, None, Some(false)).is_physical_host_nic());
        assert!(!nic(guid, Some(false), Some(false)).is_physical_host_nic());
    }

    #[test]
    fn physical_host_nics_exclude_loopback_regardless_of_metadata_case() {
        assert!(!nic(r"\Device\NPF_Loopback", Some(true), Some(false)).is_physical_host_nic());
        assert!(!nic(r"\device\npf_loopback", Some(true), Some(false)).is_physical_host_nic());
        // 元数据缺失的回环同样不能冒充物理网卡。
        assert!(!nic(r"\Device\NPF_Loopback", None, None).is_physical_host_nic());
    }

    #[test]
    fn physical_host_nics_keeps_only_connected_physical_items_in_order() {
        let mut disconnected = nic(r"\Device\NPF_{D}", Some(true), Some(false));
        disconnected.status = Some("Disconnected".into());
        let mut unknown_status = nic(r"\Device\NPF_{S}", Some(true), Some(false));
        unknown_status.status = None;
        let mut disabled = nic(r"\Device\NPF_{X}", Some(true), Some(false));
        disabled.status = Some("Disabled".into());
        let items = vec![
            nic(r"\Device\NPF_{B}", Some(true), Some(false)),
            nic(r"\Device\NPF_{V}", Some(true), Some(true)),
            nic(r"\Device\NPF_{U}", None, None),
            nic(r"\Device\NPF_Loopback", Some(true), Some(false)),
            disconnected,
            unknown_status,
            disabled,
            nic(r"\Device\NPF_{A}", Some(true), Some(false)),
        ];
        let names: Vec<String> = physical_host_nics(&items)
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(names, vec![r"\Device\NPF_{B}", r"\Device\NPF_{A}"]);
    }

    #[test]
    fn connected_physical_requires_status_up_and_physical_metadata() {
        let mut down = nic(r"\Device\NPF_{D}", Some(true), Some(false));
        down.status = Some("Down".into());
        let mut missing = nic(r"\Device\NPF_{M}", Some(true), Some(false));
        missing.status = None;
        assert!(nic(r"\Device\NPF_{U}", Some(true), Some(false)).is_connected_physical_nic());
        assert!(!down.is_connected_physical_nic());
        assert!(!missing.is_connected_physical_nic());
        assert!(!nic(r"\Device\NPF_{V}", Some(true), Some(true)).is_connected_physical_nic());
        assert!(!nic(r"\Device\NPF_Loopback", Some(true), Some(false)).is_connected_physical_nic());
    }

    #[test]
    fn physical_nics_is_shared_by_host_and_capture() {
        let guid = r"\Device\NPF_{AAA}";
        assert!(nic(guid, Some(true), Some(false)).is_physical_nic());
        assert_eq!(
            nic(guid, Some(true), Some(false)).is_physical_nic(),
            nic(guid, Some(true), Some(false)).is_physical_host_nic()
        );
        assert_eq!(
            physical_nics(&[nic(guid, Some(true), Some(false))]),
            physical_host_nics(&[nic(guid, Some(true), Some(false))])
        );
    }

    #[test]
    fn loopback_detection_ignores_case_and_excludes_physical_claim() {
        assert!(is_loopback_device(&nic(
            r"\Device\NPF_Loopback",
            Some(true),
            Some(false)
        )));
        assert!(is_loopback_device(&nic(
            r"\device\npf_loopback",
            None,
            None
        )));
        assert!(!is_loopback_device(&nic(
            r"\Device\NPF_{AAA}",
            Some(true),
            Some(false)
        )));
        // 采集端诊断可以只认回环设备名，但物理列表永远剔除它。
        assert!(physical_nics(&[nic(r"\Device\NPF_Loopback", Some(true), Some(false))]).is_empty());
    }

    fn addressed_nic(name: &str, description: &str, ip: &str) -> AdapterInfo {
        AdapterInfo {
            description: description.into(),
            addresses: vec![AdapterAddress {
                ip: ip.into(),
                prefix: 24,
            }],
            ..nic(name, Some(true), Some(false))
        }
    }

    #[test]
    fn adapter_mtu_is_optional_and_serde_defaulted() {
        let legacy: AdapterInfo =
            serde_json::from_str(r#"{"name":"n","description":"d"}"#).unwrap();
        assert_eq!(legacy.mtu, None);
        let reported: AdapterInfo =
            serde_json::from_str(r#"{"name":"n","description":"d","mtu":1500}"#).unwrap();
        assert_eq!(reported.mtu, Some(1500));
        let unknown: AdapterInfo =
            serde_json::from_str(r#"{"name":"n","description":"d","mtu":null}"#).unwrap();
        assert_eq!(unknown.mtu, None);
    }

    #[test]
    fn unique_connected_physical_accepts_any_vendor_and_ignores_ineligible_items() {
        let mut disconnected = addressed_nic(r"\Device\NPF_{D}", "Intel I226-V", "192.0.2.2");
        disconnected.status = Some("Disconnected".into());
        let mut unknown = nic(r"\Device\NPF_{U}", Some(true), Some(false));
        unknown.status = None;
        let items = vec![
            disconnected,
            unknown,
            nic(r"\Device\NPF_{V}", Some(true), Some(true)),
            nic(r"\Device\NPF_{M}", None, None),
            nic(r"\Device\NPF_Loopback", Some(true), Some(false)),
            addressed_nic(r"\Device\NPF_{R}", "Realtek 2.5GbE", "192.0.2.3"),
        ];
        assert_eq!(
            unique_connected_physical(&items).unwrap().unwrap().name,
            r"\Device\NPF_{R}"
        );
        assert!(unique_connected_physical(&items[..5]).unwrap().is_none());
        assert!(unique_connected_physical(&[]).unwrap().is_none());
    }

    #[test]
    fn unique_connected_physical_refuses_multiple_cards_regardless_of_vendor() {
        let items = vec![
            addressed_nic(r"\Device\NPF_{A}", "Intel I226-V", "192.0.2.2"),
            addressed_nic(r"\Device\NPF_{B}", "Realtek 2.5GbE", "192.0.2.3"),
        ];
        let error = unique_connected_physical(&items).unwrap_err();
        assert!(error.contains("2 张") && error.contains("手动选择") && error.contains("静默"));
    }

    #[test]
    fn route_match_follows_exact_address_not_vendor_or_order() {
        let items = vec![
            addressed_nic(r"\Device\NPF_{A}", "Intel I226-V", "192.0.2.2"),
            addressed_nic(r"\Device\NPF_{B}", "Realtek 2.5GbE", "192.0.2.3"),
        ];
        let chosen = adapter_for_local_ip(&items, "192.0.2.3".parse().unwrap()).unwrap();
        assert_eq!(chosen.name, r"\Device\NPF_{B}");
        assert!(adapter_for_local_ip(&items, "192.0.2.30".parse().unwrap()).is_err());
    }

    #[test]
    fn route_match_rejects_disconnected_virtual_unknown_and_ambiguous_cards() {
        let address = "192.0.2.2".parse().unwrap();
        let physical = addressed_nic(r"\Device\NPF_{P}", "Ethernet", "192.0.2.2");
        let mut disconnected = physical.clone();
        disconnected.name = r"\Device\NPF_{D}".into();
        disconnected.status = Some("Disconnected".into());
        let mut virtual_nic = physical.clone();
        virtual_nic.virtual_nic = Some(true);
        let mut unknown = physical.clone();
        unknown.status = None;
        let mut unknown_hardware = physical.clone();
        unknown_hardware.hardware_interface = None;
        let mut items = vec![disconnected, virtual_nic, unknown, unknown_hardware];
        assert!(adapter_for_local_ip(&items, address).is_err());
        items.push(physical.clone());
        assert_eq!(
            adapter_for_local_ip(&items, address).unwrap().name,
            physical.name
        );
        let mut duplicate = physical;
        duplicate.name = r"\Device\NPF_{OTHER}".into();
        items.push(duplicate);
        let error = adapter_for_local_ip(&items, address).unwrap_err();
        assert!(error.contains("2 张") && error.contains("无法唯一选择"));
    }

    #[test]
    fn auto_adapter_validates_controller_even_with_connected_cards() {
        let items = vec![addressed_nic(
            r"\Device\NPF_{A}",
            "Intel I226-V",
            "192.0.2.2",
        )];
        let error = choose_auto_adapter(&items, "not-an-ip").unwrap_err();
        assert!(error.contains("无效的主机 IPv4"));
    }

    #[test]
    fn auto_adapter_keeps_loopback_diagnostics_on_loopback_device() {
        let items = vec![
            addressed_nic(r"\Device\NPF_{A}", "Intel I226-V", "192.0.2.2"),
            nic(r"\Device\NPF_Loopback", None, None),
        ];
        let chosen = choose_auto_adapter(&items, "127.0.0.1").unwrap();
        assert_eq!(chosen.name, r"\Device\NPF_Loopback");
        // 没有显式提供回环设备时，不能回退到任意物理卡。
        assert!(choose_auto_adapter(&items[..1], "127.0.0.1").is_err());
    }

    #[test]
    fn read_capped_limits_memory_but_drains() {
        let input = vec![b'x'; 100];
        let (kept, truncated) = read_capped(input.as_slice(), 10);
        assert_eq!(kept.len(), 10);
        assert!(truncated);

        let (kept, truncated) = read_capped(input.as_slice(), 1000);
        assert_eq!(kept.len(), 100);
        assert!(!truncated);
    }

    #[cfg(windows)]
    #[test]
    fn bounded_runner_captures_short_command() {
        let mut command = Command::new("cmd.exe");
        command.args(["/C", "echo hello"]);
        let output = run_bounded(command, Duration::from_secs(5)).unwrap();
        assert!(!output.timed_out);
        assert!(output.status.unwrap().success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    }
}
