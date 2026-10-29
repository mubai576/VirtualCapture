# PLAN：VirtualCapture Phase 3（收口 ✅ → 双机放行 ✅ → 产品链路 P4）

> 入口与规则见根目录 `AGENTS.md`。本文件只放：现状、冻结契约、待决事项、当前任务卡。
> 完成一项在卡头标 DONE + 一行证据；长证据写 `docs/bringup/` 或 `docs/proposals/`，这里只放链接。
> 档案（只读）：旧 PLAN 全文与 "hist S11/S23/…" 编号 → `docs/history/plan-history.md`；
> 已关闭的 P1~P3 卡原文 → `docs/history/phase3-closed-cards.md`。

## 0. 现状快照（2026-10-23 EOD）

| 维度 | 状态 |
|---|---|
| 传输正确性 | ✅ 双机 T0/T1 五档/T2 10min 零丢零错；T2 144000 帧 PC2 hashlog 分页全量对账零错（docs/bringup/bringup-20261022.md） |
| 240fps | ✅ DDA 实测 235fps（testwin D3D11 动画源），batch=69，gpu_convert=on |
| 延迟 | ✅ D14 网卡调优后全程 p99=9.83ms（batch=69）；rseg_max 1015ms→35ms（-97%）；Npcap 用户态实用极限约 10ms |
| P4-1 GPU 采集 | ✅ gpu_dispatch_ms=0.028ms（D3D11 timestamp，235fps 实测）；gpu_convert 代码落地，默认 CPU 路径待 owner 切换 |
| P4-2 延迟测量 | ✅ 分段计时落地（pc1_seg + rseg）；双机实测门限候选 <10ms；确认包（flags bit1）待 owner 终审 |
| P4-3 虚拟采集卡 | ✅ vcam DShow filter 落地，本机回环 4455 帧逐字节零错，240.0fps，D12 无信号行为实现；PC2 接线验收待 P4-4 |
| P4-4 虚拟卡验收 | ✅ DONE 2026-10-23：三判据全通（J1 枚举/J2 NV12 640x640@240/J3 35069帧零错+SHM hash 27/27一致）；tools/vcam_verify/ + scripts/p4_4_shm_verify.py；docs/bringup/bringup-20261023-p4-4-vcam-acceptance.md |
| 产品链路 P4 | ✅ P4-1/2/3/4 全部完成 |
| 网卡调优 | ✅ PC2 I226-V: FlowControl=0, ITR=0（D14，docs/runbooks/pc2-nic-tuning.md）；PC1 Realtek 已同等设置 |
| 版本 | 本机/PC1 0.4.1（scripts/PS1/sender 已更新）；PC2 主 agent 0.4.0（D9 待手动重启） |

环境：MSVC 19.44（cmake VS 生成器直连，cl 不在 PATH）、python 3.14、Npcap 运行时、
`thirdparty/npcap-sdk` + `thirdparty/ninja` 已 vendor。拓扑：同网口方案，PC1=192.168.2.100，
PC2=192.168.2.59（agent 手动跑在 PC2 的 Documents\VirtualCapture，exe 在根目录），联调发单播 `--dst-mac`。

## 1. 冻结契约（改即全线返工，须 owner 批准）

### 1.1 包头与链路
- `proto/vc_wire.h`：`vc_pkt_hdr_t` 28B，EtherType `0x88B5`，`payload = mtu - 42`，
  `total = ceil(614400/payload)`。proto/ 归 owner 锁定。

### 1.2 构建
- 根 `CMakeLists.txt` 编排 `common/`、`sender/`、`receiver/`、`vcam/` 与 `tools/`，MSVC 优先；统一规则在 `cmake/`。默认 `-B build` 下发收端入口仍为 `build/vc-sender.exe`、`build/vc-receiver.exe`；各目标路径见 `docs/runbooks/build.md`。
- 一键便携包：`python scripts/package.py [--allow-dirty] [--output <目录>]`；默认 `dist/VirtualCapture/`，包含 7 个原生目标、冻结 agent（含 GUI）、MSVC 运行库、脚本和文档。生成文件 SHA-256 与源码来源清单；成功才替换，旧成品整目录归档。契约见 skill `references/packaging.md`。

