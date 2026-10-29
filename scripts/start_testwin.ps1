# start_testwin.ps1 — launch vc-testwin.exe detached (for DDA capture test)
# Called via agent /exec: pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/start_testwin.ps1
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'runtime-paths.ps1')
$exe = Get-VcBinaryPath 'vc-testwin.exe'
Start-Process -FilePath $exe -WindowStyle Normal
Write-Output "launched: $exe"
