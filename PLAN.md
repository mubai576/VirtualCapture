# PLAN：VirtualCapture Phase 3（收口 → 双机放行 → 产品链路）

> 重写于 2026-10-22。旧版全文（含 T-01~T-10 原卡、§6~§39 流水账、S 编号证据）原样归档于
> `docs/plan-history.md`，凡引用 "S11/S23/…" 均指该文件，不要改它。
> 本文件只放：现状、冻结契约、待决事项、当前任务卡。完成一项就在卡头打 ✅ + 一行证据，不要再追加流水账。

## 0. 现状快照（2026-10-22）

| 维度 | 状态 |
|---|---|
| 传输正确性 | ✅ 双机 T0/T1 五档/T2 10min/T3/T4 全部零丢零错；全史 43 份报告 hash_mismatch=0（hist S36） |
| 240fps | ✅ 仅当 `--batch ≥23`（hist S23）；默认 batch=69 |
| 延迟抖动 | ✅ p99 全史 <8.3ms（最大 6.99）；❌ 冻结的 avg 门限在真网卡上不可用（drain 尾 + PC2 偶发 ~1s 卡顿，hist S32） |
| dda 采集 | 🟡 最小实现可用，仅 ~10fps；夜间无屏 selftest SKIP（D4） |
| 调用链止血（原 T-07） | ❌ DONE:`--adapter` 仍可空、无 `/adapters`、全错发送 exit 0、无 live 测试 |
| 构建/版本管理（原 T-08） | ❌ DONE:无 git、无 docs/build.md、CMake 不回退 thirdparty、`run/` 594 文件 |
| OTA | ✅ stage/activate + execv 接管已验证（本地 10×/冻结/CLI + PC2 推→回滚→再推全绿，含 T2 9MB 分页全量对账）；detached helper 已删 |
| 版本 | 本地 0.4.0（文件；主 agent 待重启生效） PC2 0.4.0（已验证 execv 接管 + 分页拉取）；release/vc-0.4.0.zip 为当前包 |
| 产品链路 | ⬜ 未开始：2K240 实时采集裁 640@240、端到端延迟 <4.16ms、PC2 虚拟采集卡、推理端联调 |

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
- 哈希 FNV-1a64（初值 14695981039346656037，prime 1099511628211，小写 16hex），整帧 614400B。

### 1.5 全局禁止
1. 不改包头尺寸/字段顺序。 2. 像素生成禁用浮点。 3. 链路禁止任何压缩。
4. 性能宣称必须用 `motion` 并注明 pattern/batch/adapter。 5. 跨机不报 QPC 差值延迟，只报 inter-arrival。
6. 不许拿回环数据冒充双机结论。 7. 不许放宽 test-matrix 判据（改判据只能经 D1）。

### 1.6 目录
`build/` `run/` 均 gitignore；任务只碰自己的目录（sender/ receiver/ agent/ ctrl/ scripts/ docs/）。

## 2. 待决事项（owner 2026-10-22 已拍板，批示见表）

| # | 事项 | 建议 | 解锁 |
|---|---|---|---|
| D1 | T-06 判据重定 | ✅批：节奏门=发端 cadence 4.166±0.3ms（PC1 本地 hashlog，不涉跨机时钟）；抖动门=收端 p99<8.3ms 保留；收端 avg 只判稳态窗口 | P2-2 |
| D2 | OTA 0.4.0 | ✅方案批（stage/activate 分离 + execv + 版本化 exe + GC + 全树 prev）；代码已随 P3 落地 e01af52，PC2 实推待做 | P3 |
| D3 | receiver hashlog 加 qpc_ns | ✅批：实际写点 vc_receiver.cpp:443，全文件唯一；reconcile 只比 frame_id+hash | P2-1 |
| D4 | dda selftest 无屏 SKIP | ✅批：有屏仍须 PASS；初始化失败但非无输出仍判 FAIL | P2-1 |
| D5 | `--batch` 默认值 | ✅批：改 69 | P2-1 |
| D6 | 0.3.7 re-stage | ✅批：作废，并入 0.3.8 | P2-1 |
| D7 | checker-transient | ✅保留并自动判：发端 hashlog 相邻帧间隔 >1s 由 gap_any.py 判（曾现 1713 帧停住 8.5s，删等于丢证据） | P2-3 |

## 3. 任务卡

派发顺序：`P1-1 × P1-2 × P1-3 并行` → `P2（需 D1/D3~D6 + 双机）` → `P3 × P4-1 并行` → `P4-2 → P4-3 → P4-4`。
每卡交付须附验收命令完整 stdout；不过打回。

---

### DONE P1-1 done

