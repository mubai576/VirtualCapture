# update_sender.ps1 — download new vc-sender.exe from PC2 and replace
param([string]$Url = "http://192.168.2.100:18181/vc-sender.exe")
$root   = Split-Path -Parent $PSScriptRoot
$outExe = Join-Path $root "build\vc-sender.exe"
New-Item -ItemType Directory -Force -Path (Split-Path $outExe) | Out-Null
Write-Output "Downloading sender from $Url ..."
Invoke-WebRequest -Uri $Url -OutFile $outExe -UseBasicParsing -ErrorAction Stop
Write-Output "Done: $outExe ($((Get-Item $outExe).Length) bytes)"
