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

    /// 兼容旧名：主机接收网卡与采集网卡共用同一物理判定。
    pub fn is_physical_host_nic(&self) -> bool {
        self.is_physical_nic()
    }
}

/// Npcap 回环设备名，大小写无关。
pub fn is_loopback_device(item: &AdapterInfo) -> bool {
    item.name.eq_ignore_ascii_case(NPF_LOOPBACK)
}

const NPF_LOOPBACK: &str = r"\Device\NPF_Loopback";

/// 过滤出可确认的物理网卡，保持原有顺序；虚拟、回环与元数据未知都不进入。
pub fn physical_nics(items: &[AdapterInfo]) -> Vec<AdapterInfo> {
    items
        .iter()
        .filter(|item| item.is_physical_nic())
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

struct BoundedOutput {
    status: Option<std::process::ExitStatus>,
    stdout: Vec<u8>,
    /// 保留子进程错误输出，便于诊断时查看；当前没有消费方。
    #[allow(dead_code)]
    stderr: Vec<u8>,
    timed_out: bool,
    truncated: bool,
}

/// 带超时与输出上限地运行子进程。超时后强杀并等待回收，避免僵尸进程。
fn run_bounded(mut command: Command, timeout: Duration) -> Result<BoundedOutput, String> {
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
    if local_ip.is_loopback() {
        return items
            .iter()
            .find(|item| is_loopback_device(item))
            .ok_or_else(|| "找不到 Npcap 回环设备（\\Device\\NPF_Loopback）。".to_string());
    }
    let local = local_ip.to_string();
    let matches: Vec<&AdapterInfo> = items
        .iter()
        .filter(|item| item.addresses.iter().any(|address| address.ip == local))
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!(
            "无法把本机出站地址 {local} 匹配到采集网卡，请检查网口 IPv4 配置。"
        )),
        _ => Err(format!(
            "本机出站地址 {local} 匹配到 {} 张网卡，无法唯一选择。",
            matches.len()
        )),
    }
}

/// Intel I226 标记：只看系统名称/描述，大小写无关。
fn is_i226(item: &AdapterInfo) -> bool {
    item.description.to_ascii_lowercase().contains("i226")
        || item
            .label
            .as_deref()
            .is_some_and(|label| label.to_ascii_lowercase().contains("i226"))
}

/// 可确认物理网卡中的唯一 Intel I226。
///
/// 0 张返回 `Ok(None)`；多于 1 张返回错误而不是静默挑第一张，调用方必须要求
/// 显式选择。虚拟网卡、回环与元数据未知的设备永远不参与 I226 优选。
pub fn unique_i226(items: &[AdapterInfo]) -> Result<Option<&AdapterInfo>, String> {
    let matches: Vec<&AdapterInfo> = items
        .iter()
        .filter(|item| item.is_physical_nic() && is_i226(item))
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        count => Err(format!(
            "检测到 {count} 张可确认的 Intel I226 物理网卡，无法自动唯一选择；请在配置中显式指定网卡，绝不静默挑一张。"
        )),
    }
}

