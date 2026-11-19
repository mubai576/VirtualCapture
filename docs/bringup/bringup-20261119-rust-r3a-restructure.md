# Rust-R3a 仓库收拢与本机验证（2026-11-19）

## 范围与结果

本轮按全 Rust 产品方向清理工作树：移除旧 C++/Python 产品树、根 CMake 与退休运维工具，旧实现只保留在 Git 历史。保留独立整数图案参考、冻结 wire 向量、自检/专项 smoke、Python 门禁与打包协调脚本；Cargo 是唯一产品 workspace，CMake 只构建 PyroWave 第三方 Vulkan 运行库。新增由 Cargo 构建的隔离 DirectShow verifier。13 份过时部署/API/CPU MJPEG/旧双机工具操作文档从当前文档树删除，当前约束收敛至 AGENTS、PLAN、skill references、spec 与 runbooks；PC2 调优文档标明仅为历史记录，原内容不作为当前操作授权。

DDA/WGC 强制 D3D11 硬件 GPU BGRA→NV12 转换，无 CPU/WARP 降级；PyroWave 仅显式 `pyrowave:vulkan`，实际选中设备须可证明为集成/独立 GPU 且 Vulkan≥1.3。生产 CPU TurboJPEG/MJPEG 后端已移除，旧配置/冻结 codec ID 只用于明确拒绝。I226 自动选择与 MTU 的新旧配置迁移、双端只读 NlMtu、有效值复核均实现；不修改操作系统网卡设置。

## 自动门禁

`python -I scripts/verify.py` 最终源码态完整退出码 0：fmt、workspace tests、全 target Clippy `-D warnings`、Release、独立 Python pattern reference、冻结 wire vectors 与 DLL self-check 全部通过。Rust workspace 测试共 **312 passed / 0 failed / 0 ignored**，22 个测试组。Python 自检脚本 19 个文件均通过编译检查。

- 总门最终证据：`run/rust-final-gates-708a449ebc/summary.json` 与 `verify.log`。退出码 0；22 组 Rust 测试 312 passed / 0 failed / 0 ignored，fmt、Clippy、Release、独立 pattern reference、wire vectors、DLL self-check 全通过。175 个 Rust/Python 门禁输入在门前与门后 SHA-256 指纹一致。
- Windows 定向复核：`cargo check --manifest-path rust/Cargo.toml --workspace --all-targets --locked --target x86_64-pc-windows-msvc` 退出码 0，包含 `vc-media` 与 `vc-stream`；125 个 Rust 路径的门前/门后指纹未变化。证据：`run/rust-peer-recheck-25145e1c27/summary.json` 与 `cargo-check.log`。此复核只证明当前源码编译通过，不增加硬件或产品验收结论。
- PyroWave 第三方 native-only CMake configure/build/install：退出码 0；安装只包含产品所需共享运行库与许可。

## 同机数据面与采集验证

所有测试使用隔离运行目录与当前 Release DLL；Loopback 是功能诊断，不代表物理 NIC、双机吞吐或矩阵验收。

