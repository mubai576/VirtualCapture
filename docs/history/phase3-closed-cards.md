# Phase 3 已关闭任务卡原文（P1-1~P3，2026-10-22）

> 2026-10-23 从 PLAN.md §3 原样迁出，只读档案。现行进度见 PLAN.md。

### DONE P1-1 done

```text
先读 .agents/skills/virtualcapture/SKILL.md、PLAN §1、docs/history/plan-history.md §7。
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
   注明；D4 落地后应为 SKIP）。命令写进 docs/runbooks/build.md（含 PyInstaller 打 vc-agent.exe、打 release zip）。
3. git init；.gitignore 补 thirdparty/ninja-win.zip、release/、pkg*/、staging/、prev/、*.bak*、
   *.exe、*.zip、*.log；首 commit，交付 git log --oneline。
4. 治理：pkg034x/ pkg035/ PLAN.md.bak-pre034 agent/*.bak-* 移到 archive/（gitignore）；
   scripts/ 一次性分析脚本移到 scripts/analysis/（保留，不删）；run/ 按策略归档：
   保留所有 report-*.json + 被报告/文档引用的 session 文件 + 最近 5 轮，其余进 run/archive.d/。
   策略写进 docs/runbooks/build.md，前后文件数写进交付说明。
验收：rm -rf build 后按 docs/runbooks/build.md 重编通过；git log 可见；find run -maxdepth 1 -type f | wc -l < 60。
禁止：二进制入 git；删 PASS 报告；删 prev/ staging/ 中正被 agent 使用的目录（只在 PC1 本地归档测试残留）。
```

### DONE P1-3 done

```text
1. docs/spec/deployment.md §9：scripts 标 DONE（含 install-pc2/set_nic/watchdog），TODO 只留真双机/直连线项；
   C:\vc 路径加注 "PC2 现行为 Documents 手动部署（hist S33）"。
2. SKILL.md：Quickstart 加 "先 GET /adapters 查卡、--adapter 必填、双机加 --batch 69 --dst-mac"；
   PLAN S 编号引用改为 docs/history/plan-history.md S 编号。
3. references/test-matrix.md：D1 批准后按新判据改写（只在 D1 落地后做）。
验收：grep -rn "未实现\|无 compiler" .agents/ docs/spec/deployment.md PLAN.md → 无命中。
禁止：改阈值数字（D1 除外）；改代码。
```

### DONE P2 done（2026-10-22 晚，T-06 关闭）

```text
P2-1 代码：receiver hashlog 加 qpc_ns（D3）；dda selftest 无屏 SKIP（D4）；--batch 默认 69（D5）；
     ctrl 判分按 D1 实现并单测锁定新数字；version.json=0.3.8；按 docs/runbooks/build.md 打 release/vc-0.3.8.zip。
P2-2 安装：install-pc2.ps1 手工通道装 PC2（禁止 POST /update），双端 /version 一致。
进展（2026-10-22 晚）：PC2 已手工装上 0.3.8（冻结版，18080）；同版本热更新实证通过（PID 已换代），双端 /version 一致，P2-2 实质完成；待 P2-3 联调。
P2-3 联调（scripts/run_T1.ps1 / ctrl/bringup.py，单播 --dst-mac）：T0 → T1 五档 → T2 10min soak；
     每次结束立即 /files 拉回 receiver hashlog 做全量 reconcile（hist S35 教训）。
     用 receiver qpc_ns 定位 ~1s 卡顿（gap_any.py 两端同法），给出 PC2 侧根因假设。
     直连线：owner 决定短期不考虑，广播 vs 单播 T2 dropped。
验收：docs/bringup/bringup-<日期>.md；T1 五档 + T2 按新判据全绿；全部 receiver hashlog 本地留档；T-06 关闭。
交付：docs/bringup/bringup-20261022.md；T0 + T1 五档 + T2 144000/144000 全绿（rings 判据 bug 修一次、noise 外部 stop 重跑一次） |
  T-06 关闭备注（owner 2026-10-22 定稿）：/files 实际限制有两层——>4MB 直接 413；<4MB 只返前 256KB（FILES_MAX_BYTES=262144）。T1×5 + T2 的 PC2 hashlog 离线只覆盖头部约 27%（3929–3988/14400）和 0%，判定依据是 PC2 端运行中的全量校验（mm=0）。P2-3 遗留：receiver qpc_ns ~1s 卡顿定位（截断 hashlog 内 max_gap 7.8–14.1ms、>20ms 为 0，T2 全程待分页补查）；T2 发端 203.5ms 间隔一次（按 D7 >1s 才算 transient，不判 fail，batch69 后首次 >20ms，T2 重跑留意）。16:08 T0/T1（report-T0-20261022-160857、report-T1-20261022-160905）系 owner 本人 VC_ADAPTER=Loopback pytest -k live，报告留 run/、session 已进 archive.d；hashlog 留档（T2 9MB 已用 /files 分页全量拉回并离线对账零错） |
```

### DONE P3 done（2026-10-22 晚，PC2 推→回滚→再推全绿）

```text
先读 docs/history/plan-history.md S19/S29/S31/S37。
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

