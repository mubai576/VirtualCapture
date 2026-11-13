# VirtualCapture 便携目录使用说明

本目录包含双机采集/传输所需程序，以及预览、诊断、验收工具；将整个目录复制到两台 Windows x64 机器即可。
日常运行 agent 与 Web 控制台不需要安装 Python，MSVC 运行库已随包放在程序旁。
真实网卡收发仍需在目标机器安装 Npcap（WinPcap 兼容模式）并按项目 runbook 配好网卡。

## 启动

1. 主机双击 `Start-VirtualCapture.cmd`，打开 `http://127.0.0.1:5143/ui`。
2. 被采集机在登录桌面用户下双击 `Start-Capture-Worker.cmd`，隐藏启动后台；无需浏览器或额外配置。
3. 主机只填写被采集机 IP，从自动获取的列表选择本机网卡，保存后点击“开始采集”。
4. 接收开始时自动注册虚拟采集卡，下游选择 `VirtualCapture`；同一按钮切换为“停止采集”，停止后自动回收。

也可运行 `./vc-agent.exe --role host`（主机）或 `./vc-agent.exe --role capture`（后台），默认端口 5143。
`vc-monitor.exe` 用于预览，`vc-testwin.exe` 是 DDA 动画测试源。详见 `docs/runbooks/gui.md`。
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

`scripts/install-pc2.ps1` 保留历史 SYSTEM 计划任务部署；DDA 采集及主机虚拟卡需要登录用户会话，产品使用上面的启动方式。
普通手动运行不需要执行安装脚本。打包脚本本身不会注册虚拟卡、修改网卡或部署到其他机器。

## P6 开发模式与 MJPEG

默认仍为 Raw。MJPEG 须两端使用匹配的新程序，并在 GUI 显式允许开发编码模式；未支持或版本不匹配直接拒绝，不自动回退。H.264/H.265 已按 D23 移除。
MJPEG选择CPU/SIMD后端`turbojpeg:cpu`，GOP1，可调JPEG质量1..100（默认80），有损而非Raw逐像素相等。
目标码率在MJPEG下不适用，最大码率仍限制发送：640×640@240/Q80本机motion约需141Mbps，原80Mbps上限会限流；应按内容与链路预算显式调整，不能直接保证240fps。
本轮不自动部署或切换；若5143已有其他目录的agent，启动器会拒绝复用，不要为试新包随意关闭稳定后台。
构建、许可证和测试边界见`docs/runbooks/mjpeg.md`，双机并存/切换须按`docs/runbooks/p6-cutover.md`另行确认。
