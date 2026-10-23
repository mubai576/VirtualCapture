# bringup-20261024-vcam-outlet（P4-3 shm 通路 + 出口逐字节对账）

本机回环：sender(noise)→receiver(`--shm`)→shm→vcam→NullRenderer，全绿。
帧源切换为 noise，保证 motion-fallback 泄漏可被检出（见下）。

## 链路证据（outlet8 会话，loopback + batch 1）
- sender：1200 帧 noise，60fps，`--batch 1`（回环 batch>1 每 transmit 只抓 1 包，见下）。
- receiver：`complete=1200 dropped=0 packets_lost=0 hash_mismatch=0 shm_published=1200`。
- vcam outlet（NullRenderer 20s，并发窗口）：4455 样本，**全 `shm:1`**（零 motion fallback 泄漏在此轮），
  出口 hash 与发端 hashlog 对账：**outlet ⊆ sender，foreign=0**（逐字节一致）。
- sender ⊆ outlet 为 False（missing=8，均为 probe 窗口之外的 tail fid），系窗口覆盖问题，非产品缺口：
  读端 240Hz 严格快于写端（本轮写端 30fps），已发布帧必被多次读取。

## 修过的真 bug（P1 级）：只读映射上用 LOCK 前缀指令
- 现象：vcam 切到 shm:1 通路后间歇 AV（fallback/静态映射轮次全 clean，极具迷惑性）。
- 根因：读端用 `InterlockedOr(&seq, 0)` 采 seq——Interlocked 系列是 read-modify-write，
  即使 OR 0 也会发写周期；view 以 FILE_MAP_READ 映射，只读页写必崩。
  对齐、无撕裂都救不了：崩的是“写”这个动作本身。
- 修复：读端改 plain volatile 读（x86/x64 对齐 32 位读天生原子；写端保留 Interlocked），
  重跑 20s/4455 样本零崩。教训已记入 PLAN D11：**只读映射上禁 LOCK 前缀**。
- 附带发现：torn-read 保护（seq 奇偶 + 双读比对）实测生效——4455 样本中 5 次 fallback，
  全部为已知 motion 向量（vcam 本地 fid 0..4），无一 corrupt 字节流出。

## 回环 batch 说明（repo 级发现）
- Npcap loopback 上 sender `--batch 69`（sendqueue 一次 transmit）只被抓到约 1 包/次；
  `--batch 1` 全量交付。T0 用 batch=1 故不受影响；回环结论不得外推双机。

## noise 模式多样性说明（D11 已批已改，2026-10-24）
- 原种子 `((uint64_t)fid<<32)|0x123456789ABC`：C 高 16 位 0x1234 置位 bit{2,4,5,9,12}，
  实测 1500 帧只有 **128 个独立 hash**；T1 14400 帧 512 个，T2 144000 帧 4608 个（约每 32 帧不同）。
  历史 noise 对账仍有效（逐帧比）；防压缩/去重主张被削弱；motion 不受影响（XOR 种子）。
- 已改 XOR（pattern.h + small64 + pattern_ref.py 三处，VEC 重核，300 帧回环零错配、300/300 独立）。

## 240fps 起搏（本轮新增，产品代码）
- Worker 改 QPC 节拍：4.166666ms 周期，Sleep 粗睡 + spin 收尾（对标 sender sleep_us），
  timeBeginPeriod(1)，>20ms 落后重同步（不追赶积压，契合“只留最新”）。
- NullRenderer 10s 实测：tick=2400 @ elapsed=10000ms，**240.0fps**，逐秒 240 tick 对齐。
  （测量用临时计数，产品代码仅留 pacing 本体。）
