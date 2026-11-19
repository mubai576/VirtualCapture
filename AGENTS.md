# AGENTS.md — VirtualCapture 的 AI/开发者入口

任何 AI 会话先读本文件（约 3 分钟），再按"读什么"表按需深入。本文件是 **repo map 的唯一真相源**。

## 项目一句话

被采集机（目标 2K240 正常使用）只把 `640x640@240 NV12`（614400 B/帧，~1.18 Gbps）经 2.5G 网线以
raw L2 EtherType `0x88B5`（Npcap）发到主机重组，逐帧 FNV-1a64 对账，零压缩零编解码。
传输链路已双机验证（T0/T1/T2 全绿，T-06 已关），P4 链路已落地；稳定版保留 P5 实际使用状态，开发进入 P6 可配置采集与传输。
当前部署为 PC2 192.168.2.59 采集 → PC1 192.168.2.100 接收/GUI/虚拟卡（:5143），不按历史报告固定角色。
2026-11-02 owner 确认约三天实际使用满意；封存与未验收边界见 `PLAN.md`、`docs/bringup/bringup-20261102.md`。
同日 owner 批准进入 P6：可变分辨率、裁切/缩放、链路预算与有损编码。
开发版 P6-2 已接通可变尺寸 Raw、四种裁切/缩放、真正发送限速、SHM v3 和动态 DShow；稳定部署仍为上述固定格式。
2026-11-03 owner 批准 MJPEG（D21）：显式libjpeg-turbo CPU/SIMD、每帧独立、JPEG质量配置；
构建与边界见`docs/runbooks/mjpeg.md`，不自动替换部署。本机Raw验证见 `docs/bringup/bringup-20261103-p6-raw.md`。
**2026-11-13 owner 决定（D23）移除 H.264/H.265**（含 Media Foundation 与原生 NVENC）及其测试结果、
`vc-codec-probe`/`vc-codec-roundtrip`：这种编解码方式不适合当前项目。当前只有 Raw 与可选开发版 MJPEG；
MJPEG 开发链路已接通 L2/解码/SHM/DShow 与显式 GUI 后端选择，须明确启用，未通过协商的组合拒绝启动。
双机/长稳/画质仍待验收。设计背景见 `docs/proposals/configurable-stream-20261102.md`。
**同日 owner 批准（D24）接入 PyroWave**（Vulkan 计算小波 intra-only，MIT）：开发版显式后端
`pyrowave:vulkan`，阶段一 CPU 上传/回读，两端需 Vulkan 1.3；构建/限制见 `docs/runbooks/pyrowave.md`，
本机证据见 `docs/bringup/bringup-20261113-pyrowave.md`。不自动替换部署，未双机/长稳/画质验收。
**范围：只管采集与传输；推理端不在本项目范围**（PLAN D8）。

## 读什么（按任务）

| 你要做 | 先读 |
|---|---|
| 了解当前进度、领任务 | `PLAN.md`（§0 现状、§2 决策、§3 任务卡） |
| 改协议/发收端/测试源/agent API | `.agents/skills/virtualcapture/SKILL.md` + 其 `references/`（契约） |
| 构建、打包、发版 | `docs/runbooks/build.md` |
| 找开发工具、诊断脚本 | `docs/runbooks/development-tools.md` |
| 部署/切换 PC2 | `docs/runbooks/pc2-cutover.md`，背景 `docs/spec/deployment.md` |
| 理解需求与链路预算 | `docs/spec/requirements.md` |
| 查某个历史结论（"hist S23" 之类） | `docs/history/plan-history.md`（只读档案，不改） |
| 双机联调证据 | `docs/bringup/`（每次联调一份，日期命名） |
| P4 设计与 owner 批示 | `docs/proposals/` |

## Repo map

