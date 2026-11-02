# 主机和采集后台共用入口：校验已有服务，隐藏启动，等待健康检查。
[CmdletBinding()]
param(
  [ValidateSet('host', 'capture')][string]$Role = 'host',
  [string]$Root = ''
)
$ErrorActionPreference = 'Stop'
if (-not $Root) { $Root = Split-Path -Parent $PSScriptRoot }
$Root = (Resolve-Path -LiteralPath $Root).Path
$expectedRole = if ($Role -eq 'host') { 'receive' } else { 'capture' }
function Test-VcService($Service) {
  if ($Service.product_role -ne $expectedRole -or $Service.product_revision -ne 2 -or
      -not $Service.root -or [IO.Path]::GetFullPath($Service.root) -ne [IO.Path]::GetFullPath($Root)) {
    throw '5143 端口已有其他角色、目录或旧版服务，请先关闭该服务再启动。'
  }
}
try { $health = Invoke-RestMethod -Uri 'http://127.0.0.1:5143/health' -TimeoutSec 2 }
catch { $health = $null }
if ($health) {
  Test-VcService $health
  Write-Output ('服务已经运行，角色=' + $Role + '，PID=' + $health.pid)
  return
}
$agentExe = Join-Path $Root 'vc-agent.exe'
if (Test-Path -LiteralPath $agentExe) {
  $agentArgs = @('--role', $Role, '--root', ('"' + $Root + '"'))
} else {
  $source = Join-Path $Root 'agent/vc-agent.py'
  if (-not (Test-Path -LiteralPath $source)) { throw '未找到 vc-agent.exe 或 agent/vc-agent.py。' }
  $agentExe = (& python -c 'import sys; print(sys.executable)').Trim()
  if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $agentExe)) { throw '未找到 Python。' }
  $agentArgs = @(('"' + $source + '"'), '--role', $Role, '--root', ('"' + $Root + '"'))
}
$logDir = Join-Path $Root 'run'
New-Item -ItemType Directory -Path $logDir -Force | Out-Null
$logName = 'agent-' + $Role + '-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff')
$stdoutLog = Join-Path $logDir ($logName + '.stdout.log')
$stderrLog = Join-Path $logDir ($logName + '.stderr.log')
$agentProcess = Start-Process -FilePath $agentExe -ArgumentList $agentArgs -WorkingDirectory $Root -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog
for ($attempt = 0; $attempt -lt 60; $attempt++) {
  Start-Sleep -Milliseconds 250
  if ($agentProcess.HasExited) { throw ('后台启动失败，请检查 ' + $stderrLog) }
  try { $health = Invoke-RestMethod -Uri 'http://127.0.0.1:5143/health' -TimeoutSec 1 }
  catch { continue }
  try { Test-VcService $health }
  catch {
    if (-not $agentProcess.HasExited) { Stop-Process -Id $agentProcess.Id -ErrorAction SilentlyContinue }
    throw
  }
  Write-Output ('后台已启动，角色=' + $Role + '，端口=5143，PID=' + $health.pid)
  return
}
if (-not $agentProcess.HasExited) { Stop-Process -Id $agentProcess.Id -ErrorAction SilentlyContinue }
throw ('后台未就绪，请检查 ' + $stderrLog)
