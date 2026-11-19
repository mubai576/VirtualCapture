# Rust-R3 本机产品 DLL 验证（2026-11-19）

## 范围

本记录补充 Rust-R3 的同机验证。三编码性能测试通过同一进程加载 Release `vc_stream.dll`，在 Npcap Loopback 上使用 `motion`、640×640 NV12、240fps、MTU 9000、batch1 各发送 2400 帧。Loopback 的 batch1 是诊断例外，不代表产品物理网卡的整帧 batch 行为，也不能代替双机或物理链路验收。所有 QPC 延迟均为本机时间。

Release 数据面 DLL SHA-256：`eecfac05e9f143aa405411e76c44d625681f9f681f5f76220a70d438b8734844`。

## 三编码 240fps Loopback

| Codec / backend | TX steady fps | send steady fps | RX steady fps | send→RX p99 / max | source start→RX p99 / max | ≥10ms | 完整性 |
|---|---:|---:|---:|---:|---:|---:|---|
| Raw | 240.0000 | 240.0002 | 240.0018 | 0.4536 / 0.5703 ms | 0.9700 / 1.0760 ms | 0 | TX/RX 2400/2400 一致；motion 参考 2400/2400 |
| MJPEG `turbojpeg:cpu` | 240.0000 | 239.9659 | 239.9669 | 0.8605 / 1.2242 ms | 3.4589 / 3.6188 ms | 0 | TX/RX transport 2400/2400 一致；参考逐帧通过 |
| PyroWave `pyrowave:vulkan` | 239.9242 | 239.8619 | 239.8314 | 2.6318 / 5.2355 ms | 6.6164 / 9.2733 ms | 0 | TX/RX transport 2400/2400 一致；参考逐帧通过 |

三者的 DLL smoke 都报告 `performance_pass=true`；发送帧起始和发送起始 cadence 均在目标 240fps 的 1% 门内，所有逐帧 source-start→接收完成样本均小于 10ms。编码后对账使用 transport hash，另验证解码后的 SHM NV12；不把有损输出误称为 Raw 逐像素相等。

原始报告与完整逐帧证据：

- Raw：[run/rust-dll-stream-smoke-0857dd94d3f6/report.json](../../run/rust-dll-stream-smoke-0857dd94d3f6/report.json)
- MJPEG：[run/rust-dll-stream-smoke-3432f4f8c632/report.json](../../run/rust-dll-stream-smoke-3432f4f8c632/report.json)
- PyroWave：[run/rust-dll-stream-smoke-eebdb53fac93/report.json](../../run/rust-dll-stream-smoke-eebdb53fac93/report.json)

上述是同机 DLL/Loopback 性能证据，不是物理 NIC、双机、持续稳定性或画质验收。尤其 PyroWave 最大 source-start→RX 为 9.2733ms，距离 10ms 门限余量有限，仍需物理链路和长稳专项评估。

## Rust 采集与虚拟卡 DLL

`source_dll_smoke.py` 在私有窗口夹具和 Loopback 上实测 WGC：CPU crop + 窗口 resize 90 帧全量收发一致；GPU stretch 45 帧全量一致；关闭窗口正确报告 terminal error 且停止/销毁成功。整体报告为 `PARTIAL`，不是全 PASS：该次 Rust smoke 无 DXGI output，DDA CPU 与 GPU 两项明确 `BLOCKED`，没有切换到其他采集源。证据：[source smoke report](../../run/rust-source-dll-smoke-41cf49b57a92/report.json)。后续旧产品总闸已枚举到 DDA/WGC 输出，但没有据此补记 Rust DDA 为通过。

Rust `vc_vcam.dll` 由 `vc-vcam-verify.exe` 隔离加载，7 个场景全部通过：no-data、empty-ring、delivery-repeat、geometry-416、stop-blocked、stop-held-samples、disconnect-running。验证器报告 SHA-256 `4b94cc3cc724f22aa508cfaac2945ff5a5c67601f52a9c885ad205ba06c7866a`。未注册 DirectShow，也未修改注册表。证据：[vcam smoke report](../../run/rust-vcam-dll-smoke-4207c8007b80/report.json)。

随后生命周期复核发现共享 `--native-dir` 配合不同运行根时，旧版按根保存的 owner 文件不足以约束全局 CLSID：另一根可能凭过期本地记录覆盖 `FrameMap` 或注销现用设备。修复将 schema 2 owner（DLL/根/PID/进程创建时间）写入 HKCU CLSID `VirtualCaptureOwner`，并要求它与本根一份有效证明精确匹配；缺少旧版全局值时 fail-closed，不自动恢复。注册、恢复、注销持全局 CLSID mutex 串行化；其他运行根或其他存活属主，即使共享同一 DLL 也不能接管。

