# PLAN：VirtualCapture Phase 3（收口 ✅ → 双机放行 ✅ → 产品链路 P4）

> 入口与规则见根目录 `AGENTS.md`。本文件只放：现状、冻结契约、待决事项、当前任务卡。
> 完成一项在卡头标 DONE + 一行证据；长证据写 `docs/bringup/` 或 `docs/proposals/`，这里只放链接。
> 档案（只读）：旧 PLAN 全文与 "hist S11/S23/…" 编号 → `docs/history/plan-history.md`；
> 已关闭的 P1~P3 卡原文 → `docs/history/phase3-closed-cards.md`。

## 0. 现状快照（2026-10-23）

| 维度 | 状态 |
|---|---|
| 传输正确性 | ✅ 双机 T0/T1 五档/T2 10min 零丢零错；T2 144000 帧 PC2 hashlog 分页全量对账零错（docs/bringup/bringup-20261022.md） |
| 240fps | ✅ 默认 `--batch 69`，实测 240.0fps（hist S23） |
| 延迟抖动 | ✅ 判据按 D1（发端 cadence + 稳态 p99<8.3ms），T-06 已关；~1s 卡顿 T2 全程未复现（收端 max_gap 205.7ms 为 PC1 侧传导、531ms 为收尾 drain） |
| 调用链止血 / 构建 / git | ✅ P1 完成：--adapter 必填 + /adapters + 全错 exit 2/3 + live 测试；可重复构建（docs/runbooks/build.md）；git 管理 |
| OTA | ✅ 0.4.0 stage/activate + execv，PC2 推→回滚→再推全绿；/files 分页 |
| 版本 | PC2 0.4.0（运行中）；本机 version.json 0.4.0，但主 agent 进程仍为旧会话启动（pid 28796，未换代，见 §2 D9） |
| dda 采集 | 🟡 最小实现 ~10fps；无屏 SKIP |
| 产品链路 | 🟡 P4-1/2/3 提案已出、方向已批（docs/proposals/），未开工写码。范围：只管采集与传输，推理不在本项目（D8） |

环境：MSVC 19.44（cmake VS 生成器直连，cl 不在 PATH）、python 3.14、Npcap 运行时、
`thirdparty/npcap-sdk` + `thirdparty/ninja` 已 vendor。拓扑：同网口方案，PC1=192.168.2.100，
PC2=192.168.2.59（agent 手动跑在 PC2 的 Documents\VirtualCapture，exe 在根目录），联调发单播 `--dst-mac`。

## 1. 冻结契约（改即全线返工，须 owner 批准）

### 1.1 包头与链路
- `proto/vc_wire.h`：`vc_pkt_hdr_t` 28B，EtherType `0x88B5`，`payload = mtu - 42`，
  `total = ceil(614400/payload)`。proto/ 归 owner 锁定。

### 1.2 构建
- 根 `CMakeLists.txt` 编排 `sender/` `receiver/`，MSVC 优先；产物 `build/vc-sender.exe` `build/vc-receiver.exe`。

### 1.3 CLI（动词级冻结；dda/batch 为已落地的追加，本次正式纳入）
```text
vc-sender.exe --adapter <子串> --mode pattern|file|dda --pattern motion --fps 240
  --frames 0 --ethertype 0x88B5 --mtu 9000 --pacing-us 0 --batch 69 --dst-mac ff:ff:ff:ff:ff:ff
  --session <id> --stats <path> --hashlog <path> [--file <nv12> --loop] [--list-adapters] [--selftest]
vc-receiver.exe --adapter <子串> --ethertype 0x88B5 --mtu 9000
  --session <id> --stats <path> --hashlog <path> --timeout-s 0 [--expect-frames N] [--selftest]
```

