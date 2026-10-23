# start_testwin.ps1 — launch vc-testwin.exe detached (for DDA capture test)
# Called via agent /exec: pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/start_testwin.ps1
$root = Split-Path -Parent $PSScriptRoot
$exe  = Join-Path $root "build\Release\vc-testwin.exe"
if (-not (Test-Path $exe)) { Write-Error "vc-testwin.exe not found: $exe"; exit 1 }
Start-Process -FilePath $exe -WindowStyle Normal
Write-Output "launched: $exe"