### 1.3 CLI（动词级冻结；dda/batch 为已落地的追加，本次正式纳入）
```text
vc-sender.exe --adapter <子串> --mode pattern|file|dda --pattern motion --fps 240
  --frames 0 --ethertype 0x88B5 --mtu 9000 --pacing-us 0 --batch 69 --dst-mac ff:ff:ff:ff:ff:ff
  --session <id> --stats <path> --hashlog <path> [--file <nv12> --loop] [--list-adapters] [--selftest] [--gpu-convert]
vc-receiver.exe --adapter <子串> --ethertype 0x88B5 --mtu 9000
  --session <id> --stats <path> --hashlog <path> --timeout-s 0 [--expect-frames N] [--shm <mapname>] [--selftest]
```

### 1.4 文件型 IPC
- stats ~500ms 原子覆写（tmp→rename）；hashlog 每帧 append 一行。
- `sender.hash.jsonl`：`{"frame_id":N,"hash":"<16hex>","qpc_ns":T}`
- receiver hashlog D3 carries qpc_ns; reconcile compares frame_id and hash only.
- vcam DShow 设备契约（2026-10-25）：FriendlyName `VirtualCapture`，pin `Capture`（PIN_CATEGORY_CAPTURE），仿 GC573 枚举为 `(video)`；HKCU CLSID 键下可选 REG_SZ `FrameMap`、REG_DWORD `Motion`=1（D12 显式 bring-up）。
- receiver `--shm <name>` (P4-3): publish latest complete frame to named file mapping (layout vcam/vcam_shm.h, NOT proto/); stats adds `shm_published`; agent /receiver/start forwards `shm`.
- **shm v2（D15，2026-10-28）**：`vcam/vcam_shm.h` 为唯一布局（magic `VCA2`，12 槽 ring，数据区 4096 起）；receiver 原地重组进空闲槽，发布=翻槽 seq + 置命名事件 `<name>.evt0/.evt1`（manual-reset 交替），整帧哈希在发布后由哈希线程写入（`hash_fid==frame_id` 才有效）；vcam 事件驱动（新帧立即交付，静默 1.5 帧后才重复上一帧，D12 不变）。v1 读端遇 v2 映射判"无信号"，不崩。Python 镜像 `scripts/vc_shm.py`，monitor 内联镜像。
- vcam→receiver 回写环 `<name>.vcam`（`vc_vstat_*`，仅测量）：receiver stats 新增 `vseg_avg_ms/vseg_p50_ms/vseg_p99_ms/vseg_max_ms`（receiver 发布→vcam 交付下游，同机 QPC，不含重复帧）、`vcam_delivered`、`vcam_repeats`、`hash_queue_max`；sender stats 新增 `cap_lag_avg_ms/cap_lag_p99_ms/cap_lag_max_ms`（dda：DXGI LastPresentTime→转换完成，同机 QPC）。
- receiver hashlog `qpc_ns` = 重组完成时刻（原为当次循环起点，差 <1 包）；sender PC1 段不再含整帧哈希（哈希移到末包之后）。dda 模式不再按 `--fps` 睡眠节流，由 AcquireNextFrame 节拍。
- sender stats 新增 `gpu_dispatch_ms`（D3D11 timestamp 包住 CopyResource 的真 GPU 时间均值）+ `gpu_dispatch_samples`（有效样本数）；`gpu_ms` 仍为 capture() wall-clock，待 P4-1 实测分布后按 D10 决定是否切换口径。
- sender stats 新增 `gpu_convert`（0/1，采集走 CPU 还是 GPU compute shader；GPU 失败显式报错，不静默回退）。
- P4-2步骤1（免协议改动）：receiver stats 新增 `rseg_avg_ms/rseg_p50_ms/rseg_p99_ms/rseg_max_ms`（R(N)=首包到达→重组完成，同机QPC）；sender stats 新增 `pc1_seg_avg_ms/pc1_seg_p99_ms/pc1_seg_max_ms`（PC1段=采集/生成完成→末包发出，同机QPC）；确认包（flags bit1）仍待 owner 终审后做步骤2。
- P5 GUI（D16）：agent 新增 `/ui`、`/config`（ROOT/vc-config.json）、`/vcam/status|register`、`/product/{start,stop,status}`、`/product/local/{start,stop}`；`/sender/start` 可选 `gpu_convert`。详见 agent-api.md。
- 哈希 FNV-1a64（初值 14695981039346656037，prime 1099511628211，小写 16hex），整帧 614400B。

