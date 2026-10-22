<#
.SYNOPSIS
  run_T1.ps1 — 一键 T1 linked 60s (T-05)。调 vc-ctrl 跑 motion 240Hz 并回显判分。
.PARAMETER SenderIP
  PC1 控制面 IP。
.PARAMETER ReceiverIP
  PC2 控制面 IP。
.PARAMETER Port
  agent 端口，默认 18080。
.EXAMPLE
  .\scripts\run_T1.ps1 -SenderIP 192.168.2.100 -ReceiverIP 192.168.2.59 -Adapter I226 -Batch 69
  .\scripts\run_T1.ps1 -SenderIP 127.0.0.1 -ReceiverIP 127.0.0.1 -Port 18081 -Seconds 10 -Adapter Loopback -Batch 1
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
  [Parameter(Mandatory = $true)][string]$SenderIP,
  [Parameter(Mandatory = $true)][string]$ReceiverIP,
  [Parameter(Mandatory = $true)][string]$Adapter,
  [int]$Port = 18080,
  [int]$Seconds = 60,
  [string]$Pattern = "motion",
  [string]$DstMac = "",
  [int]$Batch = 0
)
$ErrorActionPreference = "Stop"
$root = Split-Path (Split-Path $PSCommandPath -Parent) -Parent
if (-not $PSCmdlet.ShouldProcess("$SenderIP,$ReceiverIP", "跑 T1 ${Seconds}s pattern=$Pattern")) {
  Write-Host "[T1] WhatIf 干跑: 不跑。"; exit 0
}
# 1. pattern 自检 (无硬件可跑)
Write-Host "[T1] 1/2 pattern 自检..."
python "$root/ctrl/pattern_ref.py"
if ($LASTEXITCODE -ne 0) { Write-Error "pattern 自检失败"; exit 1 }
# 2. vc-ctrl T1
Write-Host "[T1] 2/2 vc-ctrl linked..."
$extra = @("--adapter", $Adapter)
if ($DstMac -ne "") { $extra += @("--dst-mac", $DstMac) }
if ($Batch -gt 0) { $extra += @("--batch", $Batch) }
python "$root/ctrl/vc-ctrl.py" --sender "http://${SenderIP}:${Port}" --receiver "http://${ReceiverIP}:${Port}" --seconds $Seconds --pattern $Pattern --case T1 @extra
exit $LASTEXITCODE