```text
先读 .agents/skills/virtualcapture/SKILL.md、PLAN §1、docs/plan-history.md §7。
1. sender：--adapter 缺省时按 pcap_findalldevs 顺序逐卡试发，首包成功即锁定；全部失败 →
   stderr 列出试过的卡+错误，exit 2。live 发送 sent_frames>0 且成功包为 0 → exit 3。
   --selftest/--list-adapters 行为不变。
2. agent：新增 GET /adapters（调 exe --list-adapters，返回 {text, names[]}）；
   /sender/start、/receiver/start 缺 adapter → 400，message 附本机 adapters 列表，不拉起进程。
   同步 references/agent-api.md。
3. ctrl：vc-ctrl.py --adapter 必填（缺省 exit 2，提示先 GET /adapters，不产生报告）。
   test_matrix.py 新增 test_T0_live_loopback / test_T1_live_loopback：读 VC_ADAPTER，未设 skip；
   motion 240Hz，frames=480，断言 dropped==0、hash_mismatch==0、send_errors==0、steady_state p99<8.3。
   T0 stub 用例改为显式传错误 adapter 并断言 400/非零退出，而不是断言"裸跑 FAIL"。
验收：
  build/vc-sender.exe --adapter NoSuchNic --frames 5 ...        # exit 2，列卡
  python ctrl/vc-ctrl.py --sender ... --receiver ... --seconds 5  # 无 --adapter 秒退 exit 2，run/ 无新报告
  VC_ADAPTER=Loopback pytest ctrl/tests -q                        # 全绿（live 两例实跑）
禁止：改包头；live 测试加 sleep 蒙混；放宽判据。
```

### DONE P1-2 done

```text
先读 PLAN §0/§1.2。
1. sender/receiver CMakeLists：NPCAP_SDK 未设时自动回退 ${CMAKE_SOURCE_DIR}/thirdparty/npcap-sdk，
   检查 Include/pcap.h，message 写明用了哪个路径。不依赖任何环境变量。
2. 删 build/ 从零 configure+build（记录确切命令与编译链），双 selftest PASS（dda 夜间 FAIL 可接受，
   注明；D4 落地后应为 SKIP）。命令写进 docs/build.md（含 PyInstaller 打 vc-agent.exe、打 release zip）。
3. git init；.gitignore 补 thirdparty/ninja-win.zip、release/、pkg*/、staging/、prev/、*.bak*、
   *.exe、*.zip、*.log；首 commit，交付 git log --oneline。
4. 治理：pkg034x/ pkg035/ PLAN.md.bak-pre034 agent/*.bak-* 移到 archive/（gitignore）；
   scripts/ 一次性分析脚本移到 scripts/analysis/（保留，不删）；run/ 按策略归档：
   保留所有 report-*.json + 被报告/文档引用的 session 文件 + 最近 5 轮，其余进 run/archive.d/。
   策略写进 docs/build.md，前后文件数写进交付说明。
验收：rm -rf build 后按 docs/build.md 重编通过；git log 可见；find run -maxdepth 1 -type f | wc -l < 60。
禁止：二进制入 git；删 PASS 报告；删 prev/ staging/ 中正被 agent 使用的目录（只在 PC1 本地归档测试残留）。
```

### DONE P1-3 done

```text
1. 部署方案.md §9：scripts 标 DONE（含 install-pc2/set_nic/watchdog），TODO 只留真双机/直连线项；
   C:\vc 路径加注 "PC2 现行为 Documents 手动部署（hist S33）"。
2. SKILL.md：Quickstart 加 "先 GET /adapters 查卡、--adapter 必填、双机加 --batch 69 --dst-mac"；
   PLAN S 编号引用改为 docs/plan-history.md S 编号。
3. references/test-matrix.md：D1 批准后按新判据改写（只在 D1 落地后做）。
验收：grep -rn "未实现\|无 compiler" .agents/ 部署方案.md PLAN.md → 无命中。
禁止：改阈值数字（D1 除外）；改代码。
```

### DONE P2 done（2026-10-22 晚，T-06 关闭）