### 1.5 全局禁止
1. 不改包头尺寸/字段顺序。 2. 像素生成禁用浮点。 3. 链路禁止任何压缩。
4. 性能宣称必须用 `motion` 并注明 pattern/batch/adapter。 5. 跨机不报 QPC 差值延迟，只报 inter-arrival。
6. 不许拿回环数据冒充双机结论。 7. 不许放宽 test-matrix 判据（改判据只能经 D1）。
注：UNORM 解码允许 float（B8G8R8A8_UNORM→float→floor(v*255+0.5) 属格式转换，非像素生成；结果必须和 CPU 参考逐字节一致，dda_gpu 自测已证）；不许拿它当先例去别处用 float。

### 1.6 目录
以 `AGENTS.md` Repo map 为准；产物与运行数据全部 gitignore；任务只碰自己的目录。共享 CRC32/FNV 统一在 `common/`，原生辅助工具源码归 `tools/`；`proto/vc_wire.h`、`sender/pattern.h`、`vcam/vcam_shm.h` 的契约位置保留。

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
| D8 | 范围与 P4-2 门限口径 | ✅批（2026-10-23）：本项目只管**采集与传输**，推理不在范围内。延迟口径 = PC1 采集完成 → PC2 重组完成，分段同机 QPC 测；**门限候选 <10ms（全程 p99）**：双机实测 9.83ms（batch=69 + D14 网卡调优），Npcap 用户态实用极限；确认包（flags bit1）待 owner 终审 | P4-2 |
| D11 | noise 种子 + 只读映射 + 撕裂回退 | ✅批（2026-10-24，三项全做）：①noise 种子 `fid<<32|C` 与 C 高 16 位 0x1234 置位 bit{2,4,5,9,12} 重叠，1500帧只有128独立帧/T1 14400帧512个/T2 144000帧4608个（约每32帧不同）；已改 XOR（C++/Python/small64 三处，VEC 已重核）；历史 noise 对账仍有效（逐帧比），防压缩/去重主张被削弱，motion 不受影响（XOR 种子）；②读端禁 LOCK 前缀（已改 plain volatile 读）；③撕裂读跳过该 tick（缺口由 discontinuity 标，motion 只保留给无映射 bring-up） | P4-3 |
| D12 | vcam 无信号默认行为 | ✅定（2026-10-23，owner 授权 AI 定；owner 可推翻）：receiver 未运行/从无有效帧时 vcam 不输出任何样本（下游看到停顿而非假运动画面）；motion 帧源改为显式 bring-up 开关（默认关）；有过有效帧后撕裂/停顿则重复最后一帧好数据。实现完成 7e7ce80（vcam.cpp：m_lastGoodFrame/m_hasLastGood/m_motionEnable，三态全改，verify.py ALL PASS） | vcam |
| D13 | UNORM 解码 float 例外 | ✅定（2026-10-23，owner 授权 AI 定；owner 可推翻）：硬规则第 2 条禁的是“生成像素用浮点”；B8G8R8A8_UNORM→float→floor(v*255+0.5) 属格式转换，允许用 float，但结果必须和 CPU 参考逐字节一致（dda_gpu 自测 5 档已证）。§1.5 已注，不许扩散 | — |
| D10 | P4-1 测量口径与 idle 语义 | ✅批（2026-10-24）：`gpu_ms` 切真 GPU 时间待 PC1 2K240 实测分布后再定，`gpu_dispatch_ms` 先作诊断；dda selftest 连续 5x E_TIMEOUT（2.5s 无新帧）判 SKIP（idle 与故障不可分，live 已容忍超时） | P4-1 |
| D14 | PC2 网卡调优（I226-V） | ✅执行（2026-10-23，AI 操作，owner 可回滚）：FlowControl=0、ITR=0、EEE=0、InterruptModeration=0；实测 rseg_max 1015ms→35ms（-97%），rseg_p99 8.3ms→6.7ms（-19%），全程 p99 12ms→9.8ms；步骤见 docs/runbooks/pc2-nic-tuning.md；回滚命令见同文件 | P4-2 |
| D15 | 端到端延迟优化 1+2 组（含 shm v2 IPC 变更） | ✅批（2026-10-28，owner"1 2 一起优化"）：见 §1.4 shm v2 条；本机回环 motion/240/batch1 实测 pc1_seg 1.03→0.37ms、rseg 2.34→0.39ms、vcam 出口 vseg avg 0.03ms（旧轮询理论 +2.1ms）；双机（PC2 发→PC1 收+vcam，motion/240/batch69）rseg p99 6.70→2.05ms、max 35→2.26ms，vseg p99 0.08ms，7200 帧对账零错；证据 docs/bringup/bringup-20261028-latency-opt.md。PC2 已推 0.4.2（发版号 AI 暂定，owner 可改）；v1/v2 不互通，下游须重开 | ✅已部署 |
| D16 | P5 GUI 整合方案 | ✅批 A（2026-10-29，owner）：agent 内置 Web UI；首批 G0 统一配置 + G1 仪表 + G2 一键开播已落地（agent/vc-agent.py、ctrl/tests/test_gui.py，操作见 docs/runbooks/gui.md）。未定：WebView2 外壳、访问控制，按默认（两台都可开控制台、无鉴权）先行 | P5 G3/G4 |
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

