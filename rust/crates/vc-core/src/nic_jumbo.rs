//! 本机物理网卡巨帧（*JumboPacket / IPv4 NlMtu）查询与显式切换。
//!
//! 只在采集端桌面用户主动操作滑块时修改用户选定物理网卡，后台自动枚举与会话启动仍保持只读。

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::adapters::run_bounded;

const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const APPLY_DIRECT_TIMEOUT: Duration = Duration::from_secs(30);
const APPLY_UAC_TIMEOUT: Duration = Duration::from_secs(65);
const UAC_CANCELLED_EXIT: i32 = 1223;

/// 开启/关闭网卡巨帧时写入 `*JumboPacket` 的目标注册表值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumboTargets {
    pub enable_value: String,
    pub disable_value: String,
}

/// 采集端本机已连接物理网卡的巨帧能力与当前状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NicJumboInfo {
    pub guid: String,
    pub label: String,
    pub description: String,
    pub nl_mtu: Option<u32>,
    pub current_registry_value: Option<String>,
    pub current_display_value: Option<String>,
    pub targets: Option<JumboTargets>,
    pub unsupported_reason: Option<String>,
}

impl NicJumboInfo {
    /// 与 [`crate::adapters::AdapterInfo::display`] 保持一致的网卡展示名。
    pub fn display(&self) -> String {
        if self.label.is_empty() {
            self.description.clone()
        } else {
            format!("{} — {}", self.label, self.description)
        }
    }

    pub fn is_supported(&self) -> bool {
        self.targets.is_some()
    }

    /// 优先以系统实际生效的 IPv4 `NlMtu` 判断是否处于巨帧状态；缺失时回退到驱动注册表值。
    pub fn is_jumbo_enabled(&self) -> bool {
        if let Some(mtu) = self.nl_mtu {
            return mtu >= 9000;
        }
        self.current_registry_value
            .as_deref()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .is_some_and(|v| v >= 9000)
    }

    /// 驱动已配置巨帧（>=9000）但 IPv4 `NlMtu` 仍低于 9000 时，提示未完全生效。
    pub fn has_mtu_mismatch(&self) -> bool {
        let reg_jumbo = self
            .current_registry_value
            .as_deref()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .is_some_and(|v| v >= 9000);
        reg_jumbo && self.nl_mtu.is_some_and(|mtu| mtu < 9000)
    }
}

/// 成功切换网卡巨帧后的核验结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub guid: String,
    pub label: String,
    pub enabled: bool,
    pub registry_value: String,
    pub nl_mtu: u32,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
struct RawNicJumboRow {
    guid: String,
    label: Option<String>,
    description: Option<String>,
    nl_mtu: Option<u32>,
    has_jumbo: bool,
    current_registry_value: Option<String>,
    current_display_value: Option<String>,
    default_registry_value: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    valid_registry_values: Vec<String>,
    numeric_min: Option<String>,
    numeric_max: Option<String>,
    numeric_step: Option<String>,
}

/// 兼容 Windows PowerShell 5.1 `ConvertTo-Json`：
/// 嵌套空数组会变成 `{}`，单元素数组可能退化为字符串/数字，无值时为 `null`。
fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    fn scalar_to_string(v: serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::String(s) => Some(s),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Object(map)) if map.is_empty() => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => {
            Ok(items.into_iter().filter_map(scalar_to_string).collect())
        }
        Some(other) => Ok(scalar_to_string(other).into_iter().collect()),
    }
}