```text
P2-1 代码：receiver hashlog 加 qpc_ns（D3）；dda selftest 无屏 SKIP（D4）；--batch 默认 69（D5）；
     ctrl 判分按 D1 实现并单测锁定新数字；version.json=0.3.8；按 docs/build.md 打 release/vc-0.3.8.zip。
P2-2 安装：install-pc2.ps1 手工通道装 PC2（禁止 POST /update），双端 /version 一致。
进展（2026-10-22 晚）：PC2 已手工装上 0.3.8（冻结版，18080）；同版本热更新实证通过（PID 已换代），双端 /version 一致，P2-2 实质完成；待 P2-3 联调。
P2-3 联调（scripts/run_T1.ps1 / ctrl/bringup.py，单播 --dst-mac）：T0 → T1 五档 → T2 10min soak；
     每次结束立即 /files 拉回 receiver hashlog 做全量 reconcile（hist S35 教训）。
     用 receiver qpc_ns 定位 ~1s 卡顿（gap_any.py 两端同法），给出 PC2 侧根因假设。
     直连线：owner 决定短期不考虑，广播 vs 单播 T2 dropped。
验收：docs/bringup-<日期>.md；T1 五档 + T2 按新判据全绿；全部 receiver hashlog 本地留档；T-06 关闭。
交付：docs/bringup-20261022.md；T0 + T1 五档 + T2 144000/144000 全绿（rings 判据 bug 修一次、noise 外部 stop 重跑一次）；hashlog 留档（T2 9MB 已用 /files 分页全量拉回并离线对账零错） |
```

### DONE P3 done（2026-10-22 晚，PC2 推→回滚→再推全绿）

```text
先读 docs/plan-history.md S19/S29/S31/S37。
1. POST /update 只 stage（校验+解压+全树 prev 快照，不重启），返回 staged 版本；新增 POST /activate。
2. 脚本模式：py_compile 后 os.execv 原地重启；冻结 exe：vc-agent-<ver>.exe 并存 + version.json 指针 + execv，
   启动时 GC 非当前 exe 与 staging/mirror-*。
3. rollback = activate prev 指针。全流程不得存在 detached 子进程。
4. update.ps1 / run_remote --update 改为 stage→activate→轮询 /version。
验收：test_update_roundtrip_and_rollback 连跑 10 次全绿；PC2 实推 0.4.0→回滚→再推各一次成功。
禁止：保留任何依赖"父进程退出后存活"的路径。
进展（本地，e01af52）：test_update_roundtrip_and_rollback 10×全绿；run_remote --update/--rollback 对临时 agent 端到端 exit 0；待 PC2 实推 0.4.0→回滚→再推。
进展（冻结模式，本地验证）：dist/vc-agent.exe 冷启动 0.3.8 → stage/activate 到 9.9.98-frozen（版本化 exe + 指针切换，单监听）→ rollback 回 0.3.8；run_remote --update/--rollback 均 exit 0。
进展（PC2，2026-10-22）：0.3.8 已上线；同版本 stage→activate 实证通过（PID 10316→2528，单监听，updates.log 有链）；注意 prev 快照停在 0.3.5（手工安装不快照），首次回滚将回到 0.3.5 而非 0.3.7，下一次异版本 OTA 后自动纠正；0.4.0 实推待发版。
交付（PC2，2026-10-22）：0.4.0 同版本 stage→activate 接管（PID 换代）；T2 hashlog 9MB 分页全量拉回并对账 144000/144000 零错；rollback 回 0.3.8；re-push 回 0.4.0。全部 exit 0，单监听。
后续（未排期，不阻塞 DONE）：receiver stop 日志记来源 IP（排障用）；activate 应答与 execv race 仍返回 RST，轮询确认为准。 stop 来源 IP 记录排入下个版本。
```

### P4 产品链路（新立项，按 需求文档.md；P4-1 可与 P3 并行）

```text
P4-1 采集性能：DDA 2K240 → GPU 居中裁 640 → GPU RGB→NV12（整数矩阵，与 CPU 参考逐字节一致）。
     目标 240fps 下 gpu_ms<4ms、不丢采集帧；为 T5 定独立门限写入 test-matrix（需 owner 批）。
     无屏/独占全屏仍须显式报错，不许静默黑帧。
P4-2 端到端延迟测量：设计同钟测法（PC2 收齐帧后回发 1 个小确认包，PC1 用同一 QPC 算 RTT/2 上界），
     报 采集→重组 延迟分布，目标 <4.16ms。确认包格式需 owner 批（proto 变更）。
P4-3 PC2 虚拟采集卡：先出 DShow vs MF 选型报告（MF 可见性、推理端开卡方式），批后实现
     NV12 640x640、AvgTimePerFrame=41666、只留最新 1 帧、不跟墙钟。
P4-4 推理端联调：枚举到虚拟设备、逐字节无损（与发端 hash 对账）、FIFO 堆积为 0。
范围外：独占全屏 Hook、分辨率超 2.5G 链路后的编解码方案（另立项，hist S24）。
owner 批示 2026-10-22：P4-1 不回退 CPU（显式报错）已批，门限数字待测量改对 + 实测分布后再定；P4-2 确认包格式可行，门限指标改为 R(N) 重组段（RTT/2 只做上界交叉验证）；P4-3 DShow 优先（对标 OBS 虚拟机，推理端 AverMedia USB 07CA 系），Win11 MF 降为备选。
```
