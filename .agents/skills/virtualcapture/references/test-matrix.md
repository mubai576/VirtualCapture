# 冻结验收矩阵 T0–T5

执行顺序：T0不过不联双机；T1不过不进长稳；T3反例跑一次归档即可。
下表历史门限原样保留，本次移除旧工具不意味着改判据。

| 用例 | 条件 | 判据 |
|---|---|---|
| T0 loopback | 同机自环，`motion` 240Hz×10s | hash 0 错（无 PC2 可开工） |
| T1 linked | 双机 240Hz×60s，MTU9000，五档各一轮（放行以 `motion` 为准） | `complete==sent, dropped==0, hash_mismatch==0` |
| T2 soak | 双机 `motion` 240Hz×10min | `pps≈16560±2%, bps≈1.18G, inter-arrival p99<8.3ms(2帧)` |
| T3 mtu1500反例 | MTU1500×10s | 预期 `pps≈101k` 且丢帧>0（证明 1500 不可用，归档） |
| T4 pacing对照 | `pacing 0/50/200us` | 选 `dropped` vs CPU 最优为默认 |
| T5 dda摸底 | 无边框 2K→640，240Hz×30s | 只看 `gpu_ms/pps/CPU`，不判 hash |


## D1（2026-10-22 批准）

节奏门采用发送端本地hashlog cadence：4.166±0.3ms，不涉跨机时钟；抖动仍为收端p99<8.3ms。
接收avg只判稳态窗口，末尾ring含drain尾段。其余仍要求 complete==sent、dropped==0、hash_mismatch==0、senderCPU<25%、receiverCPU<30%。

## 运行工具与缺口

- `python scripts/verify.py`：Rust本地静态/单元门与独立参考，不是T0/T1。
- `rust/tests/dll_stream_smoke.py --performance`：显式motion/Loopback的本机模块cadence/延迟诊断；CPU门与物理/双机条件须另外报告，不因这个脚本PASS就标矩阵全通过。
- 旧 `ctrl/vc-ctrl.py`、Python agent 与p6专项已从工作树移除。Rust REST v2没有旧 `/exec`、`/files`、`/pcap/start`、`/receiver/report`，主机也没有REST。**Rust双机T1–T5等价自动编排/完整取证工具未实现**；补齐前不声称可一键重现旧矩阵。
- PyroWave专项经DLL smoke显式选codec；压缩hash/decoded/SHM/DShow分开核对，不套Raw像素相等门。画质、长稳、双机另验收。

## 失败与证据

1. 丢帧先核对两端MTU/协商速率和包率、sent/recv；只读诊断，不擅自改EEE/ITR/Jumbo等物理参数。
2. CRC错区分传输损坏；CRC对但整帧hash错查生成/重组。发送少与接收丢分别记录。
3. 双端身份/版本、配置与实际backend须一致；参数不符直接失败，不静默修正。
4. hashlog按frame_id/hash全量对账，不跨机减QPC。发端相邻帧gap>1s标记D7 transient，保留原证据。
5. 双机结束立刻保全双端完整hashlog、stats与运行参数；Rust没有旧分页下载接口，必须提前安排独立完整导出路径。

## 历史实测限定（不改阈值）

- 单包提交曾约200fps，sendqueue batch23/69后达到240fps；旧约14000pps不是悄悄重定T2基线的理由。
- MTU1500物理卡实测曾69fps/约28kpps、慢而不丢；表中约101kpps丢帧反例需要足够快的发端，不能写成任意环境必丢。
- 广播曾长时间挂起，历史建议单播；未直连复测前不将猜测当结论。
- DDA/WGC只发新画面；静止不补假帧。D12允许下游重复最后好帧，不以此伪造motion推进。
- 历史表格针对640² Raw；可变尺寸/有损组合独立验收，不继承旧PASS。