/// 校验并规范化 36 位 UUID（小写、无花括号）。
pub fn normalize_guid(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches(|ch| ch == '{' || ch == '}');
    let bytes = trimmed.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    for (idx, &b) in bytes.iter().enumerate() {
        let expect_dash = matches!(idx, 8 | 13 | 18 | 23);
        if expect_dash {
            if b != b'-' {
                return None;
            }
        } else if !b.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(trimmed.to_ascii_lowercase())
}

/// 根据驱动上报的 `*JumboPacket` 枚举列表或数值范围，解析 9000 开启与 1500 关闭的注册表目标值。
pub fn resolve_jumbo_targets(
    has_jumbo: bool,
    valid_registry_values: &[String],
    default_registry_value: Option<&str>,
    numeric_min: Option<&str>,
    numeric_max: Option<&str>,
    numeric_step: Option<&str>,
) -> Result<JumboTargets, String> {
    if !has_jumbo {
        return Err("该网卡驱动未提供巨帧（*JumboPacket）高级属性".into());
    }

    // 分支 A：枚举型驱动（如 Realtek、VMware 等提供 ValidRegistryValues）。
    if !valid_registry_values.is_empty() {
        let mut parsed: Vec<(String, u32)> = Vec::new();
        for raw in valid_registry_values {
            let trimmed = raw.trim();
            if let Ok(num) = trimmed.parse::<u32>() {
                parsed.push((trimmed.to_string(), num));
            }
        }
        if parsed.is_empty() {
            return Err("无法解析该网卡驱动的可选巨帧档位".into());
        }
        let max_val = parsed.iter().map(|(_, n)| *n).max().unwrap_or(0);

        let enable_value = [9014u32, 9000, 9018]
            .into_iter()
            .find_map(|pref| {
                parsed
                    .iter()
                    .find(|(_, n)| *n == pref)
                    .map(|(s, _)| s.clone())
            })
            .or_else(|| {
                parsed
                    .iter()
                    .filter(|(_, n)| (9000..=9216).contains(n))
                    .min_by_key(|(_, n)| *n)
                    .map(|(s, _)| s.clone())
            })
            .ok_or_else(|| {
                if max_val > 0 && max_val < 9000 {
                    format!("该网卡驱动可选巨帧档位不包含 9000（最大仅支持 {max_val}）")
                } else {
                    "该网卡驱动可选巨帧档位不包含 9000".to_string()
                }
            })?;

        let default_num = default_registry_value.and_then(|s| s.trim().parse::<u32>().ok());
        let disable_value = [1514u32, 1500, 0]
            .into_iter()
            .find_map(|pref| {
                parsed
                    .iter()
                    .find(|(_, n)| *n == pref)
                    .map(|(s, _)| s.clone())
            })
            .or_else(|| {
                default_num
                    .filter(|&d| d <= 1518)
                    .and_then(|d| parsed.iter().find(|(_, n)| *n == d).map(|(s, _)| s.clone()))
            })
            .or_else(|| {
                parsed
                    .iter()
                    .filter(|(_, n)| *n <= 1518)
                    .min_by_key(|(_, n)| *n)
                    .map(|(s, _)| s.clone())
            })
            .ok_or_else(|| "该网卡驱动未提供 1500 标准帧档位".to_string())?;

        return Ok(JumboTargets {
            enable_value,
            disable_value,
        });
    }

    // 分支 B：数值范围型驱动（如 Intel I226-V：Min=1514, Max=9014, Step=1）。
    if let (Some(min_str), Some(max_str)) = (numeric_min, numeric_max)
        && let (Ok(min), Ok(max)) = (min_str.trim().parse::<u32>(), max_str.trim().parse::<u32>())
        && min <= max
    {
        let step = numeric_step
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(1);

        if max < 9000 {
            return Err(format!("该网卡驱动最大仅支持 {max}，不支持 9000 巨帧"));
        }

        let enable_num = [9014u32, 9000, 9018]
            .into_iter()
            .find(|&cand| in_stepped_range(cand, min, max, step))
            .or_else(|| {
                let upper = max.min(9216);
                let preferred_upper = if min <= 9014 { upper.min(9014) } else { upper };
                largest_stepped_at_most(preferred_upper, min, max, step)
                    .filter(|&v| v >= 9000)
                    .or_else(|| {
                        largest_stepped_at_most(upper, min, max, step).filter(|&v| v >= 9000)
                    })
            })
            .ok_or_else(|| {
                format!("该网卡驱动范围 {min}..={max}（步长 {step}）不支持 9000 巨帧")
            })?;

        let default_num = default_registry_value.and_then(|s| s.trim().parse::<u32>().ok());
        let disable_num = [1514u32, 1500]
            .into_iter()
            .find(|&cand| in_stepped_range(cand, min, max, step))
            .or_else(|| default_num.filter(|&d| d <= 1518 && in_stepped_range(d, min, max, step)))
            .or_else(|| (min <= 1518).then_some(min))
            .ok_or_else(|| format!("该网卡驱动范围 {min}..={max} 未包含 1500 标准帧值"))?;

        return Ok(JumboTargets {
            enable_value: enable_num.to_string(),
            disable_value: disable_num.to_string(),
        });
    }

    // 分支 C：既无枚举列表也无数值范围。
    Err("无法识别该网卡驱动的巨帧可选参数范围".into())
}

fn in_stepped_range(val: u32, min: u32, max: u32, step: u32) -> bool {
    val >= min && val <= max && (val - min).is_multiple_of(step)
}

fn largest_stepped_at_most(limit: u32, min: u32, max: u32, step: u32) -> Option<u32> {
    let cap = limit.min(max);
    if cap < min {
        return None;
    }
    let steps = (cap - min) / step;
    Some(min + steps * step)
}

/// 解析 PowerShell 只读查询输出的 JSON，保持网卡返回顺序。
pub fn parse_query_output(raw: &str) -> Result<Vec<NicJumboInfo>, String> {
    let trimmed = raw.trim().trim_start_matches('\u{feff}');
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("网卡巨帧查询结果 JSON 解析失败：{e}"))?;
    let rows: Vec<RawNicJumboRow> = match value {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("网卡巨帧查询条目解析失败：{e}"))?,
        other @ serde_json::Value::Object(_) => {
            vec![
                serde_json::from_value(other)
                    .map_err(|e| format!("网卡巨帧查询条目解析失败：{e}"))?,
            ]
        }
        _ => return Err("网卡巨帧查询输出格式无效".into()),
    };

    let mut result = Vec::new();
    for row in rows {
        let Some(guid) = normalize_guid(&row.guid) else {
            continue;
        };
        let (targets, unsupported_reason) = match resolve_jumbo_targets(
            row.has_jumbo,
            &row.valid_registry_values,
            row.default_registry_value.as_deref(),
            row.numeric_min.as_deref(),
            row.numeric_max.as_deref(),
            row.numeric_step.as_deref(),
        ) {
            Ok(targets) => (Some(targets), None),
            Err(reason) => (None, Some(reason)),
        };
        result.push(NicJumboInfo {
            guid,
            label: row.label.unwrap_or_default().trim().to_string(),
            description: row.description.unwrap_or_default().trim().to_string(),
            nl_mtu: row.nl_mtu,
            current_registry_value: row
                .current_registry_value
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            current_display_value: row
                .current_display_value
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            targets,
            unsupported_reason,
        });
    }
    Ok(result)
}

