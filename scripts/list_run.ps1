# list_run.ps1 — list recent files in run directory
$root = Split-Path -Parent $PSScriptRoot
$run  = Join-Path $root "run"
Write-Output "run dir: $run"
if (Test-Path $run) {
    Get-ChildItem $run -File | Sort-Object LastWriteTime -Descending | Select-Object -First 15 | ForEach-Object {
        Write-Output ("  {0,-50} {1,8}B  {2}" -f $_.Name, $_.Length, $_.LastWriteTime.ToString("HH:mm:ss"))
    }
} else {
    Write-Output "run dir not found"
}
# also check if sender process is running
$proc = Get-Process vc-sender -ErrorAction SilentlyContinue
if ($proc) { Write-Output ("sender running: pid={0}" -f $proc.Id) }
else { Write-Output "sender NOT running" }
