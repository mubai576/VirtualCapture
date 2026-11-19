# Rust + GPUI 重构（2026-11-14）

## 状态与授权

owner 确认在新 `rust` 分支开发，当前 C++/Python 实现先不动；首期迁移控制面并复用原生二进制，
允许重新设计 API。UI 使用 Zed GPUI；主机只有一个客户端，内嵌主机管理，被采集机只有 CLI 后台。
分支从 `bb7a6ff` 创建，独立 worktree `D:\Project\REA\VirtualCapture-rust`。

R1 的配置/预算/会话/进程/主机编排、REST 后台和 GPUI 客户端已实现；
实际验证与未验边界见[开发记录](../bringup/bringup-20261114-rust-control.md)，
构建运行见[runbook](../runbooks/rust.md)。不自动部署、提交或替换现用产品。

2026-11-19 owner 进一步授权（D28）全面转 Rust：产品宿主通过版本化 DLL 模块在进程内连接数据面，
不再以 sender/receiver EXE CLI 拼装本机数据面；模块同时可编译 DLL 与独立 EXE 并共享实现；仅 NV12
Raw/MJPEG/PyroWave；Raw 按尺寸/MTU 一帧一个 batch，编码按实际 AU 整帧单次 queue；Loopback 仅显式
诊断且固定 batch1 例外。旧 C++/Python 契约与稳定部署保持不变。最新本机测试状态与边界见
[Rust-R3 本机记录](../bringup/bringup-20261119-rust-r3-local.md)：三编码Loopback240fps延迟门及WGC/vcam隔离验证通过，
但GUI黑屏截图使便携完整验收失败；物理网卡、双机、长稳、画质和DShow完整编排仍待验收；Rust-R3保持IN PROGRESS。

## 目标结构

```text
VirtualCapture.exe / vc-host.exe (GPUI host)
              | links Rust crates; no local data-plane CLI children
         vc-core (configuration, session, budget, lifecycle, OTA)
              | loads vc_stream.dll (versioned C ABI v1, in-process)
              | loads vc_vcam.dll (Rust DirectShow/COM source filter, SHM v3)
              +-- vc-capture.exe (capture-side REST service host)

capture PC: vc-capture.exe -> vc-core -> vc_stream.dll (same in-process ABI)

vc-stream crate (rlib + cdylib + independent vc-stream.exe diagnostic)
  +-- pattern / DDA / WGC and integer transforms (vc-source)
  +-- Raw / MJPEG / PyroWave codecs with fixed dynamic DLLs (vc-media)
  +-- frozen 28B wire, fragmentation, checksums, bounded reassembly (vc-transport)

Standalone vc-stream.exe / vc-transport.exe / vc-vcam-diag EXE reuse the same crates.
vc-vcam-verify.exe is an isolated read-only DShow capability probe, not a data-plane product.
```

The process boundary between the two PCs remains the versioned Rust control API; DLLs are local module boundaries,
not a replacement for network control. The product host loads versioned module DLLs through explicit C ABI tables or
links the same crates directly; it does not spawn sender/receiver CLI processes for local data flow. Independent
EXE tools remain supported and use the same library implementation. `vc_stream.dll` exposes C ABI v1
(`vc_module_get_api(1)`, opaque session handles, caller-owned buffers, explicit error codes, asynchronous
start/status and retryable stop/destroy); Rust ABI is never exported. `vc_vcam.dll` uses the DShow/COM ABI.

- GPUI stays in `vc-host`; `vc-core::HostController` remains UI-independent and owns lifecycle, snapshots and OTA.
- `vc-capture` provides the capture-side control endpoint and loads the same data-plane DLL.
- Capture, transport, codec and virtual-camera crates each have library APIs; `vc-stream` provides both DLL and
  independent EXE front ends on the same implementation. Module DLL dependencies are recorded in the package
  manifest with ABI/version compatibility checks.
- Codecs are `raw` (NV12 passthrough), `mjpeg` and `pyrowave` only; H.264/H.265 are not part of the target.
  Encoded modes require explicit `experimental=true` and exact backend identity; no silent fallback.
- Raw sendqueue packet count is derived at prepare/start from exact NV12 frame bytes and MTU
  (`ceil(frame_bytes / (mtu - 42))`); Raw submits one whole frame per sendqueue. Encoders submit the actual
  encoded access unit and flush at each frame boundary. Npcap Loopback is an explicit diagnostics exception:
  it submits per packet (batch 1) and is never used for product performance or dual-machine claims.
- GPUI uses `gpui-kit 0.9.1` (`gpui-pre 0.3.12` snapshot + `gpui-component`).
- The legacy C++/Python product remains separately buildable; it is not copied into the Rust package and is
  not a runtime fallback for the Rust product path.

## Current Rust structure (D28)

```text
capture PC: vc-capture.exe -> vc-core -> vc_stream.dll (in-process sender)
host PC:    VirtualCapture.exe -> vc-host.exe -> vc-core
                                             -> vc_stream.dll (in-process receiver)
                                             -> vc_vcam.dll (Rust COM DShow source)
```

`vc-stream` is both the product DLL implementation and the standalone diagnostic CLI; `vc-transport` is a
shared library, not a process boundary. The old `vc-sender.exe`/`vc-receiver.exe`/`vc-vcam.dll` C++ product
still exists for the legacy deployment under its own unchanged contract and is not used by the Rust product.

## 后续其他软件接入

