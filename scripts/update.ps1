<#
.SYNOPSIS
  update.ps1 — 推版本到双机 agent (T-05)。调双端 POST /update，轮询 /version 一致才退出 0。
.DESCRIPTION
  MVP: 版本以 version.json 为准，zip 推包待 agent /updateBinary 落地后启用；
  当前先比对双端 /version，一致才算成功 (避免跨版本对账错位)。
.PARAMETER VersionZip
  版本 zip 路径 (当前仅归档记录，实际比对走 /version)。
.PARAMETER SenderIP
  PC1 控制面 IP (192.168.2.x)。
.PARAMETER ReceiverIP
  PC2 控制面 IP。
.PARAMETER Port
  agent 端口，默认 18080。
.EXAMPLE
  .\scripts\update.ps1 -VersionZip .\vc-0.1.0.zip -SenderIP 192.168.2.100 -ReceiverIP 192.168.2.59
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
  [string]$VersionZip = "",
  [Parameter(Mandatory = $true)][string]$SenderIP,
  [Parameter(Mandatory = $true)][string]$ReceiverIP,
  [int]$Port = 18080,
  [int]$TimeoutS = 60
)
$ErrorActionPreference = "Stop"
$sender = "http://${SenderIP}:${Port}"
$receiver = "http://${ReceiverIP}:${Port}"
# 1. 干跑提示
if (-not $PSCmdlet.ShouldProcess("$sender, $receiver", "POST /update 并轮询 /version")) {
  Write-Host "[update] WhatIf 干跑: 不发包。"; exit 0
}
# 2. 读本地期望版本 (zip 名或 version.json)
$expected = ""
if ($VersionZip -and (Test-Path $VersionZip)) {
  Write-Host (("[update] version package: {0}" -f $VersionZip))
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  $zf = [System.IO.Compression.ZipFile]::OpenRead((Resolve-Path $VersionZip).Path)
  $ve = $zf.Entries | Where-Object { $_.FullName -eq "version.json" } | Select-Object -First 1
  if ($ve) { $sr = New-Object System.IO.StreamReader($ve.Open()); $expected = ($sr.ReadToEnd() | ConvertFrom-Json).version; $sr.Close() }
  $zf.Dispose()
  Write-Host (("[update] expected version: {0}" -f $expected))
}
# 3. 推包: 版本 zip 二进制 POST 到双端 /update(原子替换+自重启)
foreach ($base in @($sender, $receiver)) {
# 2b. updater preflight: refuse when helper channel is not ps-ok
foreach ($ep in @($sender, $receiver)) {
  try {
    $h = Invoke-RestMethod -Uri "$ep/health" -TimeoutSec 10
    if ($h.updater -ne "ps-ok") { Write-Error "UPDATER-NOT-READY"; exit 1 }
    Write-Host "GATE-OK"
  } catch { Write-Error "HEALTH-CHECK-FAILED"; exit 1 }
}
  Write-Host (("[update] POST {0}/update ..." -f $base))
  if ($VersionZip -and (Test-Path $VersionZip)) {
    try {
      $bytes = [System.IO.File]::ReadAllBytes((Resolve-Path $VersionZip).Path)
      $req = [System.Net.HttpWebRequest]::Create("$base/update")
      $req.Method = "POST"; $req.ContentType = "application/zip"; $req.Timeout = 60000
      $rs = $req.GetRequestStream(); $rs.Write($bytes, 0, $bytes.Length); $rs.Close()
      $resp = New-Object System.IO.StreamReader($req.GetResponse().GetResponseStream())
      Write-Host ($resp.ReadToEnd()); $resp.Close()
    } catch { Write-Warning (("POST failed, poll phase decides: {0}" -f $_.Exception.Message)) }
  } else {
    Write-Warning "VersionZip missing, skip push, version check only."
  }
}
$deadline = (Get-Date).AddSeconds($TimeoutS)
while ($true) {
  try {
    $sv = (Invoke-RestMethod -Uri "$sender/version" -TimeoutSec 10).version
    $rv = (Invoke-RestMethod -Uri "$receiver/version" -TimeoutSec 10).version
    Write-Host ("[update] sender={0} receiver={1}" -f $sv, $rv)
    if ($expected -and ($sv -eq $expected) -and ($rv -eq $expected)) { Write-Host "[update] both at expected, PASS."; exit 0 }
    elseif (-not $expected -and $sv -and ($sv -eq $rv)) { Write-Host "[update] versions agree."; exit 0 }
  } catch { Write-Warning ("轮询失败: {0}" -f $_.Exception.Message) }
  if ((Get-Date) -gt $deadline) { Write-Error "版本不一致或超时，拒绝放行。"; exit 1 }
  Start-Sleep 3
}