| 检查 | 本轮配置/结果 |
|---|---|
| Raw `vc_stream.dll` | `motion`，640×640 NV12，240fps，MTU 9000，Loopback，batch 1，480 帧；TX/RX 全帧 hash 与独立参考一致，`complete=480`、drops/gaps/hash mismatch/transport errors 均为 0；同机 send→RX p99 0.4752ms，source-start→RX p99 0.9909ms。未启用独立 performance gate。 |
| PyroWave `vc_stream.dll` | 显式 `pyrowave:vulkan`，同一几何/帧率/帧数；480 个 AU 完成解码，480 帧 source 参考、TX/RX transport hash 和 SHM 校验通过；`complete=480`、drops/gaps/hash mismatch/transport errors 均为 0；send→RX p99 1.4019ms，source-start→RX p99 2.9929ms。未启用独立 performance gate。 |
| DDA/WGC GPU source | `gpu_convert=true`、`motion`、320×180/240、30fps、MTU 9000、Loopback；DDA stretch 45 帧、DDA fill 45 帧、WGC crop/resize 90 帧、WGC stretch 45 帧全帧 hash 对账通过，close 终态通过；每项 drops/gaps/hash mismatch=0。 |
| Rust DirectShow verifier | `vc_vcam.dll` 与 Rust verifier 隔离加载；no-data、empty-ring、delivery-repeat、geometry-416、stop-blocked、stop-held-samples、disconnect-running 七场景全 PASS。未注册 filter、未访问真实用户注册表。 |
| REST/control | 416×416、60fps、Raw motion、Loopback、batch 1，120 帧；Host DLL receiver 与 Rust capture REST 完成全帧 FNV 对账；生命周期、参数回显、错误会话拒绝与优雅停机通过。 |
| 独立 Raw CLI | `vc-stream.exe` 双进程 Loopback，640×640、60fps、MTU 9000、batch 1、motion、120 帧；TX/RX 及独立参考 120/120 相同，SHM v3 最新帧/哈希标记正确。 |
| 网卡列表 | 完整诊断列表 14 项；REST 隐藏 11 个非物理/Loopback 项；无诊断授权的 Loopback 自动路由 prepare 返回 409，未启动会话。未发送物理 NIC 数据包。 |
| GUI | 实际 GPUI 窗口 smoke 退出 0；初始截图正常非黑屏。覆盖更新面板、token 配对/替换、预设、诊断详情、紧凑窗口、物理网卡刷新、连接错误与正常关闭。未开始采集或注册 DShow。 |

专项证据全部在 gitignored `run/` 下：本机整合摘要为 `run/rust-only-validation-2bf16ac120/summary.json`；Raw/PyroWave 480 帧详细报告分别位于 `run/rust-dll-stream-smoke-5f6d03b83f70/report.json` 和 `run/rust-dll-stream-smoke-539a58ffc4d9/report.json`；GPU source 报告为 `run/rust-source-dll-smoke-c3fdc9e6ea46/report.json`；DirectShow 为 `run/rust-vcam-dll-smoke-c446ce77beff/report.json`。DLL smoke 首轮发现诊断脚本字段/共享统计校验器调用签名与 Raw harness 不一致；修正并加入自检后，两个 codec 重跑均 PASS。未改任何验收门限。

GUI 初始截图：[initial.png](../../run/rust-gui-smoke-2e5fac33fb/initial.png)。本轮截图与历史黑屏结果分开保留；历史失败不回填为本轮失败，本轮非黑屏也不等同于完整便携包验收。

## 清理审计

- 147 个旧跟踪文件删除；旧产品目录/脚本清理后无跟踪残留。根目录只保留 `.gitignore`、`AGENTS.md`、`PLAN.md`。
- 未暂存任何改动；index 为空。6 个未跟踪文件为本轮 bring-up、SHM v3 spec、Cargo verifier、固定 PyroWave 依赖脚本、两份独立 Python pattern/hash 参考。
- `proto/`、`thirdparty/`、`docs/history/`、历史 `docs/bringup/` 均无修改；既有运行证据未清理。`update-token.json` 与 `update-peer-token.json` 仍仅在 ignore 规则中；本轮未读取、显示、提交或删除令牌文件。
- Rust 解释性注释扫描无剩余英文说明句；必要 API/类型名、代码字面量、rustdoc `# Safety` 标题不翻译。冻结文件 fixture 中的历史来源文本不改写，以免改变字节契约。

## 未验收边界

这批证据只证明特定 Windows 主机、当前 Release 产物和隔离 Loopback/窗口测试。Loopback batch=1，不是物理网卡整帧 sendqueue 性能；短时 480 帧测试未启用性能 gate。DDA 是私有合成窗口夹具，不是目标桌面画质/性能验收。未执行物理 NIC、双机 T1–T5、长稳、画质、完整便携包/OTA 或完整产品编排；Rust 等价双机矩阵自动编排与完整双端 hashlog 取证工具仍缺。未操作 PC2/物理网卡，未部署/切换稳定版，未注册真实 DirectShow filter。Rust-R3a 的 DONE 仅表示仓库收拢、GPU/MTU 实现与本机验证子卡完成；Rust-R3 整体不得因这些本机 smoke 通过而标 DONE，仍须完成 Rust-R3b 所列的专项验收。
