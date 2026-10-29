@echo off
setlocal
chcp 65001 >nul
cd /d "%~dp0"
echo VirtualCapture
echo 启动后在浏览器打开 http://127.0.0.1:18080/ui
echo 保持此窗口运行。关闭窗口会停止本机控制服务。
"%~dp0vc-agent.exe" %*
if errorlevel 1 pause