首次 OTA 的 Legacy/Managed 切换、回滚和写入中断由稳定 owner 日志处理：current/pending 固定在 `ROOT/data/run/vcam-owned{,.pending}.json`，兼容读取旧 `ROOT/run/vcam-owned.json`，不移动 run 证据。持锁更新先保全旧全局证明，再刷盘 pending、提交注册表、提升 current。写注册表失败保留旧 current；提交后退出或最终 rename 失败保留新 pending，重试先保存精确证明再覆盖 pending。仍拒绝任何不匹配记录，未采用“两属主均死即允许”的宽松恢复；尚无 InprocServer32 的全局预约也会阻止另一宿主接管。此机制不承诺断电时跨介质原子提交。

`97349f8` 提交后复核另确认路径别名缺陷：经 junction 别名启动，OTA 删除旧 release DLL 后，旧注册路径无法直接 canonicalize，回退词法拼写会先使 `InprocServer32` 与规范 owner DLL 不匹配，恢复返回 `Unknown`；路径 containment 也可能误判。修复在共享路径规范化层解析最深存在祖先，再拼回缺失后缀，让原始别名注册字符串与 canonical owner 比较；不只修 containment。

只有 `NotFound` 可按缺失组件继续向上；访问错误、悬空重解析点及含 `/`、`.`、`..` 的歧义 verbatim 路径均 fail-closed，不退回词法相等。普通 Win32 路径消解 `.`/`..`；盘符与 UNC 可折叠 verbatim 前缀，合法 `\\?\Volume{GUID}\` 路径保留卷命名空间，拒绝任意设备命名空间。若 Windows 将卷 GUID 路径规范化为已分配盘符，须查询盘符根的卷 GUID，并确认重建后的完整路径一致；查询失败或路径不一致均拒绝。全局 owner DLL/根记录须保持 canonical 拼写，避免把别名重定向当作原属主证明。

`97349f8` 的 owner 回归 16 项通过，覆盖共享 DLL 跨根、PID 复用/未知状态、首次激活与布局回滚、注册表写失败、提交后中断及再次失败、最终 rename 失败、缺失/损坏/外根证明、等价根路径、空路径、预约及 mutex 互斥/abandonment。失败注入使用私有临时目录与注册表回调替身，未运行注册表集成测试，未触碰 DirectShow 或用户注册表。本次追加 GUID→盘符规范化兼容回归，分别覆盖同卷同路径接受、不同卷/路径拒绝及当前系统卷只读往返；未改盘符、注册表或挂载状态。

## 构建与打包门禁

- 本次 owner 路径别名与卷 GUID 修复后，顺序执行 `cargo fmt --manifest-path rust/Cargo.toml --all --check`、workspace tests（`--locked`）、Clippy（`--all-targets --locked -- -D warnings`）及 Release workspace build（`--locked`），完整命令链退出 0。workspace 合计 **292 passed / 0 failed / 0 ignored**，含 `vc-core` 132 项、owner 29 项；[最终 Rust 日志](../../run/rust-r3-owner-path-final-rust-20261119-185318/rust-gates.log)、[结果与日志摘要](../../run/rust-r3-owner-path-final-rust-20261119-185318/rust-summary.json)。Release 仍有既有 EXE/DLL `vc_stream.pdb` 同名输出警告。本轮不据构建通过补判性能、GUI 或打包验收。
- 随后运行旧产品 `python -I scripts/verify.py`：**ALL PASS，303 passed / 0 failed / 6 skipped**，pytest 157.16s，完整总闸 160.51s；build、双 selftest、pattern reference 均通过。[最终总闸日志](../../run/rust-r3-owner-path-final-legacy-20261119-185041/legacy-verify.log)、[结果及源码前后哈希](../../run/rust-r3-owner-path-final-legacy-20261119-185041/legacy-summary.json)。`skipped` 表示条件未满足或需显式启用而未执行，不是通过或失败；本次 pytest 输出未启用逐项 skip-reason 报告，因此不推断这六项的具体原因。归档 Rust 结果时观测到的 `vcam.rs` SHA-256，以及此次旧产品总闸前后哈希均为 `5b08803a43612f93b35f8dca1570f281e051b00d7b87303cbd476a2e6f13b06e`；Rust 原命令链未采集执行前哈希，不将归档观测值冒充前后测量。
- 保留路径修复的中间失败：较早一轮 Clippy 因 6 处测试闭包多余借用失败，该轮 Release build 未运行；提前启动的旧产品总闸被编辑者取消，不作最终证据。[中间 Rust 日志](../../run/rust-r3-owner-path-20261119-173128-e59967de/rust-gates.log)、[取消的旧产品日志](../../run/rust-r3-owner-path-20261119-173128-e59967de/legacy-verify.log)、[尝试状态](../../run/rust-r3-owner-path-20261119-173128-e59967de/attempt-status.json)。随后格式检查、测试夹具缺失 helper 与合成卷 GUID 恢复 fixture 也各有失败；修正格式和夹具后才取得上述最终通过，未修改验收判据。
- 历史 owner 日志修复轮的 `cargo fmt --all --check`、workspace tests、Clippy（`-D warnings`）、Release build：通过。workspace 合计 **279 passed / 0 failed / 0 ignored**，含 `vc-core` 119 项、owner 16 项；[历史完整日志](../../run/rust-r3-owner-journal-20261119-160741-24240/rust-gates.log)。该轮重建后 `vc_stream.dll` 与 `vc_vcam.dll` SHA-256 均与上述实际测试模块一致；宿主/控制代码已变更，不能据此把早先候选包称为包含新 owner 修复的完整发布包。
- 历史 owner 日志修复轮的旧产品 `python -I scripts/verify.py`：**ALL PASS，303 passed / 6 skipped，151.92s**；build、双 selftest、pattern reference 均通过。[历史总闸日志](../../run/rust-r3-owner-journal-verify-20261119-160925-22477/legacy-verify.log)。
- 保留先前失败，不将重试隐藏为首轮全绿：首次总闸 `test_rotation_markers_and_live_recorder` 在 45ms 时间窗只采到 1 次，要求至少 2 次，结果 1 failed / 302 passed / 6 skipped；该用例单独复跑通过，随后完整总闸 303 passed / 6 skipped、151.97s 通过，未修改旧代码、测试或判据。[先前失败日志](../../run/rust-r3-owner-journal-20261119-160741-24240/legacy-before-journal-first-failure.log)、[先前完整复跑](../../run/rust-r3-owner-journal-20261119-160741-24240/legacy-before-journal-rerun-pass.log)。
- Rust 便携包候选：native build、portable smoke、同进程 DLL smoke、adapter smoke 均通过。候选目录：`build/rust-package/20261119-144121-ba6e13/`。
- GUI smoke 未通过：窗口存在且未最小化，但保存的 1280×900 client 截图全黑（单色 `(0,0,0)`），证据：[initial.png](../../run/rust-gui-smoke-5862eeacbc/initial.png)。非空白截图是既有硬门禁，未放宽。package coordinator 因此中止，未产出经完整流程验收的最终 ZIP；该候选不能作为通过 GUI 门禁的发布包。
- GUI 失败前 package coordinator 未执行 OTA 和 `--extended-smokes`；之后对同一候选解压目录独立补跑：`control_smoke.py` 120 帧 Raw motion Loopback 全帧hash对账通过；`ota_smoke.py` 以真实候选ZIP通过暂存、激活、回滚、载荷篡改拒绝与失败恢复；`host_update_smoke.py` 通过私有ROOT的HostController本机/peer更新、helper重启及GPUI ready回执。三者均未触碰PC2、稳定服务、网卡或DShow注册。报告分别为[control](../../run/rust-smoke-4e603e6180/report.json)、[capture OTA](../../run/rust-ota-smoke-149239604e/report.json)、[host update](../../run/rust-host-update-smoke-d2d17fd742/report.json)。这些独立专项通过不改变GUI门失败及package coordinator失败状态。

## 结论与未验收边界

本轮证明 Release `vc_stream.dll` 的 Raw/MJPEG/PyroWave 在同机 Loopback 的 240fps cadence 与低于 10ms 延迟门通过；WGC 窗口路径及隔离 vcam DLL 生命周期场景通过。control/OTA/host update 的独立候选包专项也通过。该次 Rust DDA smoke 因缺少 DXGI 输出不可测，未另补 Rust DDA 验收；GUI 非空白截图门禁失败，完整 Rust 包验收未通过且未生成最终发布 ZIP。物理网卡整帧批量收发、双机、长稳、最终图像质量、DShow 注册和完整产品编排均未据此宣称通过；不部署，不切换稳定版本。
