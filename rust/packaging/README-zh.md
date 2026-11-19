# VirtualCapture Rust 绿色开发版

Windows 11 x64，Rust 控制面 0.2.1，GPUI 采集工作台。产品数据面使用 Rust 模块：
`vc_stream.dll` 提供进程内 Raw/MJPEG/PyroWave 采集/传输 ABI，`vc_vcam.dll` 提供 Rust DirectShow 虚拟摄像头。
本包不包含旧 C++ 产品的 `vc-sender.exe`、`vc-receiver.exe` 或 `vc-vcam.dll`，也没有静默回退路径。
Raw 是默认编码；MJPEG/PyroWave 必须显式启用并匹配后端，未声明的组合会被明确拒绝。

## 使用

将整个目录解压到本地可写目录，两台电脑各放一份。无需安装 Rust、Python、Visual Studio 或单独的
VC++ 运行库；所需 MSVC DLL 已放在程序旁。实际枚举网卡/收发前，两端必须安装 Npcap
（https://npcap.com/#download）；内核驱动不随 ZIP 分发。需要正常 Windows 图形驱动；
使用 PyroWave 时两端驱动须支持 Vulkan 1.3，`vulkan-1.dll` 由显卡驱动提供，不在包内。

1. **被采集机**：双击 `Start-Capture.cmd`。这是无 GUI 的 CLI 后台，保留控制台，无参数启动时监听
   `0.0.0.0:15143`。保持窗口打开，按 Ctrl+C 正常退出；如 CMD 询问是否终止批处理，输入 Y。
   启动器会自动使用 `app/current.json` 指向的版本，首次运行使用包根目录程序。
2. **主机**：双击带图标的 **`VirtualCapture.exe`** 打开采集工作台，无需命令行。
   此入口会自动选择 OTA 当前版本；`vc-host.exe` 为实际 GUI 进程，旧 CMD 仅保留作兼容入口。
3. 填写被采集机的 `IPv4:15143`，点击连接，确认双端网卡、分辨率、帧率和采集源，再开始采集。
4. 主机的下游软件选择 VirtualCapture DirectShow 摄像头。停止采集或正常关闭工作台会回收本次管理的
   模块和虚拟卡注册。

首次联网可能出现 Windows 防火墙提示；只在受控直连网络中使用。主机输入省略端口时仍默认 5143，
所以使用随包后台时请明确填写 `:15143`。默认备用端口用于与旧 Python 后台区分；本包不替换旧产品。

高级启动示例：

```powershell
.\Start-Capture.cmd --port 15143 --bind 0.0.0.0
.\vc-capture.exe --bind 127.0.0.1 --port 15143 --root . --native-dir .
.\vc-capture.exe --root . --native-dir . --list-adapters
.\vc-capture.exe --root . --print-config
.\vc-capture.exe --root . --print-update-token
```

`VirtualCapture.exe --root <目录>` 可指定独立主机配置/运行目录。首次启动使用默认
`vc-rust-config.json`；不要把旧 `vc-config.json` 当成 Rust 配置复制过来。
查看或临时回环诊断时，采集端需显式加 `--allow-loopback-diagnostics`；普通运行不接受回环或虚拟网卡。

## 更新（OTA）

1. 在**被采集机**运行 `vc-capture.exe --print-update-token --root <运行根>`，得到 64 位令牌。
2. 在主机“应用更新”面板粘贴令牌并“配对”。令牌只保存在本机 `update-peer-token.json`，不进入普通配置。
3. “选择 ZIP”可把新版本暂存到本机；“选择 ZIP 上传”会同时上传到已配对的采集机。
   上传/校验在后台进行，最大 180 秒；暂存不会立即切换版本。
4. 确认当前没有采集或待清理会话后点击“应用更新”。更新助手会在旧进程退出后校验、切换并启动新版本；
   失败会尝试恢复并确认旧版本，事件写入 `updates/history.jsonl`，恢复失败必须查看助手日志处理。
   已配对令牌可重新配对，保留且校验有效的历史版本提供回滚入口。