| 路径 | 内容 | 归属/规则 |
|---|---|---|
| `CMakeLists.txt` + `cmake/` | 根构建编排、统一编译/输出规则、Npcap SDK 查找、long-path manifest、MJPEG/PyroWave 固定依赖 ExternalProject | 从根目录配置；产物跟随 `-B` |
| `rust/` | D26/D27独立开发版：`vc-core` UI无关编排/配置/OTA，`vc-capture` CLI后台，`vc-host` GPUI，`vc-updater`版本切换助手；`vc-transport`冻结wire/分片/重组迁移起点；独立 `vc-stream` 提供 Windows Npcap Raw 双向诊断与可选 SHM v3，未接入产品控制面/打包/OTA；实际产品采集仍复用C++且仅Raw；运行布局分app/data/updates，legacy配置保留 | 构建/独立绿色ZIP见 `docs/runbooks/rust.md`（`rust/packaging/package.py`），契约见 skill `references/rust-control.md`；不自动部署 |
| `common/` | 共用 CRC32 / FNV-1a64、`stream.h` 会话/整数变换/限速、`mjpeg_codec.h`、`pyrowave_codec.h`、`access_unit.h`/`codec_session.h` 编码载荷与会话（`vc-common` CMake target） | 算法只保留一份；MJPEG依赖/许可证见`docs/runbooks/mjpeg.md`，PyroWave见`docs/runbooks/pyrowave.md` |
| `proto/vc_wire.h` | 28B 包头，**冻结** | 仅 owner 可改 |
| `sender/` | `vc-sender.exe`（pattern/file/dda/wgc，`--batch`，`pattern.h` 整数测试源，`dda.h`/`wgc.h` 桌面采集） | C++17 |
| `vcam/` | `vc-vcam.dll`（P4-3 DShow source filter，用户态，会话 NV12 尺寸/fps，SHM v3，motion 仅显式 bring-up） | C++17 |
| `receiver/` | `vc-receiver.exe`（重组、hash、stats、hashlog 带 qpc_ns） | C++17 |
| `tools/` | `monitor/` 预览与诊断、`testwin/` DDA 动画源、`vcam_verify/` DShow 枚举/隔离取帧 | C++17；随根 CMake 一起构建，现有产物路径保留 |
| `agent/vc-agent.py` | 主机 UI / 被采集机后台（REST :5143，/exec 白名单，/files 分页，stage/activate OTA） | 单文件零依赖 |
| `ctrl/` | `vc-ctrl.py` 编排判分、`bringup.py` 联调报告、`reconcile.py` 对账、`run_remote.py` 远控/OTA、`vc-monitor-tui.py` 文本仪表、`pattern_ref.py` Python 镜像 | |
| `ctrl/tests/` | pytest：matrix（向量/判据/live）、OTA、GUI、package（打包保护/回滚/路径） | |
| `scripts/` | `package.py`（一键编译验证打包）、`verify.py`（本地总闸）、`brn.py`（双机 batch 探针）、`*.ps1`（装机/网卡/更新/watchdog） | |
| `scripts/analysis/` | 一次性取证脚本（gap/对账/审计），可复用但非主流程 | |
| `version.json` + `packaging/` | 版本元数据、`vc-agent.spec` 打包配置、成品启动入口 | 发版操作见 build runbook |
| `.agents/skills/virtualcapture/` | 项目技能与 wire/API/test-matrix 契约 | 与代码同步维护 |
| `docs/` | `spec/` 需求与早期方案、`runbooks/` 操作手册、`bringup/` 联调记录、`proposals/` 设计提案、`history/` 档案 | 只放 md |
| `thirdparty/` | vendored npcap-sdk、ninja | 不改 |
| `build/` `dist/` `run/` `release/` `staging/` `prev/` `archive/` | 产物 / 运行数据 / OTA 机关 / 归档 | 全部 gitignore；`staging/` `prev/` 是 live OTA 机关，不手删 |

## 常用命令

```powershell
python scripts/package.py              # 编译 + 验证 + 全量便携目录 → dist/VirtualCapture
# 已确认包含当前未提交改动时：python scripts/package.py --allow-dirty
python scripts/verify.py                 # 改完任何代码必跑：build + 双 selftest + pattern_ref + pytest
python scripts/verify.py --no-build --live Loopback   # 加跑本机回环 live 用例
curl http://127.0.0.1:5143/adapters     # 查卡；--adapter 必填
python ctrl/vc-ctrl.py --sender http://192.168.2.59:5143 --receiver http://192.168.2.100:5143 `
  --seconds 60 --adapter Controller --batch 69 --dst-mac <主机MAC>   # 双机 T1
python ctrl/run_remote.py --agent http://192.168.2.59:5143 --update release/vc-<ver>.zip   # OTA
```

环境：Windows，MSVC 19.44（cmake VS 生成器，cl 不在 PATH），python 3.14，Npcap。PC1=192.168.2.100，PC2=192.168.2.59（agent 手动部署在 PC2 的 Documents\VirtualCapture）。

## 硬规则（违反即打回）

1. 不改包头尺寸/字段顺序；proto 变更须 owner 批。
2. 像素生成禁用浮点；原有 raw 模式禁止压缩。D21 批准的 MJPEG 有损链路仍为开发功能
   （H.264/H.265 已按 D23 移除），不得将其冒充 raw 逐像素哈希通过；不得静默切编码/降尺寸/降帧率，
   未实现或未支持的组合须明确拒绝。
3. 性能结论必须用 `motion`，注明 pattern/batch/adapter；回环数据不得冒充双机结论。
4. 跨机不比 QPC 绝对值，只报同机时段（inter-arrival、单端分段）。
5. 不放宽 `references/test-matrix.md` 判据；改判据只能经 owner 决策（记入 PLAN §2）。
6. 契约变更（CLI/API/IPC/拓扑/判据）必须同一改动内更新 skill `references/` 与 PLAN §1。
7. 二进制/日志/运行数据不入 git；删除 PASS 报告或 receiver 证据前先归档到 `run/archive.d/`。
8. 双机跑完**立刻**拉回远端完整 hashlog（历史 PC2 receiver，当前 PC2 sender；本机 receiver 证据也保留；`/files` 支持 offset/limit 分页），远端 run/ 不保证长存。

## 协作约定（多 AI 并行）

- **并发编辑**：本仓库常有多个 AI 会话同时工作。动手前 `git status` 看是否有他人未提交改动；
  不要覆盖、不要把他人未提交的工作区文件打进 release/OTA 包；冲突时停下报告。
- **提交**：只在 owner 要求时 commit；按文件精确 `git add`，提交信息写"卡号 + 做了什么"。
  提交前先 `git diff --cached --stat` 确认暂存区只有自己的文件；推荐 `git commit -- <paths>` 只提交指定路径
  （`git mv` 会直接进暂存区，裸 `git commit` 会把别的会话的 rename 一起带走——2026-10-23 d15287d 实例）。
- **进度记录**：完成任务卡 → 在 PLAN §3 卡头标 `DONE` + 一行证据（commit/报告路径）。
  不在 PLAN 追加流水账；长证据写 `docs/bringup/` 或 `docs/proposals/`，PLAN 只放链接。
- **需要 owner 决策**：写进 PLAN §2 表格（建议 + 解锁什么），不要自行拍板。
- **新文档放哪**：联调记录 → `docs/bringup/bringup-YYYYMMDD.md`；设计 → `docs/proposals/`；
  操作步骤 → `docs/runbooks/`；契约 → skill `references/`。根目录不新增 md。
