<#
.SYNOPSIS
  update.ps1 — 推版本到双机 agent (P3)。双端 stage→activate→poll /version 一致才退出 0；无 detached 进程。
.DESCRIPTION
  P3 流程: POST zip 到 /update 只做 stage(校验+解压+prev 快照，不重启)；
  再 POST /activate 原地 execv 接管，最后轮询双端 /version 一致才算成功。
.PARAMETER VersionZip
  版本 zip 路径 (必含 version.json；stage 用；缺省只比对双端版本)。
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
if (-not $PSCmdlet.ShouldProcess($sender + ', ' + $receiver, 'stage/activate 并轮询 /version')) {
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
# 3. stage: 版本 zip 二进制 POST 到双端 /update(只校验+解压+prev 快照，不重启)
foreach ($base in @($sender, $receiver)) {
  try {
    $h = Invoke-RestMethod -Uri ($base + '/health') -TimeoutSec 10
    if ($h.updater -ne 'ps-ok') { Write-Error 'UPDATER-NOT-READY'; exit 1 }
    Write-Host 'GATE-OK'
  } catch { Write-Error 'HEALTH-CHECK-FAILED'; exit 1 }
  Write-Host ('[update] stage ' + $base + '/update ...')
  if ($VersionZip -and (Test-Path $VersionZip)) {
    try {
      $bytes = [System.IO.File]::ReadAllBytes((Resolve-Path $VersionZip).Path)
      $req = [System.Net.HttpWebRequest]::Create($base + '/update')
      $req.Method = 'POST'; $req.ContentType = 'application/zip'; $req.Timeout = 60000
      $rs = $req.GetRequestStream(); $rs.Write($bytes, 0, $bytes.Length); $rs.Close()
      $resp = New-Object System.IO.StreamReader($req.GetResponse().GetResponseStream())
      $staged = $resp.ReadToEnd(); $resp.Close()
      Write-Host $staged
      if (($staged -notmatch 'staged') -or ($expected -and ($staged -notmatch [regex]::Escape($expected)))) { Write-Error ('STAGE-REJECTED: ' + $staged); exit 1 }
    } catch { Write-Warning ('stage 失败，终止(不进 activate): ' + $_.Exception.Message); exit 1 }
  } else {
    Write-Warning 'VersionZip missing, skip push, version check only.'
  }
}
if ($VersionZip -and (Test-Path $VersionZip) -and $expected) {
foreach ($base in @($sender, $receiver)) {
  Write-Host ('[update] activate ' + $base + '/activate ...')
  $qq = [char]34
  $body = ('{' + $qq + 'version' + $qq + ':' + $qq + $expected + $qq + '}')
  try {
    $r = Invoke-RestMethod -Uri ($base + '/activate') -Method Post -ContentType 'application/json' -Body $body -TimeoutSec 60
    Write-Host ('[update] activate 应答: ' + ($r | ConvertTo-Json -Compress))
  } catch { Write-Warning ('activate POST 异常(可能已 execv，轮询裁决): ' + $_.Exception.Message) }
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
