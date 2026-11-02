# 被采集机只启动后台，不打开界面。
[CmdletBinding()]
param([string]$Root = '')
$ErrorActionPreference = 'Stop'
if (-not $Root) { $Root = Split-Path -Parent $PSScriptRoot }
& (Join-Path $PSScriptRoot 'start-agent.ps1') -Role capture -Root $Root
