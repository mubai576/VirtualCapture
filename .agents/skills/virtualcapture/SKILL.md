---
name: virtualcapture
description: VirtualCapture Rust 采集、GPU NV12 转换、Npcap L2 传输、SHM/DirectShow、控制面与验收契约。修改发收端、API、协议、测试或打包时使用。
---

# VirtualCapture 项目技能

仓库地图和协作规则只维护在根 `AGENTS.md`；进度与决策看 `PLAN.md`。
所有路径按仓库根解释。当前仅维护 Rust 产品；旧 C++/Python 产品查 Git，不恢复为隐式依赖。

## 阅读顺序

| 任务 | 契约 |
|---|---|
| 冻结 28B 包头、MTU、批提交、会话隔离 | `references/wire-protocol.md` |
| REST v2、网卡/MTU、预算、模块生命周期、OTA/归属 | `references/rust-control.md` |
| PyroWave、48B VCAU、有界解码与有损对账 | `references/encoded-stream.md` |
| T0–T5、D1、不允许放宽的门限 | `references/test-matrix.md` |
| 便携载荷、来源与硬门禁 | `references/packaging.md` |
| SHM v3 布局 | `docs/spec/shm-v3.md` |
| 构建与运行操作 | `docs/runbooks/build.md`、`docs/runbooks/rust.md` |

## 不变量

- 范围仅采集与传输。`VirtualCapture.exe` 默认打开 GPUI 角色入口：首次选择采集端/接收端，原子保存独立启动偏好后记住选择；接收工作台、空闲采集页及 `--select-role` 可重新选择。显式 `capture` 子命令仍运行 REST v2 采集后台；角色不从历史 PC 编号推断。
- 两端同网卡承载 L2 数据与 IPv4 控制，保留控制 IP；不改系统 MAC。普通产品仅接受 Windows 明确标记为 Up 的物理/非虚拟网卡，不偏置厂商或型号；路由自动选择，多候选需明确选择。
- `proto/vc_wire.h` 冻结 28B、EtherType `0x88B5`；改动必须先获 owner 批准。
- `vc_stream.dll` 在宿主进程内运行，与诊断 EXE 共享实现；不回退已移除的产品进程。
- DDA/WGC 必须硬件 GPU 转换，无 WARP/CPU 降级；`pattern` 是独立合成诊断。编码仅 Raw 与显式 PyroWave；旧 MJPEG 拒绝启动。
- 图案 `counter/checker/rings/noise/motion` 在 `rust/crates/vc-stream/src/pattern.rs`，独立 Python 参考在 `rust/tests/reference/pattern_ref.py`；整数、确定性。性能必须 `motion`，静态帧不能代替压力源。
- Raw 整帧 FNV-1a64 对账；有损比较传输 hash，再核对解码/SHM/DShow，不要求源像素与解码像素相等。
- 跨机 QPC 不相减；Loopback 不冒充物理网卡、双机或完整产品验收。没有运行的门禁不记 PASS。
- API/CLI/IPC/拓扑变化同步对应 references 与 PLAN §1。旧文档/证据仅说明当时版本，不能覆盖当前实现。

## 本地入口

```powershell
python scripts/verify.py
```

```powershell
python scripts/verify.py --no-build --live Loopback
```

默认总闸不进行真实采集、网卡收发、注册或远程操作。可选 Loopback 是明确的本机诊断；双机 T1–T5 自动编排的 Rust 替代工具尚未实现，旧工具已按重构要求移除。门限保留，缺口不能默认为通过。
