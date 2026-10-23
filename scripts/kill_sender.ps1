# kill_sender.ps1 — kill all vc-sender.exe processes
$procs = Get-Process vc-sender -ErrorAction SilentlyContinue
if ($procs) {
    $procs | ForEach-Object { Stop-Process -Id $_.Id -Force }
    Write-Output "killed $($procs.Count) vc-sender process(es)"
} else {
    Write-Output "no vc-sender running"
}
# verify
Start-Sleep -Milliseconds 500
$after = Get-Process vc-sender -ErrorAction SilentlyContinue
Write-Output "after kill: $($after.Count) remaining"
