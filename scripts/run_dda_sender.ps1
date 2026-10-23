# run_dda_sender.ps1 — download new sender + start DDA+gpu-convert (detached)
param(
    [string]$PC2     = "http://192.168.2.100:18181",
    [string]$DstMac  = "34:5a:60:4f:f2:33",
    [string]$Session = "p4final"
)
$root   = Split-Path -Parent $PSScriptRoot
$build  = Join-Path $root "build"
$run    = Join-Path $root "run"
$exe    = Join-Path $build "vc-sender.exe"
New-Item -ItemType Directory -Force -Path $build | Out-Null
New-Item -ItemType Directory -Force -Path $run   | Out-Null

# 1. Download latest sender binary from PC2 HTTP server
Write-Output "Downloading vc-sender.exe from $PC2 ..."
Invoke-WebRequest -Uri "$PC2/vc-sender.exe" -OutFile "$exe.tmp" -UseBasicParsing -ErrorAction Stop
Move-Item -Force "$exe.tmp" $exe
$sz = (Get-Item $exe).Length
Write-Output "Updated: $exe ($sz bytes)"

# 2. Launch sender with --gpu-convert (detached, hidden window)
$argList = @(
    "--adapter", "Realtek",
    "--mode",    "dda",
    "--gpu-convert",
    "--batch",   "69",
    "--dst-mac", $DstMac,
    "--session", $Session,
    "--stats",   "$run\sender-$Session.json",
    "--hashlog", "$run\sender-$Session.hash.jsonl",
    "--frames",  "0"
)
Start-Process -FilePath $exe -ArgumentList $argList -WorkingDirectory $root -WindowStyle Hidden
Write-Output "Sender launched (dda + gpu-convert), stats: $run\sender-$Session.json"