/// 只读枚举本机所有已连接物理网卡（`Status=Up`、`HardwareInterface=True`、`Virtual=False`）的巨帧状态。
pub fn list_physical_nic_jumbo() -> Result<Vec<NicJumboInfo>, String> {
    if !cfg!(windows) {
        return Ok(Vec::new());
    }
    const SCRIPT: &str = r#"[Console]::OutputEncoding = [Text.UTF8Encoding]::new()
$ifs = @(Get-NetIPInterface -AddressFamily IPv4 -ErrorAction SilentlyContinue)
$nics = @(Get-NetAdapter | Where-Object { $_.HardwareInterface -eq $true -and -not $_.Virtual -and ([string]$_.Status).Trim() -ieq 'Up' })
$rows = @($nics | ForEach-Object {
  $nic = $_
  $iface = @($ifs | Where-Object ifIndex -eq $nic.ifIndex) | Select-Object -First 1
  $jp = Get-NetAdapterAdvancedProperty -Name $nic.Name -RegistryKeyword '*JumboPacket' -ErrorAction SilentlyContinue | Select-Object -First 1
  $regVal = if ($null -ne $jp -and $null -ne $jp.RegistryValue) { [string](@($jp.RegistryValue) | Select-Object -First 1) } else { $null }
  $validVals = if ($null -ne $jp -and $null -ne $jp.ValidRegistryValues) { @($jp.ValidRegistryValues | ForEach-Object { [string]$_ }) } else { $null }
  [pscustomobject]@{
    guid = [string]$nic.InterfaceGuid
    label = [string]$nic.Name
    description = [string]$nic.InterfaceDescription
    nl_mtu = $(if ($null -ne $iface -and $null -ne $iface.NlMtu) { [uint32]$iface.NlMtu } else { $null })
    has_jumbo = ($null -ne $jp)
    current_registry_value = $regVal
    current_display_value = $(if ($null -ne $jp -and $null -ne $jp.DisplayValue) { [string]$jp.DisplayValue } else { $null })
    default_registry_value = $(if ($null -ne $jp -and $null -ne $jp.DefaultRegistryValue) { [string]$jp.DefaultRegistryValue } else { $null })
    valid_registry_values = $validVals
    numeric_min = $(if ($null -ne $jp -and $null -ne $jp.NumericParameterMinValue) { [string]$jp.NumericParameterMinValue } else { $null })
    numeric_max = $(if ($null -ne $jp -and $null -ne $jp.NumericParameterMaxValue) { [string]$jp.NumericParameterMaxValue } else { $null })
    numeric_step = $(if ($null -ne $jp -and $null -ne $jp.NumericParameterStepValue) { [string]$jp.NumericParameterStepValue } else { $null })
  }
})
$rows | ConvertTo-Json -Depth 4 -Compress"#;

    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT]);
    let output = run_bounded(command, QUERY_TIMEOUT)?;
    if output.timed_out {
        return Err("查询物理网卡巨帧状态超时。".into());
    }
    if output.truncated {
        return Err("查询物理网卡巨帧状态输出超限。".into());
    }
    if !output.status.is_some_and(|s| s.success()) {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "查询物理网卡巨帧状态失败。".into()
        } else {
            format!("查询物理网卡巨帧状态失败：{stderr}")
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_query_output(&stdout)
}

