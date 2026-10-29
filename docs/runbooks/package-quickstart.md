# VirtualCapture 便携目录使用说明

本目录包含双机采集/传输所需程序，以及预览、诊断、验收工具；将整个目录复制到两台 Windows x64 机器即可。
日常运行 agent 与 Web 控制台不需要安装 Python，MSVC 运行库已随包放在程序旁。
真实网卡收发仍需在目标机器安装 Npcap（WinPcap 兼容模式）并按项目 runbook 配好网卡。

## 启动

1. 双击 `Start-VirtualCapture.cmd`，保持控制服务窗口运行。
2. 浏览器打开 `http://127.0.0.1:18080/ui`。
3. 在两台机器各自的控制台填写采集/接收角色、对端地址、本机网卡与目标 MAC，再保存并开播。
4. 接收机按控制台提示注册虚拟采集卡，下游软件选择 `VirtualCapture`。操作细节见 `docs/runbooks/gui.md`。

也可在 PowerShell 中运行：`./vc-agent.exe --port 18080`。`vc-monitor.exe` 用于预览，`vc-testwin.exe` 是 DDA 动画测试源。
首次使用时 `vc-config.json`、`run/` 由程序创建；包中不含开发机配置或历史日志。
更新目录前先停止正在使用该目录的程序；保留自己的配置与运行证据。

## 文件

| 内容 | 路径 |
|---|---|
| 控制服务与内置 GUI | `vc-agent.exe` |
| 采集 / 接收 | `vc-sender.exe`、`vc-receiver.exe` |
| 虚拟采集卡 | `vc-vcam.dll` |
| 预览 / D3D 诊断 / 动画测试 | `vc-monitor.exe`、`vc-monitor-diag.exe`、`vc-testwin.exe` |
| DShow 枚举与格式验收 | `vc-vcam-verify.exe` |
| MSVC 运行库 | 根目录的 `msvcp*.dll`、`vcruntime*.dll`、`concrt*.dll` 等 |
| 运维与专项验收 | `scripts/` |
| 双机编排、对账、报告与文本监控 | `ctrl/` |
| 版本 / 源码来源 / 文件 SHA-256 | `version.json` / `package-manifest.json` |
| 项目说明与操作手册 | `docs/` |

可选的 Python 控制/分析脚本需要另外安装 Python；`ctrl/vc-monitor-tui.py` 还需要 `rich`（`python -m pip install rich`）。
运维脚本中的机器地址是参数或项目默认值，使用前按实际环境填写。

## 检查

在此目录运行 `./vc-sender.exe --selftest`、`./vc-receiver.exe --selftest` 可做本机自检。
文件完整性可按 `package-manifest.json` 内各相对路径的 SHA-256 核对。
这是完整便携目录；OTA `/update` 仅接受白名单文件，不能直接把整个目录压缩后作为 OTA 包上传。

需要开机任务安装时，以管理员 PowerShell 执行 `scripts/install-pc2.ps1` 并显式指定角色与安装目录。
普通手动运行不需要执行安装脚本。打包脚本本身不会注册虚拟卡、修改网卡或部署到其他机器。
