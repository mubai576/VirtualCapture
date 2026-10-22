<#
.SYNOPSIS
  netinfo.ps1 — 网卡查询 + 数据面整备 (T-06)。默认只读; 改动全进 WhatIf。
.DESCRIPTION
  空参: 列全部网卡 (Name/Status/LinkSpeed/Mac/IP) + 关键高级属性
  (Jumbo/EEE/中断合并/速率双工), 末尾输出 JSON 摘要供自动化解析。
  -AdapterName <名>: 只看一块网卡的详情。
  -Enable: 启用该网卡 (需管理员)。
  -Jumbo <值>: 设巨帧, 如 9014 (需管理员, 驱动关键字不同会 warning)。
  -DisableEEE: 尝试关节能以太网 (值按驱动常见写法试, 不成 warning)。
  现场用法 (PC2, 管理员 pwsh, 先只读):
    pwsh -File netinfo.ps1
    pwsh -File netinfo.ps1 -AdapterName "以太网" -Jumbo 9014 -DisableEEE -WhatIf
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
  [string]$AdapterName = "",
  [switch]$Enable,
  [int]$Jumbo = 0,
  [switch]$DisableEEE
)
$ErrorActionPreference = "Stop"
function Show-Adapters($list) {
  $list | Format-Table Name, Status, LinkSpeed, MacAddress, InterfaceIndex -AutoSize | Out-String | Write-Host
  foreach ($a in $list) {
    $ips = Get-NetIPAddress -InterfaceIndex $a.InterfaceIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue |
      Select-Object IPAddress, PrefixLength
    Write-Host ("  [{0}] IPv4: {1}" -f $a.Name, (($ips | ForEach-Object { "$($_.IPAddress)/$($_.PrefixLength)" }) -join ", "))
    Get-NetAdapterAdvancedProperty -InterfaceDescription $a.InterfaceDescription -ErrorAction SilentlyContinue |
      Where-Object { $_.RegistryKeyword -like "*Jumbo*" -or $_.RegistryKeyword -like "*EEE" -or
                     $_.RegistryKeyword -like "*Moderation" -or $_.RegistryKeyword -like "*SpeedDuplex" } |
      Format-Table DisplayName, DisplayValue, RegistryKeyword -AutoSize | Out-String | Write-Host
  }
}
if ($AdapterName) { $adapters = @(Get-NetAdapter -Name $AdapterName -ErrorAction Stop) }
else { $adapters = @(Get-NetAdapter -ErrorAction Stop) }
Show-Adapters $adapters
$changed = @()
foreach ($a in $adapters) {
  if ($Enable -and ($a.Status -ne "Up")) {
    if ($PSCmdlet.ShouldProcess($a.Name, "启用网卡")) {
      try { Enable-NetAdapter -Name $a.Name -Confirm:$false -ErrorAction Stop; $changed += "enabled" }
      catch { Write-Warning ("启用失败 {0}: {1}" -f $a.Name, $_.Exception.Message) }
    }
  }
  if ($Jumbo -gt 0) {
    if ($PSCmdlet.ShouldProcess($a.Name, "设 Jumbo=$Jumbo")) {
      try { Set-NetAdapterAdvancedProperty -Name $a.Name -RegistryKeyword "*JumboPacket" -RegistryValue $Jumbo -ErrorAction Stop; $changed += "jumbo=$Jumbo" }
      catch { Write-Warning ("Jumbo 设置失败 {0}: {1} (换驱动面板手动改)" -f $a.Name, $_.Exception.Message) }
    }
  }
  if ($DisableEEE) {
    if ($PSCmdlet.ShouldProcess($a.Name, "关 EEE")) {
      $done = $false
      foreach ($v in @("Disabled", 0)) {
        try { Set-NetAdapterAdvancedProperty -Name $a.Name -RegistryKeyword "*EEE" -RegistryValue $v -ErrorAction Stop; $changed += "eee=off"; $done = $true; break }
        catch { }
      }
      if (-not $done) { Write-Warning ("EEE 关闭失败 {0}: 驱动面板手动改" -f $a.Name) }
    }
  }
}
$summary = @{"adapters" = @($adapters | ForEach-Object { @{"name" = $_.Name; "status" = "$($_.Status)"; "speed" = "$($_.LinkSpeed)"} }); "changed" = $changed}
Write-Host ($summary | ConvertTo-Json -Compress -Depth 3)