/// 严格校验传入修改脚本的 GUID 和目标注册表值，杜绝 PowerShell 注入或非法参数。
pub fn validate_apply_inputs(
    guid: &str,
    enable: bool,
    targets: &JumboTargets,
) -> Result<(String, String, u32), String> {
    let guid = normalize_guid(guid).ok_or_else(|| "网卡 GUID 格式无效，拒绝修改。".to_string())?;
    let raw_target = if enable {
        targets.enable_value.trim()
    } else {
        targets.disable_value.trim()
    };
    if raw_target.is_empty() || !raw_target.bytes().all(|b| b.is_ascii_digit()) {
        return Err("巨帧目标参数不是有效纯数字，拒绝修改。".into());
    }
    let target_mtu = if enable { 9000 } else { 1500 };
    Ok((guid, raw_target.to_string(), target_mtu))
}

#[derive(Deserialize)]
struct RawApplyResult {
    ok: bool,
    #[serde(default)]
    guid: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    registry_value: String,
    #[serde(default)]
    nl_mtu: Option<u32>,
    #[serde(default)]
    error: Option<String>,
}

/// 解析修改脚本写出的结果 JSON。
pub fn parse_apply_result(raw: &str, enable: bool) -> Result<ApplyOutcome, String> {
    let trimmed = raw.trim().trim_start_matches('\u{feff}');
    if trimmed.is_empty() {
        return Err("修改网卡巨帧未返回核验结果。".into());
    }
    let parsed: RawApplyResult =
        serde_json::from_str(trimmed).map_err(|e| format!("网卡巨帧修改结果解析失败：{e}"))?;
    if !parsed.ok {
        return Err(parsed
            .error
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "修改网卡巨帧失败。".into()));
    }
    let nl_mtu = parsed
        .nl_mtu
        .ok_or_else(|| "修改网卡巨帧后未能读取到 IPv4 NlMtu。".to_string())?;
    if enable && nl_mtu < 9000 {
        return Err(format!(
            "网卡 *JumboPacket 已写入但 IPv4 NlMtu={nl_mtu} 仍低于 9000。"
        ));
    }
    if !enable && nl_mtu != 1500 {
        return Err(format!(
            "网卡 *JumboPacket 已写入但 IPv4 NlMtu={nl_mtu} 未恢复到 1500。"
        ));
    }
    Ok(ApplyOutcome {
        guid: normalize_guid(&parsed.guid).unwrap_or(parsed.guid),
        label: parsed.label,
        enabled: enable,
        registry_value: parsed.registry_value,
        nl_mtu,
    })
}