软件集成层将来可在 `vc-core` 外增加 Windows `cdylib` + C ABI 包装，让其他语言调用同一主机控制器。
当前没有生成该 SDK DLL，也不承诺稳定 Rust ABI；仍需设计 opaque handle、内存所有权、线程/回调规则、
错误表示和版本协商。现有 `vc-vcam.dll` 提供 DShow 采集设备，是不同用途。

## 阶段划分

| 阶段 | 内容 | 现状 |
|---|---|---|
| R1 控制面 | UI 无关核心、被采集机 REST、主机 GPUI；API revision 2 动态 batch 契约 | 开发实现已落地 |
| R2a Raw 生命周期 | `vc_stream.dll` C ABI v1 接入 `vc-core`，移除本机 sender/receiver CLI 子进程；SHM v3 | 同机 DLL/Loopback 功能与 240fps 延迟门通过；物理网卡/双机未验收 |
| R2b 采集 | Rust pattern/DDA/WGC（`vc-source`）与 crop/fit/fill/stretch 整数变换；失败显式报错不回退 | WGC 私有窗口 CPU/GPU、resize/关闭验证通过；DDA 无 DXGI 输出 BLOCKED；双机未验收 |
| R3 虚拟卡 | Rust DShow/COM `vc_vcam.dll`，SHM v3 生命周期；外部 `vc-vcam-verify.exe` 隔离只读探测 | 隔离 DLL 七场景通过；完整 DShow 注册/编排与双机未验收 |
| R4 编码 | Raw NV12 passthrough、MJPEG、PyroWave；固定动态运行库；按实际 AU 整帧单次 queue | 三编码同机 DLL/Loopback 各 2400 帧通过 cadence/<10ms 门；画质/双机/长稳未验收 |
| R5 模块集成 | Windows C ABI v1、版本化 DLL manifest、package/OTA 兼容矩阵；保留独立 EXE hosts | control/capture OTA/host update 独立候选包专项通过；GUI 黑屏门失败，完整包未验收 |
| 软件 SDK | `vc-core` 的 C ABI DLL 包装 | 未实现；与 DShow DLL 分开 |

每阶段模块独立构建、测试；DLL/EXE 两种宿主必须共用同一实现。当前实现切片已删除用户可调 `capture.batch`：
Raw 由 `vc-core` 按尺寸/MTU 计算并回显 `batch_packets`，旧 API revision 1 对端不满足契约，revision 2
明确拒绝混跑。PyroWave 5 秒看门狗在进程内运行，只置会话 `OperationState` 粘性超时，不 `process::exit`；
挂起时保留 worker/句柄/DLL，`cleanup_pending` 期间锁配置/OTA/虚拟卡，不能安全停止时不得自动卸载。
后续继续按模块切片，不自动切换稳定部署；数据面替换仍须按现有判据验收。

## R1 接口与启动约束

完整契约只维护在 [rust-control.md](../../.agents/skills/virtualcapture/references/rust-control.md)。
后台提供 health/version/config/adapters、sender prepare/start/stop/stats；无旧 exec/files/OTA/UI。

启动前验证 Rust 身份、实际数据面模块能力、新鲜双端网卡与预算；主机下发完整 `SenderRequest`
（capture、session_id、source_mac、auto_adapter、adapter），prepare 不启动会话并精确回显。
主机将确认的网卡固定后启动本机 receiver、远端 sender（均为进程内模块），再比对启动回显。
停止必须带 session_id，防止 HTTP 超时后的清理误停其他会话。已有活动 sender 拒绝覆盖，
会话号对应的旧运行证据也拒绝覆盖。正常退出负责清理，异常断电不保证清理。

预算沿用现有公式：`P=mtu-42`、`B=width*height*3/2`、`N=ceil(B/P)`、
`wire_frame_bytes=B+N*66+max(0,18-last)`，
`usable=min(host_rx,capture_tx)*(100-headroom)/100`。
超过已知物理速度拒绝、侵入预留为 margin、缺元数据为 unknown，不把预算当作实测帧率。

## 硬边界

- 28B 包头、EtherType `0x88B5`、Raw 逐帧 FNV 与 SHM 契约不变；旧 C++/Python 契约不改，不把 Rust 行为写成 legacy 已变。
- Raw 是默认；MJPEG/PyroWave 必须显式 `experimental=true` 且两端后端精确匹配，未协商组合明确拒绝，不静默降编码/尺寸/帧率。
- 产品宿主不拉起 sender/receiver CLI 子进程；`vc_stream.dll` 缺失或 ABI/能力不符时明确失败，不回退旧程序。
- Raw 按尺寸/MTU 一帧一个 batch；编码按实际 AU 整帧单次 queue；Loopback 只作显式诊断且固定 batch1，不用于性能/双机结论。
- PyroWave 看门狗超时只报告会话粘性状态，不终止宿主、不强制卸载；pending 期间锁配置/OTA/虚拟卡。
- GUI 注册前隔离只读探测实际 DLL 能力；已有注册须由全局 owner 与本根精确匹配证明确认，只有本根本进程或确认已退出属主可恢复。跨根/其他存活属主/未知归属一律拒绝；current/pending 日志与兼容恢复见 Rust 控制契约。
- 本机 Loopback 和窗口冒烟不代替 Rust 双机、DDA/WGC、DShow、240fps、编码画质或长稳验收。
- 不触碰稳定目录、当前活动部署、远端角色或网卡设置。根原产品打包流程保持原样；Rust 包载荷以
  `rust/packaging/inventory.py`/`package.py` 为准，旧 C++ 产品不进入。
