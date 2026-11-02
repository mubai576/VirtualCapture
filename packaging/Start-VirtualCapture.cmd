@echo off
setlocal
chcp 65001 >nul
cd /d "%~dp0"
echo VirtualCapture
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\start-host.ps1" %*
if errorlevel 1 pause
