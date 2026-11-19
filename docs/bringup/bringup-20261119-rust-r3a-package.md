# Rust-R3a 提交与便携包验证（2026-11-19）

## 授权与来源

owner 本轮要求“提交打包”。仅提交已审阅改动、生成独立便携包并执行隔离本机门禁；没有 push、部署、切换稳定版、PC2、物理网卡流量/参数变更或真实 DirectShow 注册授权。

- 分支：`rust`。
- 实现提交：`a918e4f562e6d52e258ac3ad7e001b7afb498164`，`Rust-R3a: 收拢全 Rust 产品并落实 GPU-only 与 I226/MTU`。
- 提交前分组审阅覆盖 240 个变更路径，逐项指纹一致，无确认的阻断问题；精确暂存后核对 index 路径与内容。Git 检出一组重命名，提交统计为 239 files changed、4392 insertions、23669 deletions。
- 从干净工作树执行 `python -I -X utf8 scripts/package.py --extended-smokes`，未使用 `--allow-dirty`。包的 `source.revision` 为上述实现提交，`source.dirty=false`、`source.status=[]`；构建前后 HEAD 与工作树未变。
- 本报告及 PLAN 状态更新在包生成后单独提交，不属于包内记录的源码提交；不得把后续文档提交冒充包的来源。

## 产物

| 项目 | 结果 |
|---|---|
| 版本 | `0.2.1`，Windows x64 Release |
| 目录 | `dist/VirtualCapture-Rust-0.2.1-windows-x64-20261119-221315-4ee39f/` |
| ZIP | `dist/VirtualCapture-Rust-0.2.1-windows-x64-20261119-221315-4ee39f.zip` |
| 校验文件 | 同名 `.zip.sha256` |
| ZIP 字节数 | `19717132` |
| 文件数 | `1363`，含 `package-manifest.json` |
| SHA-256 | `f53a4e2c406dc4185dfb98bfbc0ac9d60fde8fad46f09d7a415b5fcbdb288789` |
| 打包报告 | `build/rust-package/20261119-221315-4ee39f/report.json` |
| 完整日志 | 同目录 `package.log`；外层记录 `run/rust-authorized-package-99040c4cfc/` |

完整命令退出 `0`，外层记录耗时 `107.44s`。独立复核 ZIP CRC、ZIP/目录清单一致、1363 个文件集合、各载荷文件大小及目录/ZIP 双份 SHA-256、ZIP 总哈希与校验文件全部通过；另核对 350 个源码文件指纹与包内来源快照一致。证据为 `run/rust-authorized-package-99040c4cfc/artifact-verification.json`。包内仅 Raw/显式 PyroWave，`gpu_only=true`、C ABI v1、SHM v3；无旧 C++ 产品、CPU MJPEG、凭据、配置、日志或 PDB。Rust 许可无 unresolved exceptions，PE 导入审计通过。

## 为什么文件数较多

共 1363 个文件中，运行程序及运行时文件为 25 个（7 个 EXE、11 个 DLL、2 个启动 CMD、README、两个 launcher support 脚本和 `version.json`）；`package-manifest.json` 另 1 个。其余 1337 个是随发行包携带的许可证材料，不是产品源码：1331 个 Rust 许可目录项覆盖 524 个 Cargo 依赖（1329 个按依赖归档的许可/NOTICE 文本，加 `inventory.json` 与汇总 `THIRD-PARTY-NOTICES.md`）；另有 7 个 PyroWave/Granite/volk/Vulkan-Headers/MSVC 许可文件。Rust 清单和汇总占约 3.9 MB；保留归属和上游原文是当前许可收集器的输出策略。七个 EXE 中，`vc-stream.exe`、`vc-transport.exe`、`vc-vcam-verify.exe` 是诊断/隔离验证入口；其余四个是图形入口、host、capture 与 updater。实际压缩包约 18.8 MiB，完整包的总字节还包含未压缩体积约 38.5 MB 的 7 个 EXE。

## 门禁与实际运行

