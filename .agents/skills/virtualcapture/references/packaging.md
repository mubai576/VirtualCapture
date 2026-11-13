# 便携目录打包契约（2026-10-29）

入口：`python scripts/package.py [--allow-dirty] [--output <目录>]`。
相对路径按仓库根解析，支持从其他工作目录调用脚本。默认输出 `dist/VirtualCapture/`。
仅支持 Windows x64 / 64 位 Python；依赖 Git、CMake、MSVC、pytest、PyInstaller。

流程：预检 → `scripts/verify.py`（完整构建/自检/pytest）→ CMake Runtime install（7 个原生目标与 MSVC DLL）
→ `packaging/vc-agent.spec` 冻结 agent → 拷贝脚本/文档 → 隔离目录成品冒烟 → 来源不变检查 → 清单 → 发布目录。
GUI 内置于 agent，不另拷网页资源。
成品 `.ps1` 统一为 UTF-8 BOM / CRLF，发布前使用 Windows PowerShell 5.1 解析全部脚本并检查便携目录程序查找。

产物根目录：`vc-agent.exe`、`vc-sender.exe`、`vc-receiver.exe`、`vc-vcam.dll`、`vc-testwin.exe`、
`vc-monitor.exe`、`vc-monitor-diag.exe`、`vc-vcam-verify.exe`、MSVC runtime DLL、`version.json`、
`Start-VirtualCapture.cmd`、`Start-Capture-Worker.cmd`、`README.md`、`package-manifest.json`；子目录 `ctrl/`、`scripts/`、`docs/`。
只打包 ctrl/scripts 顶层运行脚本和 docs 下 Markdown；排除 tests、缓存、构建入口、历史取证脚本、机器配置与运行证据。
Python 控制/分析脚本是可选工具，需目标机 Python；TUI 另需 rich。主 agent / GUI / C++ 程序不依赖目标机 Python。
Npcap 驱动不随目录复制，目标机实际收发前需安装 Npcap。
MJPEG静态链接libjpeg-turbo，Runtime install另带`licenses/libjpeg-turbo-LICENSE.md`和`licenses/libjpeg-turbo-README.ijg`；缺任一原文打包失败。
源文档`docs/runbooks/mjpeg.md`包含IJG归属声明、依赖版本/固定摘要及离线缓存操作，无新增运行时DLL。

主机入口调用 `scripts/start-host.ps1`，采集后台入口调用 `scripts/start-capture-worker.ps1`；两者共用 `start-agent.ps1`，隐藏启动，默认端口 5143，等待健康检查。同目录同角色服务复用；角色、目录或能力冲突则拒绝。源码目录可使用相同脚本启动 Python agent，主机支持 `-NoBrowser`。日志写入运行目录的 `run/agent-*.stdout.log|stderr.log`。
冻结成品冒烟同时验证主机 GUI 和采集后台：校验 `/health` 角色、版本与 root，后台的 `/`、`/ui`、`/ui/` 必须返回 404。
冒烟进程树退出后，临时目录清理遇到 Windows 文件占用会在 10 秒内重试；超过期限仍失败则不发布，不跳过清理错误。

保护与 provenance：
- 工作区脏时默认拒绝；owner 已确认可包含当前改动后用 `--allow-dirty`。2026-10-29 owner 已明确授权本次包含 GUI 等当前工作区全部功能。
- 清单记录版本、UTC 构建时间、Git revision、dirty 状态、逐源文件 SHA-256 与逐产物 SHA-256。
- 构建期间源码发生变化则失败，不替换旧成品。
- 仓库内仅允许输出到 `dist/` 的子目录；已存在目录须带匹配的成品清单标记。
- 成功时旧目录（含其中用户配置/运行证据）整体移入 `<输出目录名>.archive/`；新目录重命名失败则还原旧目录。
- 日志在 `build/package/package-*.log`。临时构建内容不进入成品。
- 打包不注册 DLL、不修改机器配置、不启动真实采集/传输、不执行 OTA。

这是完整便携目录，**不是 OTA zip**；现有 `/update` 白名单与 stage/activate 契约不变。
