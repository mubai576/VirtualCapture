<#
.SYNOPSIS
  check_mtu.ps1 — 数据网卡 9K 粗验 (T-05)。打印 Jumbo/速率/EEE/中断合并属性，
  临时配 169.254.x.x 做 ping -f -l 8972 通断粗验，验完删 IP 还原无 IP 状态。
.DESCRIPTION
  只做三层粗验 (ping)，二层真 9K 收发计数以 T1 为准。
  数据网卡平时必须无 IP (自定义 EtherType 走 NDIS 裸发，不进 tcpip.sys)。
  本脚本用 try/finally 保证任何失败都删掉临时 IP。
.PARAMETER AdapterName
  数据网卡名 (Get-NetAdapter 看到的 Name 列)。
.PARAMETER TempIP
  临时 IP，默认 169.254.99.1/16。
.EXAMPLE
  .\scripts\check_mtu.ps1 -AdapterName "以太网 2"
  .\scripts\check_mtu.ps1 -AdapterName "以太网 2" -WhatIf
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
  [Parameter(Mandatory = $true)][string]$AdapterName,
  [string]$TempIP = "169.254.99.1",
  [int]$PrefixLength = 16
)
$ErrorActionPreference = "Stop"
# 1. 网卡存在性 + 基本状态
$ad = Get-NetAdapter -Name $AdapterName -ErrorAction Stop
Write-Host ("[check] 网卡: {0} 状态={1} 速率={2}" -f $ad.Name, $ad.Status, $ad.LinkSpeed)
# 2. 高级属性: Jumbo / 速率 / EEE / 中断合并 (不同驱动关键字不同，有啥打啥)
Write-Host "[check] 高级属性:"
Get-NetAdapterAdvancedProperty -Name $AdapterName | Format-Table DisplayName, DisplayValue, RegistryKeyword -AutoSize | Out-String | Write-Host
# 3. 当前 IP (期望无 IP)
$ips = Get-NetIPAddress -InterfaceAlias $AdapterName -AddressFamily IPv4 -ErrorAction SilentlyContinue
if ($ips) { Write-Warning ("该网卡当前有 IP: {0}，验完将只删本次临时 IP，原有 IP 不动。" -f ($ips.IPAddress -join ",")) }
else { Write-Host "[check] 当前无 IPv4 (符合数据面要求)" }
# 4. 临时配 IP -> ping -f -l 8972 -> finally 删 IP
$created = $null
try {
  if ($PSCmdlet.ShouldProcess("$AdapterName", "临时配 $TempIP/$PrefixLength 并 ping -f -l 8972")) {
    $created = New-NetIPAddress -InterfaceAlias $AdapterName -IPAddress $TempIP -PrefixLength $PrefixLength -ErrorAction Stop
    Write-Host ("[check] 已临时配 {0}/{1}，ping 粗验 (需对端配同网段 169.254.x.x)..." -f $TempIP, $PrefixLength)
    ping -n 4 -f -l 8972 169.254.99.2
    Write-Host "[check] 注意: ping 通只证三层 9K，网线真吞吐以 T1 为准。"
  } else { Write-Host "[check] WhatIf 干跑: 不配 IP、不 ping。" }
} finally {
  if ($created -and $PSCmdlet.ShouldProcess("$AdapterName", "删除临时 IP 还原无 IP")) {
    Remove-NetIPAddress -InputObject $created -Confirm:$false -ErrorAction SilentlyContinue
    Write-Host "[check] 已删临时 IP，还原无 IP 状态。"
  }
  $left = Get-NetIPAddress -InterfaceAlias $AdapterName -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object { $_.IPAddress -eq $TempIP }
  if ($left) { Write-Warning "临时 IP 仍在! 手动删: Remove-NetIPAddress" } else { Write-Host "[check] 确认临时 IP 已还原。" }
}
