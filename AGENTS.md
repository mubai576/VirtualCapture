# AGENTS.md — VirtualCapture 的 AI/开发者入口

任何 AI 会话先读本文件（约 3 分钟），再按"读什么"表按需深入。本文件是 **repo map 的唯一真相源**。

## 项目一句话

PC1（2K240 正常使用）只把 `640x640@240 NV12`（614400 B/帧，~1.18 Gbps）经 2.5G 网线以
raw L2 EtherType `0x88B5`（Npcap）发到 PC2 重组，逐帧 FNV-1a64 对账，零压缩零编解码。
传输链路已双机验证（T0/T1/T2 全绿，T-06 已关）；当前阶段 = P4 产品链路（采集性能 / 端到端延迟 / PC2 虚拟采集卡）。

## 读什么（按任务）

| 你要做 | 先读 |
|---|---|
| 了解当前进度、领任务 | `PLAN.md`（§0 现状、§2 决策、§3 任务卡） |
| 改协议/发收端/测试源/agent API | `.agents/skills/virtualcapture/SKILL.md` + 其 `references/`（契约） |
| 构建、打包、发版 | `docs/runbooks/build.md` |
| 部署/切换 PC2 | `docs/runbooks/pc2-cutover.md`，背景 `docs/spec/deployment.md` |
| 理解需求与链路预算 | `docs/spec/requirements.md` |
| 查某个历史结论（"hist S23" 之类） | `docs/history/plan-history.md`（只读档案，不改） |
| 双机联调证据 | `docs/bringup/`（每次联调一份，日期命名） |
| P4 设计与 owner 批示 | `docs/proposals/` |

## Repo map

| 路径 | 内容 | 归属/规则 |
|---|---|---|
| `proto/vc_wire.h` | 28B 包头，**冻结** | 仅 owner 可改 |
| `sender/` | `vc-sender.exe`（pattern/file/dda，`--batch`，`pattern.h` 整数测试源，`dda.h` 桌面采集） | C++17 |
| `receiver/` | `vc-receiver.exe`（重组、hash、stats、hashlog 带 qpc_ns） | C++17 |
| `agent/vc-agent.py` | 双机控制 agent（REST :18080，/exec 白名单，/files 分页，stage/activate OTA） | 单文件零依赖 |
| `ctrl/` | `vc-ctrl.py` 编排判分、`bringup.py` 联调报告、`reconcile.py` 对账、`run_remote.py` 远控/OTA、`pattern_ref.py` Python 镜像 | |
| `ctrl/tests/` | pytest：`test_matrix.py`（向量/判据/live）、`test_ota.py`（OTA 往返） | |
| `scripts/` | 运维：`verify.py`（本地总闸）、`brn.py`（双机 batch 探针）、`*.ps1`（装机/网卡/更新/watchdog） | |
| `scripts/analysis/` | 一次性取证脚本（gap/对账/审计），可复用但非主流程 | |
| `docs/` | `spec/` 需求与早期方案、`runbooks/` 操作手册、`bringup/` 联调记录、`proposals/` 设计提案、`history/` 档案 | 只放 md |
| `thirdparty/` | vendored npcap-sdk、ninja | 不改 |
| `build/` `dist/` `run/` `release/` `staging/` `prev/` `archive/` | 产物 / 运行数据 / OTA 机关 / 归档 | 全部 gitignore；`staging/` `prev/` 是 live OTA 机关，不手删 |

## 常用命令

```powershell
python scripts/verify.py                 # 改完任何代码必跑：build + 双 selftest + pattern_ref + pytest
python scripts/verify.py --no-build --live Loopback   # 加跑本机回环 live 用例
curl http://127.0.0.1:18080/adapters     # 查卡；--adapter 必填
python ctrl/vc-ctrl.py --sender http://192.168.2.100:18080 --receiver http://192.168.2.59:18080 `
  --seconds 60 --adapter Controller --batch 69 --dst-mac <PC2-MAC>   # 双机 T1
python ctrl/run_remote.py --agent http://192.168.2.59:18080 --update release/vc-<ver>.zip   # OTA
```

环境：Windows，MSVC 19.44（cmake VS 生成器，cl 不在 PATH），python 3.14，Npcap。PC1=192.168.2.100，PC2=192.168.2.59（agent 手动部署在 PC2 的 Documents\VirtualCapture）。

## 硬规则（违反即打回）

1. 不改包头尺寸/字段顺序；proto 变更须 owner 批。
2. 像素生成禁用浮点；链路禁止任何压缩。
3. 性能结论必须用 `motion`，注明 pattern/batch/adapter；回环数据不得冒充双机结论。
4. 跨机不比 QPC 绝对值，只报同机时段（inter-arrival、单端分段）。
5. 不放宽 `references/test-matrix.md` 判据；改判据只能经 owner 决策（记入 PLAN §2）。
6. 契约变更（CLI/API/IPC/拓扑/判据）必须同一改动内更新 skill `references/` 与 PLAN §1。
7. 二进制/日志/运行数据不入 git；删除 PASS 报告或 receiver 证据前先归档到 `run/archive.d/`。
8. 双机跑完**立刻**拉回 PC2 receiver hashlog（`/files` 支持 offset/limit 分页），PC2 run/ 不保证长存。

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