### 1.4 文件型 IPC
- stats ~500ms 原子覆写（tmp→rename）；hashlog 每帧 append 一行。
- `sender.hash.jsonl`：`{"frame_id":N,"hash":"<16hex>","qpc_ns":T}`
- receiver hashlog D3 carries qpc_ns; reconcile compares frame_id and hash only.
- sender stats 新增 `gpu_dispatch_ms`（D3D11 timestamp 包住 CopyResource 的真 GPU 时间均值）+ `gpu_dispatch_samples`（有效样本数）；`gpu_ms` 仍为 capture() wall-clock，待 P4-1 实测分布后按 D10 决定是否切换口径。
- 哈希 FNV-1a64（初值 14695981039346656037，prime 1099511628211，小写 16hex），整帧 614400B。

### 1.5 全局禁止
1. 不改包头尺寸/字段顺序。 2. 像素生成禁用浮点。 3. 链路禁止任何压缩。
4. 性能宣称必须用 `motion` 并注明 pattern/batch/adapter。 5. 跨机不报 QPC 差值延迟，只报 inter-arrival。
6. 不许拿回环数据冒充双机结论。 7. 不许放宽 test-matrix 判据（改判据只能经 D1）。

### 1.6 目录
以 `AGENTS.md` Repo map 为准；产物与运行数据全部 gitignore；任务只碰自己的目录。

## 2. 决策记录与待决事项

| # | 事项 | 建议 | 解锁 |
|---|---|---|---|
| D1 | T-06 判据重定 | ✅批：节奏门=发端 cadence 4.166±0.3ms（PC1 本地 hashlog，不涉跨机时钟）；抖动门=收端 p99<8.3ms 保留；收端 avg 只判稳态窗口 | P2-2 |
| D2 | OTA 0.4.0 | ✅方案批（stage/activate 分离 + execv + 版本化 exe + GC + 全树 prev）；已落地，PC2 实推全绿（P3 DONE） | P3 |
| D3 | receiver hashlog 加 qpc_ns | ✅批：实际写点 vc_receiver.cpp:443，全文件唯一；reconcile 只比 frame_id+hash | P2-1 |
| D4 | dda selftest 无屏 SKIP | ✅批：有屏仍须 PASS；初始化失败但非无输出仍判 FAIL | P2-1 |
| D5 | `--batch` 默认值 | ✅批：改 69 | P2-1 |
| D6 | 0.3.7 re-stage | ✅批：作废，并入 0.3.8 | P2-1 |
| D7 | checker-transient | ✅保留并自动判：发端 hashlog 相邻帧间隔 >1s 由 gap_any.py 判（曾现 1713 帧停住 8.5s，删等于丢证据） | P2-3 |
| D8 | 范围与 P4-2 门限口径 | ✅批（2026-10-23）：本项目只管**采集与传输**，推理不在范围内。延迟口径 = PC1 采集完成 → PC2 重组完成，分段同机 QPC 测：PC1 段（采集完成→末包发出）+ R(N)（PC2 首包→重组完成）+ 线传段用 L(N) 上界；门限判全程 p99，候选 <4.16ms（1 帧），数字随实测分布定。R(N) 单独只作诊断 | P4-2 |
| D10 | P4-1 测量口径与 idle 语义 | ✅批（2026-10-24）：`gpu_ms` 切真 GPU 时间待 PC1 2K240 实测分布后再定，`gpu_dispatch_ms` 先作诊断；dda selftest 连续 5x E_TIMEOUT（2.5s 无新帧）判 SKIP（idle 与故障不可分，live 已容忍超时） | P4-1 |
| D9 | 本机主 agent 换代到 0.4.0 | ⏳待执行：agent 改动已由另一会话提交（d15287d），阻塞解除。注意 2026-10-23 00:10 对该进程 stage 成功但 /activate 无应答且未换代（pid 不变），建议直接手动重启本机 agent（本机，可逆），换代后 /health 应出新 pid | — |

## 3. 任务卡

