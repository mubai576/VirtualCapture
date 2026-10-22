# P4-2 端到端延迟测量设计（待 owner 批；本文件只设计，不改代码不动 proto）

## 1. 目标与约束（PLAN P4-2 原文转述）

- 设计同钟测法：PC2 收齐帧后回发 1 个小确认包，PC1 用同一 QPC 算 RTT/2 上界；报 采集→重组 延迟分布，目标 <4.16ms。
- 确认包格式需 owner 批（proto 变更）。冻结约束：包头 28B 尺寸/字段顺序不动（PLAN §1.1，proto/ 归 owner 锁定）；跨机不报 QPC 差值延迟（PLAN §1.5.5）。
- 现状：链路纯单向（PC1 发、PC2 收）；receiver 只收不发；sender 只发不收；qpc_ns 只做同机抖动分析。

## 2. 方案：反向小确认包（wire 平面，不走控制面）

- 格式候选：沿用 28B 头，flags bit1 = CONFIRM（bit0 仍是 key frame，其余保留位不动），frame_id = 被确认帧，seq = total = 0，payload_len = 0（零 payload），payload_crc = 空校验，magic 照旧以便 BPF 按 EtherType 0x88B5 捕获，qpc_ns 填 PC2 重组完成本地时刻（仅 PC2 侧自分析用，绝不跨机相减）。
- 发送点：vc-receiver.exe 重组完一帧且 hash 校验通过后，立即发 1 个 confirm（先只记数，不阻塞接收流水线）。
- 接收点：vc-sender.exe 侧加 pcap 过滤（同 EtherType + flags CONFIRM 位），只收确认包，不干扰发送。
- 度量定义：L(N) = t_confirm_rx(N) − t_last_tx(N)，全部取 PC1 同一 QPC；含义 = PC2 重组尾巴 + 确认包发送 + 反向线传，是单向数据延迟的上界（RTT/2 口径，对称线假设）。
- 采样：每帧 1 个（240/s × 约 60B ≈ 14KB/s，相对 1.47Gbps 可忽略）；confirm frame_id 对不上或超时记 loss，不静默。

- 主指标 R(N)（2026-10-22 owner 定）：PC2 本地重组段耗时 = 重组完成时刻 − 首包到达时刻，全取 PC2 同一 QPC，不跨机比较，直接证明重组延迟本身；L(N) 降为上界交叉验证（只能证明没超标）。R(N) 由 receiver 在本地统计上报，无需确认包参与。

## 3. 对现有链路的影响（实现时必须处理）

- receiver 必须忽略 CONFIRM 包：回环自测里收发同网卡，receiver 会同时抓到确认包；以 flags 位过滤，不计入 complete/dropped/gaps/hash_mismatch，否则 T0/T1 回环门限被污染。
- reconcile：确认包零 payload，不进 hash 对账；confirm 自身 loss 单独计数（confirm_loss），与 stream dropped 分开报。
- 开销：可忽略（见 §2 采样）；发送 pacing 不动；MTU/批量逻辑不动。
- 单播/广播：确认包目的 MAC 回填 PC1 网卡（从收到的 stream 包源 MAC 学，或沿用 --dst-mac 反向；实现细节待定，见 §4 问 6）。

## 4. 门限提案（进 test-matrix 需 owner 批）

- 主门限改为 R(N)（PC2 本地重组段）：报均值/p50/p99/max；候选门：p99 < 4.16ms（= 1 帧预算），confirm_loss = 0。L(N) 只做上界交叉验证。
- 若 R(N) p99 超标：先看分布形状（长尾 vs 整体平移）再定是重组抖动还是线传问题；不直接放宽判据（D1 精神）。

## 5. 待 owner 拍板（6 问）

1. 确认包格式是否按 §2（flags bit1 + 零 payload，28B 不动）？若要动尺寸/顺序，另立 proto 评审。
2. 门限数字是否批：L(N) p99 < 4.16ms、confirm_loss = 0？
3. 回环默认开关：确认包是否默认开启（receiver 必须带过滤），还是只在双机联调 flag 打开？提案默认开 + 单测覆盖过滤。
4. 是否每帧确认？提案是（开销可忽略，分布完整）。
5. qpc_ns 字段是否带 PC2 重组完成时刻（只自分析用）？提案带（零成本，为以后分段归因留余地）。
6. 确认包目的 MAC：从 stream 源 MAC 学，还是配置反向 --dst-mac？提案从源 MAC 学（零配置）。

## 6. 不碰项

- 本文件不改任何代码与 proto；主 agent（18080）与 PC2 现网零接触。
- D1 判据、reconcile 口径、D4 SKIP 语义保持；确认包永不进入 hash 对账。

## 7. owner 批示 2026-10-22

- 确认包格式（flags bit1 + 零 payload，28B 不动）：可行，实现时终审。
- 门限指标改为 R(N) 重组段（PC2 同钟自洽），不再用 RTT/2 上界当门限；上界只做交叉验证。门限数字随实测分布再定。

## 8. owner 批示 2026-10-23（PLAN D8，覆盖 §7 门限口径）

- 范围只管采集与传输，推理不在本项目。
- 门限口径 = PC1 采集完成 → PC2 重组完成，分段同机 QPC：PC1 段（采集完成→末包发出）+ R(N) + 线传段（L(N) 上界）；判全程 p99，候选 <4.16ms，数字随实测分布定。R(N) 单独只作诊断。