/// 未显式选择网卡时的自动选择：非回环控制器优先唯一 Intel I226，
/// 否则沿用按控制连接出站 IPv4 精确匹配的路由选择。
///
/// 回环控制器（本机诊断）始终走 Npcap 回环，不因机器上存在 I226 而改选物理网卡。
/// 多张 I226 直接报错；显式选择不经过本函数，保持原样。
pub fn choose_auto_adapter<'a>(
    items: &'a [AdapterInfo],
    controller_ip: &str,
) -> Result<&'a AdapterInfo, String> {
    let ip: std::net::Ipv4Addr = controller_ip
        .trim()
        .parse()
        .map_err(|_| format!("无效的主机 IPv4 地址：{controller_ip}"))?;
    if ip.is_loopback() {
        return items
            .iter()
            .find(|item| is_loopback_device(item))
            .ok_or_else(|| "找不到 Npcap 回环设备（\\Device\\NPF_Loopback）。".to_string());
    }
    if let Some(i226) = unique_i226(items)? {
        return Ok(i226);
    }
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
    fn physical_host_nics_keeps_only_eligible_items_in_order() {
        let items = vec![
            nic(r"\Device\NPF_{B}", Some(true), Some(false)),
            nic(r"\Device\NPF_{V}", Some(true), Some(true)),
            nic(r"\Device\NPF_{U}", None, None),
            nic(r"\Device\NPF_Loopback", Some(true), Some(false)),
            nic(r"\Device\NPF_{A}", Some(true), Some(false)),
        ];
        let names: Vec<String> = physical_host_nics(&items)
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(names, vec![r"\Device\NPF_{B}", r"\Device\NPF_{A}"]);
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

    fn i226(name: &str, label: Option<&str>, description: &str) -> AdapterInfo {
        AdapterInfo {
            name: name.into(),
            description: description.into(),
            label: label.map(str::to_string),
            hardware_interface: Some(true),
            virtual_nic: Some(false),
            mtu: Some(9000),
            ..Default::default()
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
    fn unique_i226_counts_only_confirmed_physical_matches() {
        let items = vec![
            i226(r"\Device\NPF_{A}", Some("Ethernet 2"), "Intel(R) I226-V"),
            nic(r"\Device\NPF_{V}", Some(true), Some(true)),
            AdapterInfo {
                name: r"\Device\NPF_{U}".into(),
                description: "Intel I226-V".into(),
                ..Default::default()
            },
            i226(r"\Device\NPF_Loopback", None, "Intel I226-V"),
            AdapterInfo {
                name: r"\Device\NPF_{R}".into(),
                description: "Realtek 2.5GbE".into(),
                hardware_interface: Some(true),
                virtual_nic: Some(false),
                ..Default::default()
            },
        ];
        let found = unique_i226(&items).unwrap().unwrap();
        assert_eq!(found.name, r"\Device\NPF_{A}");
        // 标签命中同样有效。
        let by_label = vec![i226(
            r"\Device\NPF_{B}",
            Some("I226-LM Control"),
            "Ethernet Controller",
        )];
        assert_eq!(
            unique_i226(&by_label).unwrap().unwrap().name,
            r"\Device\NPF_{B}"
        );
        // 没有 I226 时返回 None，而不是错误。
        assert!(
            unique_i226(&[nic(r"\Device\NPF_{R}", Some(true), Some(false))])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unique_i226_refuses_to_pick_between_multiple_cards() {
        let items = vec![
            i226(
                r"\Device\NPF_{A}",
                None,
                "Intel(R) Ethernet Controller I226-V",
            ),
            i226(
                r"\Device\NPF_{B}",
                None,
                "Intel(R) Ethernet Controller I226-LM",
            ),
        ];
        let error = unique_i226(&items).unwrap_err();
        assert!(error.contains("2 张") && error.contains("显式指定") && error.contains("静默"));
    }

    #[test]
    fn auto_adapter_prefers_unique_i226_without_route_dependency() {
        let items = vec![
            i226(
                r"\Device\NPF_{A}",
                None,
                "Intel(R) Ethernet Controller I226-V",
            ),
            nic(r"\Device\NPF_{B}", Some(true), Some(false)),
        ];
        // TEST-NET-1 地址：唯一 I226 分支在创建路由探测前返回，不依赖真实网络。
        let chosen = choose_auto_adapter(&items, "192.0.2.10").unwrap();
        assert_eq!(chosen.name, r"\Device\NPF_{A}");
        let error = choose_auto_adapter(&items[..1], "not-an-ip").unwrap_err();
        assert!(error.contains("无效的主机 IPv4"));
    }

    #[test]
    fn auto_adapter_keeps_loopback_diagnostics_on_loopback_device() {
        let items = vec![
            i226(
                r"\Device\NPF_{A}",
                None,
                "Intel(R) Ethernet Controller I226-V",
            ),
            nic(r"\Device\NPF_Loopback", None, None),
        ];
        let chosen = choose_auto_adapter(&items, "127.0.0.1").unwrap();
        assert_eq!(chosen.name, r"\Device\NPF_Loopback");
    }

    #[test]
    fn auto_adapter_reports_multiple_i226_before_route_probe() {
        let items = vec![
            i226(
                r"\Device\NPF_{A}",
                None,
                "Intel(R) Ethernet Controller I226-V",
            ),
            i226(
                r"\Device\NPF_{B}",
                None,
                "Intel(R) Ethernet Controller I226-LM",
            ),
        ];
        let error = choose_auto_adapter(&items, "192.0.2.10").unwrap_err();
        assert!(error.contains("2 张"));
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
