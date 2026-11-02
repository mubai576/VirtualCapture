[CmdletBinding()]
param([string]$Root = '', [switch]$NoBrowser)
$ErrorActionPreference = 'Stop'
if (-not $Root) { $Root = Split-Path -Parent $PSScriptRoot }
& (Join-Path $PSScriptRoot 'start-agent.ps1') -Role host -Root $Root
if (-not $NoBrowser) { Start-Process 'http://127.0.0.1:5143/ui' }
