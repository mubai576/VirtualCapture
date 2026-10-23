# fetch_testwin.ps1 — download vc-testwin.exe from PC2 HTTP server and launch it
# Usage: POST /exec {"argv":["pwsh","-NoProfile","-ExecutionPolicy","Bypass","-File","scripts/fetch_testwin.ps1","http://192.168.2.100:18181/vc-testwin.exe"]}
param(
    [string]$Url = "http://192.168.2.100:18181/vc-testwin.exe"
)
$root   = Split-Path -Parent $PSScriptRoot
$outExe = Join-Path $root "build\Release\vc-testwin.exe"
New-Item -ItemType Directory -Force -Path (Split-Path $outExe) | Out-Null
Write-Output "Downloading from $Url ..."
Invoke-WebRequest -Uri $Url -OutFile $outExe -UseBasicParsing -ErrorAction Stop
Write-Output "Saved to $outExe  ($((Get-Item $outExe).Length) bytes)"
Start-Process -FilePath $outExe -WindowStyle Normal
Write-Output "Launched vc-testwin.exe"