/// 检查当前进程是否已具备管理员权限（避免已提权时重复弹 UAC）。
#[cfg(windows)]
pub fn is_process_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 || token.is_null() {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret_len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
    };
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

#[cfg(not(windows))]
pub fn is_process_elevated() -> bool {
    false
}

struct TempDirGuard {
    path: PathBuf,
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn escape_ps_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

fn write_utf8_bom(path: &std::path::Path, content: &str) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(3 + content.len());
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(content.as_bytes());
    std::fs::write(path, bytes).map_err(|e| format!("无法写入临时脚本 {}：{e}", path.display()))
}

/// 切换指定物理网卡的 `*JumboPacket`（9000 / 1500），等待网卡恢复连接并核验 IPv4 `NlMtu`。
///
/// 当前进程非管理员时会通过 `Start-Process -Verb RunAs` 触发 UAC 提权。
pub fn apply_nic_jumbo(
    guid: &str,
    enable: bool,
    targets: &JumboTargets,
) -> Result<ApplyOutcome, String> {
    if !cfg!(windows) {
        return Err("仅支持在 Windows 平台修改网卡巨帧。".into());
    }
    let (guid, target_value, target_mtu) = validate_apply_inputs(guid, enable, targets)?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_dir =
        std::env::temp_dir().join(format!("vc-nic-jumbo-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).map_err(|e| format!("无法创建网卡巨帧临时目录：{e}"))?;
    let _guard = TempDirGuard {
        path: temp_dir.clone(),
    };

    let script_path = temp_dir.join("apply.ps1");
    let result_path = temp_dir.join("result.json");
    let result_ps = escape_ps_single_quoted(&result_path.to_string_lossy());
    let enable_ps = if enable { "$true" } else { "$false" };

    // 所有可变参数均经 validate_apply_inputs 严格白名单校验（UUID / 纯数字 / 布尔值）。
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$targetGuid = '{guid}'
$targetValue = '{target_value}'
$targetMtu = {target_mtu}
$enableJumbo = {enable_ps}
$resultPath = '{result_ps}'

function Write-Result([hashtable]$payload) {{
  $payload | ConvertTo-Json -Compress | Set-Content -Path $resultPath -Encoding UTF8
}}

function Find-TargetNic([string]$g) {{
  Get-NetAdapter | Where-Object {{
    $_.HardwareInterface -eq $true -and -not $_.Virtual -and
    ([string]$_.InterfaceGuid).Trim('{{','}}').ToLowerInvariant() -eq $g
  }} | Select-Object -First 1
}}

try {{
  $nic = Find-TargetNic $targetGuid
  if ($null -eq $nic) {{
    Write-Result @{{ ok = $false; error = '未找到匹配的物理网卡，可能已被拔出或禁用。' }}
    exit 1
  }}

  Set-NetAdapterAdvancedProperty -Name $nic.Name -RegistryKeyword '*JumboPacket' -RegistryValue $targetValue -ErrorAction Stop

  $matched = $false
  $fallbackApplied = $false
  $lastMtu = $null
  $lastStatus = ''
  $lastLabel = [string]$nic.Name
  $lastReg = ''

  for ($i = 0; $i -lt 40; $i++) {{
    Start-Sleep -Milliseconds 300
    $cur = Find-TargetNic $targetGuid
    if ($null -eq $cur) {{ continue }}
    $lastLabel = [string]$cur.Name
    $lastStatus = ([string]$cur.Status).Trim()
    $iface = Get-NetIPInterface -InterfaceIndex $cur.ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue | Select-Object -First 1
    $lastMtu = if ($null -ne $iface -and $null -ne $iface.NlMtu) {{ [uint32]$iface.NlMtu }} else {{ $null }}
    $jp = Get-NetAdapterAdvancedProperty -Name $cur.Name -RegistryKeyword '*JumboPacket' -ErrorAction SilentlyContinue | Select-Object -First 1
    $lastReg = if ($null -ne $jp -and $null -ne $jp.RegistryValue) {{ [string](@($jp.RegistryValue) | Select-Object -First 1) }} else {{ '' }}

    $mtuOk = if ($enableJumbo) {{ $null -ne $lastMtu -and $lastMtu -ge 9000 }} else {{ $null -ne $lastMtu -and $lastMtu -eq 1500 }}
    if ($lastStatus -ieq 'Up' -and $mtuOk) {{
      $matched = $true
      break
    }}
    if (-not $fallbackApplied -and $i -ge 15 -and $lastStatus -ieq 'Up' -and -not $mtuOk) {{
      $fallbackApplied = $true
      Set-NetIPInterface -InterfaceIndex $cur.ifIndex -AddressFamily IPv4 -NlMtuBytes $targetMtu -ErrorAction SilentlyContinue
    }}
  }}

  if ($matched) {{
    Write-Result @{{
      ok = $true
      guid = $targetGuid
      label = $lastLabel
      registry_value = $lastReg
      nl_mtu = $lastMtu
    }}
    exit 0
  }} else {{
    $mtuText = if ($null -ne $lastMtu) {{ [string]$lastMtu }} else {{ '未知' }}
    Write-Result @{{
      ok = $false
      error = "网卡巨帧切换未生效：当前 Status=$lastStatus, *JumboPacket=$lastReg, IPv4 NlMtu=$mtuText"
    }}
    exit 1
  }}
}} catch {{
  Write-Result @{{ ok = $false; error = $_.Exception.Message }}
  exit 1
}}
"#
    );
    write_utf8_bom(&script_path, &script)?;

    if is_process_elevated() {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ]);
        command.arg(&script_path);
        let output = run_bounded(command, APPLY_DIRECT_TIMEOUT)?;
        if output.timed_out {
            return Err("修改网卡巨帧超时（等待网卡重连超过 30 秒）。".into());
        }
        if result_path.exists() {
            let content = std::fs::read_to_string(&result_path)
                .map_err(|e| format!("读取巨帧修改结果失败：{e}"))?;
            return parse_apply_result(&content, enable);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "修改网卡巨帧失败，未产生结果文件。".into()
        } else {
            format!("修改网卡巨帧失败：{stderr}")
        });
    }

    // 非管理员进程：通过 Start-Process -Verb RunAs 触发 UAC，等待提权后的脚本退出。
    const WRAPPER: &str = r#"$ErrorActionPreference = 'Stop'
