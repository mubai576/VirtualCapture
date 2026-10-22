# Test matrix T0–T5（ctrl 一键跑）

执行顺序：T0 不过不联双机；T1 不过不进 soak；T3 反例跑一次归档即可。

| 用例 | 条件 | 判据 |
|---|---|---|
| T0 loopback | 同机自环，`motion` 240Hz×10s | hash 0 错（无 PC2 可开工） |
| T1 linked | 双机 240Hz×60s，MTU9000，五档各一轮（放行以 `motion` 为准） | `complete==sent, dropped==0, hash_mismatch==0` |
| T2 soak | 双机 `motion` 240Hz×10min | `pps≈16560±2%, bps≈1.18G, inter-arrival p99<8.3ms(2帧)` |
| T3 mtu1500反例 | MTU1500×10s | 预期 `pps≈101k` 且丢帧>0（证明 1500 不可用，归档） |
| T4 pacing对照 | `pacing 0/50/200us` | 选 `dropped` vs CPU 最优为默认 |
| T5 dda摸底 | 无边框 2K→640，240Hz×30s | 只看 `gpu_ms/pps/CPU`，不判 hash |

D1 gates approved 2026-10-22, replacing the avg gate above:
Rhythm is sender cadence from local sender hashlog qpc span, 4.166+-0.3ms, no cross-clock involved.
Jitter keeps receiver p99 below 8.3ms. Receiver avg is judged on the steady window only, the final ring contains drain tail.
Rest unchanged: dropped==0 and hash_mismatch==0 and complete==sent, senderCPU below 25 percent, receiverCPU below 30 percent.

## 失败分诊

1. `dropped>0` → 查 MTU 两端一致？EEE/中断合并关了吗？跑 T4 区分突发丢 vs 持续丢；
   再看速率协商（2.5G 跌到 1G 则 1.18G 必挂）。
2. `hash_mismatch>0` → 先看包头 CRC：CRC 错=传输损坏；CRC 对但帧 hash 错=生成/重组 bug。
3. pps 只有一半 → 对照双端 stats：`sent` 少=sender 生成瓶颈；`sent` 够 `recv` 少=抓包丢。
4. 版本错位 → ctrl 开跑前必须比对双端 `/version`，不一致直接 fail。

## 跨机对时/日志

- Log lines D3: sender and receiver rows both carry qpc_ns; reconcile compares frame_id and hash only. Adjacent-frame gap over 1s flags checker-transient D7, auto by gap_any.py.
- 控制面 NTP 对时（ms 级够用）；数据面不跑对时协议。
- 失败自动 `POST /pcap/start` 留证 + `/receiver/report` 打包。

## Dual-NIC notes 2026-10-21 (thresholds frozen; these are advisories, see docs/plan-history.md S12-S14)
- verdict avg uses the terminal 120-sample ring and end-drain dominates it. Steady state and sender_cadence are reported. Gates updated by D1 above.
- sender ceiling on real NIC is ~200fps (pcap_sendpacket backpressure; loopback does 240). T2 pps baseline ~14000, not 16560. Recalibration is T-10 work, not a quiet edit. Update 2026-10-22: sendqueue batching (--batch 23/69) reaches the 240fps target (docs/plan-history.md S23); ~200fps was the single-packet ceiling.
- broadcast dst hangs the sender (~350s, zero errors, alive): suspect switch storm suppression. Default to unicast --dst-mac <peer-MAC> until direct-cable retest. --dst-mac takes colon format.
- mtu1500 on real NIC is slow-not-lossy (sender 69fps syscall-bound, pps 28k). The 101k-lossy counterexample needs a faster sender first.
- dda mode (--mode dda): pattern 255 skips pixel verify, hash still reconciled; idle screen reuses last frame; 240Hz redlines do not apply, needs own gates.
