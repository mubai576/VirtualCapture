# AGENTS.md — VirtualCapture 开发入口

先读本文件，再按任务深入；这里是仓库地图的唯一真相源。当前开发分支全面转向 Rust，旧 C++/Python 产品源码只保留在 Git 历史，不再维护双套产品。

## 当前产品

被采集机 DDA/WGC → 硬件 D3D11 GPU 转 NV12 → Npcap L2 `0x88B5` → 主机重组/解码 → SHM v3 → Rust DirectShow。默认 Raw，另有显式开发后端 `pyrowave:vulkan`。桌面采集禁止 CPU 转换、WARP 和静默降级；CPU MJPEG 已移除。PyroWave 仍有 CPU 上传/回读，不是零拷贝。

网卡正常枚举 Windows 状态为 Up 的物理非虚拟网卡，不按厂商或型号偏置；控制连接路由可唯一确定时自动选择，多候选需明确选择。MTU 可配置，新配置默认读取双端网卡报告值，不修改系统网卡参数。640×640@240 Raw 为 614400 B/帧、约 1.18 Gbps 像素载荷；可变尺寸不等于任意组合已验收。

便携包只有 `VirtualCapture.exe` 和 `vc-updater.exe` 两个 EXE。主程序首次显示采集/接收角色选择，持久化后启动上次选择的 GPUI 角色；`capture` 子命令仍运行采集后台，内部子进程隔离探测虚拟卡；更新器独立执行。角色进程内加载 `vc_stream.dll` C ABI v1；DLL 与构建树诊断 EXE 共享 Rust 实现，不拉起旧 sender/receiver。范围仅采集与传输，不含推理或独占全屏 Hook。

**Rust-R3 仍在验收中**：历史本机 Loopback/WGC/隔离虚拟卡证据不代表物理网卡、双机、长稳或画质通过；历史 GUI 黑屏导致完整便携验收失败，硬门禁不得绕过。本次重构不自动部署、不切稳定版。

## 按任务阅读

| 任务 | 入口 |
|---|---|
| 进度、决策、任务卡 | `PLAN.md` |
| 协议、控制面、编码、测试门限 | `.agents/skills/virtualcapture/SKILL.md` 与 `references/` |
| 构建、门禁、便携包 | `docs/runbooks/build.md` |
| 运行、诊断、OTA | `docs/runbooks/rust.md` |
| 需求、链路预算 | `docs/spec/requirements.md` |
| SHM 布局 | `docs/spec/shm-v3.md` |
| 历史证据 | `docs/bringup/`；`docs/history/` 只读，旧实现查 Git |
| 历史设计 | `docs/proposals/`（日期方案不覆盖当前契约） |

## 仓库地图

| 路径 | 内容 |
|---|---|
| `rust/Cargo.toml`、`Cargo.lock` | 唯一产品 workspace 与锁定依赖 |
| `rust/crates/vc-core` | UI 无关配置、预算、网卡、会话、DLL 生命周期、虚拟卡归属、OTA |
| `rust/crates/vc-capture` / `vc-host` / `vc-updater` | REST v2 采集库 / `VirtualCapture.exe` 统一入口与 GPUI / `vc-updater.exe` 更新助手 |
| `rust/crates/vc-stream` | `vc_stream.dll` 与独立 `vc-stream.exe`；Npcap、调度、SHM 写端 |
| `rust/crates/vc-source` | 整数 pattern、DDA/WGC、GPU 转换与四种几何变换 |
| `rust/crates/vc-media` | Raw/PyroWave、VCAU、受保护的原生库加载与看门狗 |
| `rust/crates/vc-transport` | 冻结 wire、CRC/FNV、分片重组与 pcap 诊断 |
| `rust/crates/vc-vcam` | Rust COM `vc_vcam.dll`、SHM 读端、隔离 `vc-vcam-verify.exe` |
| `rust/tests` | 本机诊断；`reference/` 为独立 Python 图案/哈希参考，不是产品后台 |
| `rust/packaging` | 唯一打包器、载荷清单与支持脚本；`native/` 仅构建第三方 PyroWave/MSVC 运行库 |
| `scripts/verify.py` / `package.py` | 根目录门禁 / 打包薄入口，Python 只作开发协调工具 |
| `proto/vc_wire.h` | 冻结 28B 包头；仅 owner 可批准修改 |
| `thirdparty/` | vendored 工具与 SDK，不改 |
| `docs/` | 仅 Markdown：spec/runbooks/bringup/proposals/history |
| `build/ dist/ run/ release/ archive/ staging/ prev/` | 忽略的产物、证据与 live OTA 机关，不手动清理 |

## 常用命令

```powershell
python scripts/verify.py
```

```powershell
python scripts/verify.py --no-build --live Loopback
```

```powershell
python scripts/package.py --allow-dirty
```

`--allow-dirty` 只能在审阅全部将被收录的改动后使用，不是包含其他会话改动的授权。包写入唯一 `dist/VirtualCapture-Rust-*`，不覆盖已用成品。环境：Windows x64、Rust MSVC、Windows SDK、CMake/MSVC（仅第三方库）、Python、Npcap；细节见 build runbook。

## 硬规则

1. 不改 28B 包头尺寸/字段顺序；协议变更须 owner 批。像素生成禁浮点；GPU UNORM 格式解码例外须逐字节匹配独立整数参考。
2. Raw 禁压缩；PyroWave 不套 Raw 像素相等门。禁止静默换编码、源、设备或降低尺寸/fps；未支持组合明确拒绝。旧 MJPEG 配置须报错，不能自动改成 Raw。
3. 性能必须用 `motion`，注明尺寸/fps/pattern/batch/adapter；Loopback 不冒充物理网卡/双机。跨机不减 QPC 绝对值，只报同机分段与 inter-arrival。
4. 不放宽 `references/test-matrix.md` 门限；改判据只经 owner 决策并记 PLAN §2。旧双机自动编排工具已移除，Rust 等价工具未实现，不把缺测当通过。
5. CLI/API/IPC/拓扑/判据变化，同一改动更新 skill `references/` 与 PLAN §1。使用真实能力和精确会话回显，不以预算或枚举当实测。
6. 停止/销毁未确认不得卸载 DLL；pending 锁配置、OTA、虚拟卡操作。禁止强制展开挂起 GPU 调用。
7. 二进制/日志/运行数据不进 Git；删 PASS/receiver 证据前先归档 `run/archive.d/`。双机结束立即保全双端完整 hashlog；Rust 没有旧 `/files`，须另行安排完整取证路径。
8. 不读取、显示、提交或删除现有 `update-token.json`；不动 `staging/`、`prev/`。部署、PC2 操作、改物理网卡参数、真实 DirectShow 注册须单独授权。

## 协作

- 动手前 `git status`；不覆盖并发改动，冲突即停。不把未审阅的他人改动纳入包/OTA。
- 仅 owner 要求时 commit/push；按文件精确暂存，先看 `git diff --cached --stat`，提交信息写“卡号 + 内容”。不要裸提交夹带其他会话文件。
- 完成卡标 DONE + 一行证据；长证据进 `docs/bringup/`，PLAN 不记流水账。历史报告不回填新的通过结论。
- 新文档：操作进 runbooks、契约进 skill references、联调进 bringup、设计进 proposals；根目录不新增 md。Rust 解释性注释用准确简洁中文，保留必要 API 名称与 rustdoc 标记。