try {
  $p = Start-Process -FilePath 'powershell.exe' -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList @('-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',$env:VC_JUMBO_SCRIPT)
  exit $p.ExitCode
} catch [System.ComponentModel.Win32Exception] {
  if ($_.Exception.NativeErrorCode -eq 1223) { exit 1223 }
  [Console]::Error.WriteLine($_.Exception.Message)
  exit 1
} catch {
  [Console]::Error.WriteLine($_.Exception.Message)
  exit 1
}"#;
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", WRAPPER]);
    command.env("VC_JUMBO_SCRIPT", &script_path);
    let output = run_bounded(command, APPLY_UAC_TIMEOUT)?;
    if output.timed_out {
        return Err("修改网卡巨帧超时（等待管理员授权或网卡重连超时）。".into());
    }
    if output.status.and_then(|s| s.code()) == Some(UAC_CANCELLED_EXIT) {
        return Err("已取消管理员授权，未修改网卡巨帧设置。".into());
    }
    if result_path.exists() {
        let content = std::fs::read_to_string(&result_path)
            .map_err(|e| format!("读取巨帧修改结果失败：{e}"))?;
        return parse_apply_result(&content, enable);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        "未完成管理员授权或修改脚本执行失败。".into()
    } else {
        format!("修改网卡巨帧失败：{stderr}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_targets_supports_numeric_range_like_intel_i226() {
        let targets = resolve_jumbo_targets(
            true,
            &[],
            Some("1514"),
            Some("1514"),
            Some("9014"),
            Some("1"),
        )
        .unwrap();
        assert_eq!(targets.enable_value, "9014");
        assert_eq!(targets.disable_value, "1514");
    }

    #[test]
    fn resolve_targets_supports_enum_lists_and_rejects_small_max() {
        let realtek = resolve_jumbo_targets(
            true,
            &["1514".into(), "4088".into(), "9014".into()],
            Some("1514"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(realtek.enable_value, "9014");
        assert_eq!(realtek.disable_value, "1514");

        let exact_9000 = resolve_jumbo_targets(
            true,
            &["1500".into(), "9000".into()],
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(exact_9000.enable_value, "9000");
        assert_eq!(exact_9000.disable_value, "1500");

        let alt_9018 = resolve_jumbo_targets(
            true,
            &["1514".into(), "9018".into()],
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(alt_9018.enable_value, "9018");
        assert_eq!(alt_9018.disable_value, "1514");

        let too_small = resolve_jumbo_targets(
            true,
            &["1514".into(), "4088".into()],
            Some("1514"),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(too_small.contains("4088"), "{too_small}");

        let missing_prop = resolve_jumbo_targets(false, &[], None, None, None, None).unwrap_err();
        assert!(missing_prop.contains("*JumboPacket"), "{missing_prop}");
    }

    #[test]
    fn parses_real_query_shapes_preserving_order_and_skipping_bad_guid() {
        assert!(parse_query_output("").unwrap().is_empty());
        assert!(parse_query_output("null").unwrap().is_empty());

        let single = r#"{
            "guid": "{7406EA46-5837-40C4-8598-D3B5063E5684}",
            "label": "以太网 3",
            "description": "Intel(R) Ethernet Controller I226-V",
            "nl_mtu": 9000,
            "has_jumbo": true,
            "current_registry_value": "9014",
            "current_display_value": "9014",
            "default_registry_value": "1514",
            "valid_registry_values": [],
            "numeric_min": "1514",
            "numeric_max": "9014",
            "numeric_step": "1"
        }"#;
        let parsed = parse_query_output(single).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].guid, "7406ea46-5837-40c4-8598-d3b5063e5684");
        assert!(parsed[0].is_supported());
        assert!(parsed[0].is_jumbo_enabled());
        assert!(!parsed[0].has_mtu_mismatch());

        let array = r#"[
            {
                "guid": "{BDD01E5A-6314-4A19-8CA1-E7E19C59791B}",
                "label": "以太网 2",
                "description": "USB 2.0 Ethernet Adapter",
                "nl_mtu": 1500,
                "has_jumbo": false
            },
            {
                "guid": "not-a-guid",
                "label": "Bad",
                "description": "Bad",
                "has_jumbo": false
            },
            {
                "guid": "{7406EA46-5837-40C4-8598-D3B5063E5684}",
                "label": "以太网 3",
                "description": "Intel(R) Ethernet Controller I226-V",
                "nl_mtu": 1500,
                "has_jumbo": true,
                "current_registry_value": "9014",
                "valid_registry_values": ["1514", "9014"]
            }
        ]"#;
        let items = parse_query_output(array).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "以太网 2");
        assert!(!items[0].is_supported());
        assert!(items[0].unsupported_reason.is_some());
        assert!(!items[0].is_jumbo_enabled());

        assert_eq!(items[1].label, "以太网 3");
        assert!(items[1].is_supported());
        assert!(!items[1].is_jumbo_enabled());
        assert!(items[1].has_mtu_mismatch());
    }

    #[test]
    fn validate_apply_inputs_rejects_bad_guid_and_non_numeric_values() {
        let ok_targets = JumboTargets {
            enable_value: "9014".into(),
            disable_value: "1514".into(),
        };
        let (guid, val, mtu) =
            validate_apply_inputs("{7406EA46-5837-40C4-8598-D3B5063E5684}", true, &ok_targets)
                .unwrap();
        assert_eq!(guid, "7406ea46-5837-40c4-8598-d3b5063e5684");
        assert_eq!(val, "9014");
        assert_eq!(mtu, 9000);

        assert!(validate_apply_inputs("7406ea46'; Calc.exe", true, &ok_targets).is_err());

        let bad_targets = JumboTargets {
            enable_value: "9014;whoami".into(),
            disable_value: "1514".into(),
        };
        assert!(
            validate_apply_inputs("7406ea46-5837-40c4-8598-d3b5063e5684", true, &bad_targets)
                .is_err()
        );
    }

    #[test]
    fn parse_apply_result_checks_ok_mtu_and_errors() {
        let bom_ok = "\u{feff}{\"ok\":true,\"guid\":\"{7406EA46-5837-40C4-8598-D3B5063E5684}\",\"label\":\"以太网 3\",\"registry_value\":\"9014\",\"nl_mtu\":9000}";
        let outcome = parse_apply_result(bom_ok, true).unwrap();
        assert_eq!(outcome.guid, "7406ea46-5837-40c4-8598-d3b5063e5684");
        assert_eq!(outcome.nl_mtu, 9000);
        assert!(outcome.enabled);

        let mismatch = "{\"ok\":true,\"guid\":\"7406ea46-5837-40c4-8598-d3b5063e5684\",\"label\":\"以太网 3\",\"registry_value\":\"9014\",\"nl_mtu\":1500}";
        assert!(parse_apply_result(mismatch, true).is_err());

        let disable_ok = "{\"ok\":true,\"guid\":\"7406ea46-5837-40c4-8598-D3B5063E5684\",\"label\":\"以太网 3\",\"registry_value\":\"1514\",\"nl_mtu\":1500}";
        assert_eq!(parse_apply_result(disable_ok, false).unwrap().nl_mtu, 1500);

        let failed = "{\"ok\":false,\"error\":\"拒绝访问\"}";
        assert_eq!(parse_apply_result(failed, true).unwrap_err(), "拒绝访问");

        assert!(parse_apply_result("{broken", true).is_err());
    }

    #[test]
    fn parses_powershell_51_nested_empty_object_and_scalar_valid_values() {
        let ps51_output = r#"[{"guid":"{BDD01E5A-6314-4A19-8CA1-E7E19C59791B}","label":"以太网 2","description":"USB 2.0 Ethernet Adapter","nl_mtu":1500,"has_jumbo":false,"current_registry_value":null,"current_display_value":null,"default_registry_value":null,"valid_registry_values":{},"numeric_min":null,"numeric_max":null,"numeric_step":null},{"guid":"{7406EA46-5837-40C4-8598-D3B5063E5684}","label":"以太网 3","description":"Intel(R) Ethernet Controller I226-V","nl_mtu":9000,"has_jumbo":true,"current_registry_value":"9014","current_display_value":"9014","default_registry_value":"1514","valid_registry_values":{},"numeric_min":"1514","numeric_max":"9014","numeric_step":"1"},{"guid":"{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}","label":"以太网 4","description":"Scalar NIC","nl_mtu":1500,"has_jumbo":true,"current_registry_value":"1514","valid_registry_values":"9014"}]"#;
        let items = parse_query_output(ps51_output).unwrap();
        assert_eq!(items.len(), 3);
        assert!(!items[0].is_supported());
        assert!(items[1].is_supported());
        assert_eq!(
            items[1].targets.as_ref().unwrap(),
            &JumboTargets {
                enable_value: "9014".into(),
                disable_value: "1514".into()
            }
        );
        // 仅有一个 9014 值且无 1500 档位时明确标为不支持，不会报错崩溃。
        assert!(!items[2].is_supported());
    }

    #[cfg(windows)]
    #[test]
    fn real_host_read_only_query_succeeds() {
        let items = list_physical_nic_jumbo().unwrap();
        for item in &items {
            assert!(normalize_guid(&item.guid).is_some());
        }
    }
}