### S1 项目结构整理 — DONE（2026-10-29）

`common/` 合并重复校验头、`tools/` 收纳原生辅助工具、`cmake/` 统一规则；verify ALL PASS（28 passed / 3 skipped）+ 独立目录干净构建通过。证据：`docs/proposals/project-structure-20261029.md`。

### S2 一键编译打包 — DONE（2026-10-29）

`python scripts/package.py --allow-dirty` 实际生成 `dist/VirtualCapture/`（含当前 GUI）；verify ALL PASS（42 passed / 3 skipped）+ 成品隔离冒烟/PowerShell 5.1 检查/79 项载荷哈希通过。证据：`docs/proposals/packaging-20261029.md`。

### P4 产品链路（按 docs/spec/requirements.md；提案与批示见 docs/proposals/）

```text
P4-1 采集性能：DONE（代码）
     gpu_dispatch_ms=0.028ms（D3D11 timestamp，双机 DDA 235fps 实测，2026-10-23）。
     GPU compute shader（居中裁+整数BT.601）落地，--gpu-convert 显式开启，失败报错不回退。
     待 owner 决策：①gpu_convert 设为默认开（D10）；②T5 门限写入 test-matrix。
P4-2 端到端延迟测量：DONE（分段计时代码）
     双机实测：pc1_seg_p99=3.14ms，rseg_p99=6.70ms，全程 p99=9.83ms（batch=69，D14 网卡调优后）。
     batch 对比（优化后）：batch=69 最优（全程 9.27ms，rseg_max=9.9ms 最稳定）。
     待 owner 决策：①全程 p99 门限拍板（建议 <10ms）；②确认包（flags bit1）是否实现。
P4-3 PC2 虚拟采集卡：DONE（代码+本机验证）
     vcam DShow filter 落地，本机回环 4455 帧逐字节零错，240.0fps QPC 起搏。
     D12 无信号默认行为实现（7e7ce80）：无映射→stall，撕裂+有历史帧→repeat-last，正常→输出。
     OTA 白名单已放行 vc-vcam.dll；验收手册：docs/runbooks/pc2-vcam-acceptance.md。
P4-4 虚拟卡出口验收（不含推理）：DONE 2026-10-23
     J1 PASS：vc-vcam-verify.exe（tools/vcam_verify/ C++）ICreateDevEnum 枚举到 VirtualCapture 640x640 NV12（2026-10-25 改名为 `VirtualCapture`，见 docs/runbooks/vcam.md）。
     J2 PASS：IAMStreamConfig GetFormat = NV12 640x640 AvgTimePerFrame=41666 (240.0fps)。
     J3 PASS：35069帧 shm_published==complete, dropped=0, hash_mismatch=0；SHM FNV-1a64 采样 27/27 与 receiver hashlog 一致。
     验收工具：build/vcam_verify/Release/vc-vcam-verify.exe + scripts/p4_4_shm_verify.py。
     证据：docs/bringup/bringup-20261023-p4-4-vcam-acceptance.md。
范围外：推理端及其联调（D8）、独占全屏 Hook、分辨率超 2.5G 链路后的编解码方案（另立项，hist S24）。
owner 待决：①全程延迟门限 <10ms；②gpu_convert 默认开；③确认包是否实现；④T5 门限数字。
```