派发顺序：`P4-1 × P4-3 并行` → `P4-2` → `P4-4（需 P4-3）`。每卡交付附验收命令完整 stdout；改代码必跑 `python scripts/verify.py`。

### 已关闭（原文见 docs/history/phase3-closed-cards.md）

| 卡 | 结论 | 证据 |
|---|---|---|
| P1-1 止血 | DONE | f128e03 / 67e8ce5；live 用例 VC_ADAPTER=Loopback 通过 |
| P1-2 构建+git+治理 | DONE | docs/runbooks/build.md；run/ 顶层 <60 |
| P1-3 文档同步 | DONE | e0483fb；2026-10-23 再整理为 AGENTS.md + docs/ 分层 |
| P2 0.3.8 + T-06 | DONE | 810f11d；docs/bringup/bringup-20261022.md（备注：T1×5 离线对账仅覆盖头部 ~27%，判定依据 PC2 in-run 全量校验；T2 已分页补全） |
| P3 OTA 0.4.0 | DONE | e01af52 / ac0e2b4 / ae44cbc；PC2 推→回滚→再推全绿 |

### 遗留观察 / 小改动（不阻塞 P4）

- stop 记来源 IP：agent 侧已提交（d15287d，写 run/exec.log），随下个版本上 PC2。
- /activate 应答与 execv 竞争导致 RST，调用方靠轮询 /version；可改为先回应答再 execv。
- T2 重跑盯：发端 203.5ms 间隔（batch69 后首次 >20ms）、外部 stop 是否复发。
- PC2：role 标签仍为 sender（仅显示）；计划任务/watchdog 是否覆盖 Documents 部署未核实。
- 直连线：owner 决定短期不考虑；线到后补 广播 vs 单播 T2。

### P4 产品链路（按 docs/spec/requirements.md；提案与批示见 docs/proposals/）

```text
P4-1 采集性能（第一步：gpu_ms 改为 D3D11 timestamp query 真 GPU 时间，出 2K240 实测分布再定门限）：DDA 2K240 → GPU 居中裁 640 → GPU RGB→NV12（整数矩阵，与 CPU 参考逐字节一致）。
     目标 240fps 下 gpu_ms<4ms、不丢采集帧；为 T5 定独立门限写入 test-matrix（需 owner 批）。
     无屏/独占全屏仍须显式报错，不许静默黑帧。
     步骤1进展 2026-10-23：`gpu_dispatch_ms` 真 GPU 测量已落地（sender/dda.h timestamp+disjoint + stats 新字段），本机自测 PASS（idle-SKIP，query 创建 ok）；2K240 分布与门限待 PC1 实测（D10）。
P4-2 端到端延迟测量：确认包（flags bit1 + 零 payload，28B 不动，实现时 owner 终审）；
     口径按 D8：PC1 采集完成→PC2 重组完成，分段同机 QPC，门限判全程 p99。
P4-3 PC2 虚拟采集卡：已定 DShow（用户态 source filter），实现
     NV12 640x640、AvgTimePerFrame=41666、只留最新 1 帧、不跟墙钟。
P4-4 虚拟卡出口验收（不含推理）：通用 DShow 客户端（如 ffmpeg -f dshow / GraphEdit）枚举到设备并协商 NV12 640x640@240；
     出口帧与发端逐字节一致（hash 对账）、FIFO 堆积为 0。推理端接入不在本项目范围（D8）。
范围外：推理端及其联调（D8）、独占全屏 Hook、分辨率超 2.5G 链路后的编解码方案（另立项，hist S24）。
owner 批示 2026-10-22：P4-1 不回退 CPU（显式报错）已批，门限数字待测量改对 + 实测分布后再定；P4-2 确认包格式可行，门限指标改为 R(N) 重组段（RTT/2 只做上界交叉验证）；P4-3 DShow 优先（对标 OBS 虚拟机，推理端 AverMedia USB 07CA 系），Win11 MF 降为备选。
```
