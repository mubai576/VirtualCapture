# watchdog.ps1 - backstop probe for vc-agent.
param([int]$Port = 5143, [string]$TaskName = "vc-agent", [int]$QuietMinutes = 10)
$listening = $false
try { $c = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction Stop; if ($c) { $listening = $true } } catch { }
$ok = $false
try { $h = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/health" -TimeoutSec 5; if ($h.ok) { $ok = $true } } catch { }
if ($ok) { Write-Host "watchdog: healthy"; exit 0 }
if ($listening) { Write-Host "watchdog: port busy but unhealthy, transient, skip"; exit 0 }
$root = Split-Path (Split-Path $PSCommandPath -Parent) -Parent
$ul = Join-Path $root "run/updates.log"
try { if (Test-Path $ul -PathType Leaf) { $age = (Get-Date) - (Get-Item $ul).LastWriteTime; if ($age.TotalMinutes -lt $QuietMinutes) { Write-Host "watchdog: recent update, skip"; exit 0 } } } catch { }
try { schtasks /run /tn $TaskName | Out-Null; if ($LASTEXITCODE -ne 0) { throw "schtasks rc" }; Write-Host "watchdog: restarted task"; exit 1 }
catch { Write-Warning "watchdog: restart failed"; exit 1 }