首次启用 OTA 若采集机仍是旧 0.1.0，需要先手工替换一次支持 OTA 的版本（旧程序没有更新端点）；
之后即可从同一 `--root` 继续更新。ZIP 必须是本包生成的完整便携包，不能用旧 Python OTA 包。

更新成功后目录变为版本化布局：

```text
app/releases/<build_id>/  当前与历史版本
app/current.json          当前版本指针
data/vc-rust-config.json  配置（首次升级从旧根目录复制，不移动旧文件）
data/run/                 运行证据
updates/staged|worker|incoming/  暂存与助手工作区
updates/history.jsonl     更新历史
```

首次升级只复制配置与令牌，不迁移或覆盖已有 `run/` 证据；失败时移除 `app/current.json` 即可回到原布局。

## 当前边界

- 产品数据面是 Rust：`vc_stream.dll`（raw/mjpeg/pyrowave，Npcap/SHM v3 ABI）与 `vc_vcam.dll`（Rust DShow）。
  本包不含旧 C++ `vc-sender.exe`/`vc-receiver.exe`/`vc-vcam.dll`，不要从旧包复制这些文件。
- Raw 是默认编码。MJPEG/PyroWave 必须显式启用（`codec_experimental=true`）且两端后端精确匹配；
  未声明或未协商的组合会明确拒绝，不静默降编码/尺寸/帧率。`turbojpeg.dll` 与
  `libpyrowave-shared-1.dll` 是对应编码链路的运行依赖；PyroWave 还需要 Vulkan 1.3。
- 两端网卡列表只显示系统明确识别的物理网卡，隐藏虚拟/回环/元数据未知设备；失效选择需手动重选。
  回环只在采集端显式诊断开关下可用。
- 深蓝画幅是尺寸比例示意，不是实时预览。
- 如果已注册任何 VirtualCapture 虚拟卡，本客户端拒绝接管。虚拟卡注册项指向解压路径，运行中不要移动目录；
  强杀/断电不能保证清理。
- 本机解压运行与 OTA 自检不代表双机升级/回滚、240fps、DDA/WGC 实采、完整 DShow 编排、
  MJPEG/PyroWave 画质或长稳验收。
- `vc-vcam-verify.exe` 仅用于隔离的只读能力探测（`--dll ... --capabilities`），不会被正常启动流程注册。
- 旧 C++/Python 项目的 `python scripts/verify.py` 仍是独立的旧产品验证门禁，不由本 Rust 打包流程执行。

## 包内容与校验

- **`VirtualCapture.exe`**：带图标的主机日常入口，直接打开当前版本，无需 CMD/PowerShell。
- `vc-host.exe`、`vc-capture.exe`：Rust Release 程序；主机应用带原生窗口/任务栏图标。
- `vc-updater.exe`：OTA 版本切换助手。
- `vc_stream.dll`：进程内 Raw/MJPEG/PyroWave 采集/传输模块，带 `vc_module_get_api` 版本化 C ABI（ABI 1）。
- `vc_vcam.dll`：Rust DirectShow 虚拟摄像头 DLL（`VCStreamCapabilities` 与 COM 导出）。
- `vc-stream.exe`、`vc-transport.exe`：独立 Npcap Raw / pcap 诊断工具，不被产品启动流程调用。
- `vc-vcam-verify.exe`：隔离的 DShow 能力探测工具，不执行注册。
- `turbojpeg.dll`：固定 libjpeg-turbo 3.1.2 的 CPU/SIMD 共享运行库，供 MJPEG 使用。
- `libpyrowave-shared-1.dll`：固定 PyroWave 共享运行库，供 PyroWave 使用。
- MSVC runtime DLL：随程序分发的 Visual C++ 运行依赖。
- `scripts/start-host.ps1`、`scripts/resolve-capture.ps1`：稳定启动入口，按 `app/current.json` 选择版本。
- `licenses/`：Rust 与原生第三方依赖许可、资源归属与运行库说明。
- `package-manifest.json`：源码版本/dirty 状态、源码摘要、逐文件 SHA-256 和验证范围。

ZIP 旁的 `.sha256` 校验整个压缩包；包内清单校验解压文件。本包不能上传到旧 Python agent 的 `/update`。
