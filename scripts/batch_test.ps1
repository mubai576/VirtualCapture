# batch_test.ps1 — start DDA sender with specified batch for latency test
param(
    [int]$Batch    = 69,
    [string]$DstMac  = "34:5a:60:4f:f2:33",
    [string]$Session = "btest69"
)
$root  = Split-Path -Parent $PSScriptRoot
$exe   = Join-Path $root "build\vc-sender.exe"
$run   = Join-Path $root "run"
New-Item -ItemType Directory -Force -Path $run | Out-Null

if (-not (Test-Path $exe)) {
    Write-Error "sender not found: $exe"; exit 1
}

$argList = @(
    "--adapter", "Realtek",
    "--mode",    "dda",
    "--gpu-convert",
    "--batch",   "$Batch",
    "--dst-mac", $DstMac,
    "--session", $Session,
    "--stats",   "$run\sender-$Session.json",
    "--hashlog", "$run\sender-$Session.hash.jsonl",
    "--frames",  "0"
)
Start-Process -FilePath $exe -ArgumentList $argList -WorkingDirectory $root -WindowStyle Hidden
Write-Output "batch=$Batch session=$Session stats=$run\sender-$Session.json"
