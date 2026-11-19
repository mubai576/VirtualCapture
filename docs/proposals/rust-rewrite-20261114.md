# Rust + GPUI 重构（2026-11-14）

## 状态与授权

owner 确认在新 `rust` 分支开发，当前 C++/Python 实现先不动；首期迁移控制面并复用原生二进制，
允许重新设计 API。UI 使用 Zed GPUI；主机只有一个客户端，内嵌主机管理，被采集机只有 CLI 后台。
分支从 `bb7a6ff` 创建，独立 worktree `D:\Project\REA\VirtualCapture-rust`。

R1 的配置/预算/会话/进程/主机编排、REST 后台和 GPUI 客户端已实现；
实际验证与未验边界见[开发记录](../bringup/bringup-20261114-rust-control.md)，
构建运行见[runbook](../runbooks/rust.md)。不自动部署、提交或替换现用产品。

## 结构

```text
被采集机：vc-capture.exe  CLI + REST，管理 vc-sender.exe
                         ↑ HTTP（Rust API revision 1）
主机：    vc-host.exe     GPUI 原生客户端
               ↓
          vc-core        UI 无关主机编排、配置/预算/会话/HTTP/网卡/进程/虚拟卡
               ↓
          vc-receiver.exe + vc-vcam.dll（本阶段复用 C++）
```

- GPUI 使用 `gpui-kit 0.9.1`（`gpui-pre 0.3.12` snapshot + `gpui-component`）。
- `vc-core::host::HostController` 拥有工作线程、命令队列和快照；阻塞 HTTP、网卡探测、原生进程和注册操作
  不在 GUI 渲染线程运行。`vc-host` 负责交互和展示，主机编排没有 GPUI 依赖。
- `vc-capture` 使用 Axum/Tokio；阻塞操作交给 `spawn_blocking`。
- 主机不运行本地 REST。新服务明确返回 Rust implementation/API 身份，旧 Python 后台不混跑。
- 配置独立 `vc-rust-config.json`，不读取/覆盖旧 `vc-config.json`。

## 后续其他软件接入

软件集成层将来可在 `vc-core` 外增加 Windows `cdylib` + C ABI 包装，让其他语言调用同一主机控制器。
当前没有生成该 SDK DLL，也不承诺稳定 Rust ABI；仍需设计 opaque handle、内存所有权、线程/回调规则、
错误表示和版本协商。现有 `vc-vcam.dll` 提供 DShow 采集设备，是不同用途。

## 阶段划分

| 阶段 | 内容 | 现状 |
|---|---|---|
| R1 控制面 | UI 无关核心、被采集机 REST CLI、主机 GPUI；复用 C++、Raw only | 开发实现与本机冒烟，见记录 |
| R2 数据面 | Rust L2 发送/重组、Npcap FFI、DDA/WGC | 独立 Raw `vc-stream` Npcap/SHM v3 Loopback 诊断已通过；尚未集成产品控制器，物理 NIC/双机/DDA/WGC 未验收 |
| R3 虚拟卡 | Rust DShow source filter / Windows COM | 未开始 |
| R4 编码 | 显式 MJPEG/PyroWave 控制与 FFI | 未开始 |
| 软件 SDK | `vc-core` 的 C ABI DLL 包装 | 留出边界，未实现 |

每阶段独立构建运行，不自动切换稳定部署；数据面替换仍须按现有判据验收。

## R1 接口与启动约束

完整契约只维护在 [rust-control.md](../../.agents/skills/virtualcapture/references/rust-control.md)。
后台提供 health/version/config/adapters、sender prepare/start/stop/stats；无旧 exec/files/OTA/UI。

启动前验证 Rust 身份、实际原生能力、新鲜双端网卡与预算；主机下发完整 `SenderRequest`
（capture、session_id、source_mac、auto_adapter、adapter），prepare 不启进程并精确回显。
主机将确认的网卡固定后启动本机 receiver、远端 sender，再比对启动回显。
停止必须带 session_id，防止 HTTP 超时后的清理误停其他会话。已有活动 sender 拒绝覆盖，
会话号对应的旧运行证据也拒绝覆盖。正常退出负责清理，异常断电不保证清理。

预算沿用现有公式：`P=mtu-42`、`B=width*height*3/2`、`N=ceil(B/P)`、
`wire_frame_bytes=B+N*66+max(0,18-last)`，
`usable=min(host_rx,capture_tx)*(100-headroom)/100`。
超过已知物理速度拒绝、侵入预留为 margin、缺元数据为 unknown，不把预算当作实测帧率。

## 硬边界

- 28B 包头、EtherType `0x88B5`、Raw 逐帧 FNV 与 SHM 契约不变。
- R1 只启动 Raw。MJPEG/PyroWave 草稿可保存，但启动明确拒绝，不静默降编码/尺寸/帧率。
- GUI 管理本机虚拟卡时拒绝接管任何已有注册；注册前只读探测实际 DLL 能力。
- 本机 Loopback 和窗口冒烟不代替 Rust 双机、DDA/WGC、DShow、240fps 或长稳验收。
- 不触碰稳定目录、当前活动部署、远端角色或网卡设置。根原产品打包流程保持原样。
