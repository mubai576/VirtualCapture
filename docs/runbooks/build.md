# Build 可重复构建

## 一键编译与打包（推荐）

```powershell
python scripts/package.py
```

输出统一放在 `dist/VirtualCapture/`：agent（含 GUI）、7 个原生目标、MSVC 运行库、启动脚本、
运维与 Python 控制脚本、文档、版本信息、SHA-256 清单。直接复制整个目录到目标机器，入口是
`Start-VirtualCapture.cmd`，使用说明在成品 `README.md`（源码为 `package-quickstart.md`）。
日常使用无需目标机 Python；实际网卡收发前仍需安装 Npcap。Python 控制/分析工具的额外依赖见说明。

工作区有未提交改动时默认停止。确认这些改动均可包含后执行：

```powershell
python scripts/package.py --allow-dirty
python scripts/package.py --allow-dirty --output "D:/Deliverables/VirtualCapture"
```

相对输出路径从仓库根解析；仓库内只允许 `dist/` 的子目录。完整流程自动运行本地总闸、
冻结 agent，并从隔离目录检查成品自检、HTTP 健康、GUI 与 vcam 文件定位；全部成功才发布。
失败保留上一份成品；成功替换时将旧目录完整归档到相邻的 `VirtualCapture.archive/`，不删除旧配置或运行证据。
日志在 `build/package/package-*.log`。清单记录实际源码及产物 SHA-256；构建过程中源码变化会终止本次发布。
成品 PowerShell 脚本统一使用 UTF-8 BOM，自动检查 Windows PowerShell 5.1 解析与程序查找，避免中文编码导致启动失败。
成品目录不包含开发机配置、运行数据、编译中间文件、单元测试或历史取证脚本。

## 工具链与入口

Windows + MSVC（C++17）+ Windows SDK，CMake 最低 3.20；本项目使用 Visual Studio 17 2022 生成器。
Python 用于 agent、控制脚本与 pytest；agent 打包另需 PyInstaller。
Npcap SDK 查找顺序：有效的 `NPCAP_SDK/Include/pcap.h` → `thirdparty/npcap-sdk` → 无 SDK 的动态加载构建。
Npcap 运行时仅在实际收发时需要。

所有构建从仓库根 `CMakeLists.txt` 配置。共享编译/输出规则在 `cmake/VirtualCaptureTargets.cmake`，
SDK 查找在 `cmake/Npcap.cmake`；各模块 CMake 只声明自己的源文件与依赖。
`common/` 提供共用校验算法，`tools/` 收纳原生辅助工具；完整 repo map 只维护在 `AGENTS.md`。

## 从零构建

在仓库根目录执行：

```powershell
cmake -S . -B build
cmake --build build --config Release
```

构建目录可通过 `-B` 指定，所有原生产物都跟随该目录；例如：

```powershell
cmake -S . -B build/clean
cmake --build build/clean --config Release
```

无需单独进入 `sender/`、`receiver/` 或 `tools/` 配置 CMake。

## 产物位置

下表按默认 `-B build` + Visual Studio `Release` 配置；改用其他 `-B` 时替换 `build/` 前缀。

| 目标 | 产物 | 兼容入口 |
|---|---|---|
| `vc-sender` | `build/Release/vc-sender.exe` | 构建后同步复制到 `build/vc-sender.exe` |
| `vc-receiver` | `build/Release/vc-receiver.exe` | 构建后同步复制到 `build/vc-receiver.exe` |
| `vc-vcam` | `build/Release/vc-vcam.dll` | 注册直接使用该文件 |
| `vc-testwin` | `build/Release/vc-testwin.exe` | DDA 动画测试源 |
| `vc-monitor` | `build/Release/vc-monitor.exe` | NV12 预览仪表 |
| `vc-monitor-diag` | `build/Release/vc-monitor-diag.exe` | D3D11 诊断 |
| `vc-codec-roundtrip`（仅开发） | `build/Release/vc-codec-roundtrip.exe` | 显式MFT实际编码/解码原型与aperture自测；不交付，命令见codec-backend提案 |
| `vc-codec-probe`（仅开发） | `build/Release/vc-codec-probe.exe` | MF候选/类型探测，不含在便携包中，不代表产品支持 |
| `vc-vcam-verify` | `build/vcam_verify/Release/vc-vcam-verify.exe` | 保留原有验收工具输出路径 |

Debug 对应 `Debug/`；单配置生成器不加配置子目录。辅助工具源码迁入 `tools/` 不改变上述 Release 路径。

## 自测

```powershell
python scripts/verify.py
python scripts/verify.py --no-build --live Loopback
```

第一条运行 build + 发收端 selftest + pattern_ref + pytest；第二条在已有默认构建上加跑本机回环 live。
`verify.py` 固定查找默认 `build/`，自定义构建目录请直接运行对应产物。
未指定 `VC_ADAPTER` 时 pytest 跳过 live 用例，T3 默认跳过；这些跳过不等于双机验收通过。
P6-2 开发专项：`python scripts/p6_raw_verify.py`（motion/batch1/Loopback/真实DShow）；`--dda`需变化桌面。
`python scripts/p6_product_verify.py`验证隔离HTTP产品编排，只有注册操作被替换，使用随机端口/私有映射，不触碰稳定5143或注册表。
P6-3 开发专项：`python scripts/p6_codec_verify.py --codec h264|hevc --encoder <身份> --decoder <身份>`；支持 `--matrix`、`--loss`、`--decode-in-flight`、`--max-au-bytes` 与码率参数。`p6_product_verify.py` 同样可显式选择编码及后端；身份和实际证据见 `../bringup/bringup-20261103-p6-codec.md`。本机 Loopback 正向测试使用 batch1，batch69短发送明确失败，不自动回退。
这些脚本不随成品交付；DLL隔离加载不注册。工具与专项验收入口见 `development-tools.md`，双机判据见 skill `references/test-matrix.md`。

## Agent 打包

```powershell
python -m PyInstaller packaging/vc-agent.spec
```

单独冻结 agent 时，`packaging/vc-agent.spec` 使用 `agent/vc-agent.py` 作为入口，产物为 `dist/vc-agent.exe`。
完整交付使用上方 `scripts/package.py`，无需分别收集 CMake 与 PyInstaller 的产物。
打包前检查 `git status`；不要将其他会话未提交的改动混入 release/OTA 包。

## Release zip

版本以 `version.json` 为准。包内包含 `vc-agent.exe`、`vc-sender.exe`、`vc-receiver.exe`、
`vc-vcam.dll`、`version.json` 与所需 `scripts/*.ps1`；原生文件取上述 Release 产物。
使用 `Compress-Archive` 生成 `release/vc-<ver>.zip`；部署/切换见 `pc2-cutover.md`。
完整便携目录包含白名单之外的工具与文档，不能直接整体压缩上传 `/update`；OTA zip 仍按本节选取允许的文件。
构建验证本身不打包、不注册 DLL、不触发 OTA。

## 运行数据与归档

- `build/`、`dist/`、`release/`、`archive/`、`run/` 均为 gitignore 产物目录。
- 保留 report JSON、报告引用的 session 文件与最近 5 轮；其他历史运行证据归 `run/archive.d/`。
- 删除 PASS 报告或 receiver 证据前必须先归档；双机测试结束后立即分页拉回 PC2 receiver hashlog。
- `staging/` 与 `prev/` 是 live OTA 机关，不手动删除或移动。
- 历史取证脚本放 `scripts/analysis/`，原生辅助工具源码放 `tools/`。
