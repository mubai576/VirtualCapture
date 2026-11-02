<#
.SYNOPSIS
  install-pc2.ps1 — PC2(零环境)现场安装: Npcap检查 -> 文件落盘 -> 计划任务自启 -> 防火墙 -> /health验证。
.DESCRIPTION
  在 PC2 上用管理员 pwsh 从发布包解压目录运行:
    pwsh -ExecutionPolicy Bypass -File install-pc2.ps1 -Role receiver -Port 5143
  发布包内含 vc-agent.exe(单文件, 无需 Python), vc-sender/receiver.exe, version.json。
  agent 以 ROOT=exe 所在目录运行: C:\vc\vc-agent.exe + C:\vc\build\vc-{sender,receiver}.exe
  + C:\vc\version.json + C:\vc\run\(自动建)。用 --root 可覆盖 ROOT。
.PARAMETER Role
  sender | receiver (PC2 一般填 receiver)。
.PARAMETER Port
  agent 端口, 默认 5143。
.PARAMETER InstallDir
  安装目录, 默认 C:\vc。
.PARAMETER PackageDir
  发布包根目录, 默认 scripts 的上一级目录。
.PARAMETER ControlSubnet
  控制面放行网段, 默认 192.168.2.0/24。
.PARAMETER TaskName
  计划任务与防火墙规则名前缀, 默认 vc-agent；并存验证时改名（如 vc-agent-38）。
.EXAMPLE
  pwsh -ExecutionPolicy Bypass -File install-pc2.ps1 -Role receiver -WhatIf
  pwsh -ExecutionPolicy Bypass -File install-pc2.ps1 -Role receiver -Port 18081 -InstallDir C:\vc38 -TaskName vc-agent-38
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
  [Parameter(Mandatory = $true)][ValidateSet("sender", "receiver")][string]$Role,
  [int]$Port = 5143,
  [string]$InstallDir = "C:\vc",
  [string]$PackageDir = (Split-Path $PSScriptRoot -Parent),
  [string]$ControlSubnet = "192.168.2.0/24",
  [string]$TaskName = "vc-agent"
)
$ErrorActionPreference = "Stop"
# 0. 管理员检查(写 C 盘/计划任务/防火墙都要管理员)
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { Write-Warning "非管理员: 拷贝/计划任务/防火墙步骤会失败, 请右键-以管理员身份运行 pwsh。" }
# 1. Npcap 运行时检查(wpcap.dll 必须存在, 否则 exe 能启动但收发失败)
$wpcap = @("C:\Windows\System32\wpcap.dll", "C:\Windows\SysWOW64\wpcap.dll") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $wpcap) { Write-Error "未找到 wpcap.dll: 请先装 Npcap(官网, 勾 WinPcap 兼容模式) 再重跑本脚本。" }
Write-Host ("[install] Npcap OK: {0}" -f $wpcap)
# 2. 发布包文件检查
foreach ($f in @("vc-agent.exe", "vc-sender.exe", "vc-receiver.exe", "version.json")) {
  if (-not (Test-Path (Join-Path $PackageDir $f))) { Write-Error ("发布包缺文件: {0}" -f $f) }
}
$ver = (Get-Content (Join-Path $PackageDir "version.json") -Raw | ConvertFrom-Json).version
Write-Host ("[install] package version={0} role={1} port={2} dir={3}" -f $ver, $Role, $Port, $InstallDir)
# 3. 落盘: exe->C:\vc, sender/receiver->C:\vc\build(与 agent 的 BUILD_DIR 约定一致)
if ($PSCmdlet.ShouldProcess($InstallDir, "拷贝文件")) {
  New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
  New-Item -ItemType Directory -Path (Join-Path $InstallDir "build") -Force | Out-Null
  New-Item -ItemType Directory -Path (Join-Path $InstallDir "run") -Force | Out-Null
  # Include every product/tool binary and the app-local MSVC runtime.
  Get-ChildItem -LiteralPath $PackageDir -File | Where-Object {
    $_.Extension -in @('.exe', '.dll', '.cmd') -or $_.Name -in @('README.md', 'package-manifest.json')
  } | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $InstallDir -Force }
  Copy-Item (Join-Path $PackageDir "version.json") $InstallDir -Force
  Copy-Item (Join-Path $PackageDir "vc-sender.exe") (Join-Path $InstallDir "build") -Force
  Copy-Item (Join-Path $PackageDir "vc-receiver.exe") (Join-Path $InstallDir "build") -Force
  if (Test-Path (Join-Path $PackageDir "scripts")) {
    New-Item -ItemType Directory -Path (Join-Path $InstallDir "scripts") -Force | Out-Null
    Copy-Item (Join-Path $PackageDir "scripts/*") (Join-Path $InstallDir "scripts") -Force
  }
  foreach ($support in @('ctrl', 'docs')) {
    if (Test-Path -LiteralPath (Join-Path $PackageDir $support)) {
      New-Item -ItemType Directory -Path (Join-Path $InstallDir $support) -Force | Out-Null
      Copy-Item -Path (Join-Path $PackageDir "$support/*") -Destination (Join-Path $InstallDir $support) -Recurse -Force
    }
  }
  Write-Host "[install] 文件落盘完成。"
}
# 4. 计划任务开机自启(SYSTEM, 最高权限)
$taskName = $TaskName
$exe = Join-Path $InstallDir "vc-agent.exe"
$args = "--role $Role --port $Port"
if ($PSCmdlet.ShouldProcess($taskName, "创建开机自启计划任务")) {
  schtasks /create /tn $taskName /tr "`"$exe`" $args" /sc onstart /ru SYSTEM /rl highest /f
  Write-Host "[install] 计划任务已建: $taskName"
}
# 5. 防火墙仅放行控制面网段
if ($PSCmdlet.ShouldProcess("firewall", "放行 TCP $Port 给 $ControlSubnet")) {
  netsh advfirewall firewall delete rule name="$taskName" | Out-Null
  netsh advfirewall firewall add rule name="$taskName" dir=in action=allow protocol=TCP localport=$Port remoteip=$ControlSubnet | Out-Null
  Write-Host "[install] 防火墙已放行 $Port 给 $ControlSubnet。"
}
# 5b. watchdog backstop (SYSTEM, every 5 min).
$wdScript = Join-Path $InstallDir "scripts/watchdog.ps1"
if ($PSCmdlet.ShouldProcess(($taskName + '-watchdog'), 'create probe task')) {
  $wdTr = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"" + $wdScript + "`" -Port " + $Port
  schtasks /create /tn ($taskName + '-watchdog') /tr $wdTr /sc minute /mo 5 /ru SYSTEM /rl highest /f
  Write-Host "[install] watchdog task created."
}
# 6. 启动并验证 /health + /version
if ($PSCmdlet.ShouldProcess($taskName, "启动并验证")) {
  schtasks /run /tn $taskName | Out-Null
  $h = $null
  for ($i = 0; $i -lt 15 -and -not $h; $i++) {
    Start-Sleep 2
    try { $h = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/health" -TimeoutSec 5 } catch { }
  }
  if (-not $h) { Write-Error "agent 30s not ready" }
  $v = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/version" -TimeoutSec 10
  Write-Host ("[install] health: {0}" -f ($h | ConvertTo-Json -Compress))
  Write-Host ("[install] version: {0} (发布包 {1})" -f $v.version, $ver)
  if ($v.version -ne $ver) { Write-Warning "agent 版本与发布包不一致, 检查是否旧任务残留。" }
  else { Write-Host "[install] 完成。控制机验证: curl http://<本机控制面IP>:$Port/health" }
}