| 检查 | 结果与边界 |
|---|---|
| fmt / workspace tests / Clippy / Release | 全通过；22 组，312 passed / 0 failed / 0 ignored；Clippy 使用 `-D warnings`。 |
| PyroWave native-only CMake | configure/build/install 通过，仅第三方运行库、许可与 app-local MSVC 运行库。 |
| 解压便携运行 | 中文/空格路径、清理开发工具后的 PATH、CMD/EXE 入口、版本化 current 布局、真实 app-local runtime 加载、REST v2 健康与 DLL capability 通过。 |
| `vc_stream.dll` | Raw motion、640×640、60fps、MTU 9000、Npcap Loopback、batch 1、120 帧；TX/RX/独立整数参考逐帧一致，drops/gaps/hash mismatch=0，SHM 通过；pending destroy 保持所有权并最终返回 0，cleanup_errors 为空。 |
| 网卡可见性 | 诊断 14 项，REST 显示 3 个物理候选、隐藏 11 项；无诊断授权的 Loopback prepare 返回 409，未启动会话。仅枚举，不是物理网卡传输测试。 |
| GUI | 真正启动解压包内 GPUI，更新面板配对/替换、预设、详情、紧凑窗口、物理网卡刷新、连接 503 错误与正常关闭通过；退出 0。13 张截图均非空，已查看初始图与全套拼图，无黑屏；未开始桌面采集或注册 DShow。 |
| capture OTA | 私有根、随机本机端口，真实 capture/updater；完整候选包校验/上传/激活、配置迁移、显式回滚、失败回滚与指针恢复失败 code 6 路径通过。不是已用部署。 |
| 扩展控制面 | Raw motion、416×416、60fps、MTU 9000、Loopback batch 1，120/120 帧 hash 相同；精确回显、会话隔离、stop/destroy 与服务优雅关闭通过。 |
| 扩展 HostController OTA | 独立 fixture、私有本地/模拟 peer 根；真实 helper 更新并重启 capture、激活本机 GPUI 首帧、直接 EXE 重新打开及配对/配置保留通过；未改变真实 DShow 注册。 |

包内 10 个 `checks` 均为 `passed`，无 `pending` 或未跑冒充通过。专项落盘报告：

- DLL：`run/rust-dll-stream-smoke-93efe66885d0/report.json`。
- 网卡：`run/rust-adapter-smoke-ea317a521e/report.json`。
- GUI：`run/rust-gui-smoke-15d1703e11/report.json`；[初始截图](../../run/rust-gui-smoke-15d1703e11/initial.png)、[全套截图拼图](../../run/rust-authorized-package-99040c4cfc/gui-contact-sheet.png)。
- capture OTA：`run/rust-ota-smoke-d23c7ac796/report.json`。
- 控制面：`run/rust-smoke-9e5414a9ae/report.json`。
- HostController OTA：`run/rust-host-update-smoke-34dc822075/report.json`。
- 独立报告/截图核验：`run/rust-authorized-package-99040c4cfc/smoke-verification.json`。

## 中断、警告与未验收边界

首次 `20261119-220934-269ccf` 尝试因后台会话中断停在许可收集阶段，无最终报告、候选 ZIP 或该批次 dist 产物；不计通过。原 `build/rust-package/20261119-220934-269ccf/` 与 `run/rust-authorized-package-e2d06fb5a9/` 原样保留，本次使用新的唯一目录重跑全部门禁，未清理或覆盖旧成品。

Release 仍输出 `vc-stream` EXE 与 `vc_stream` DLL 的 PDB 同名警告；构建退出 0，PDB 不进入便携包，不宣称全程无警告。独立报告提取曾误用 CLI 摘要字段 `dll_sha256`，触发只读核验脚本的 `KeyError`；按落盘报告实际字段 `dll.sha256` 重核通过，未改产品、报告原件或门限。

本次 DLL smoke 的同机 send→RX p99 为 `16.7587ms`，source-start→RX p99 为 `17.2662ms`；`performance=false`、`performance_pass=null`，只证明该 60fps 短时配置的功能一致性，不代表延迟/240fps 门限通过。没有执行物理 NIC 整帧 sendqueue、双机 T1–T5、长稳、目标桌面画质/性能或完整真实 DShow 编排；Rust 双机矩阵与完整双端 hashlog 取证工具仍缺。此前 DDA/WGC/PyroWave 专项结果见独立 [R3a 收拢记录](bringup-20261119-rust-r3a-restructure.md)，不冒充本轮包内重跑。

本轮只补齐本机便携包及隔离 OTA 门禁，不修改历史失败，不放宽 test-matrix；Rust-R3a 保持 DONE，Rust-R3 整体保持 IN PROGRESS。没有读取/显示/提交/删除既有根令牌，没有操作 live `staging/`、`prev/`，没有 push、部署、PC2、物理网卡参数变更或真实 DirectShow 注册。
