# Wire protocol（L2 数据面）

真相源：`proto/vc_wire.h`，本文件是解读版，改协议先改头文件再同步这里。
校验实现统一在 `common/crc32.h`（包载荷）与 `common/fnv1a.h`（整帧）；发收端共用，算法、向量与包头不变。

## 一帧的账

- 帧：`640×640 NV12 = 614400 B`（`width*height*3/2`），`@240Hz → 147.5 MB/s ≈ 1.18 Gbps`。
- 包：MTU 9000，单包载荷 ≈8980 B（含 28 B 头）→ **~69 包/帧，16.5k pps**。
- 反例：MTU 1500 → ~421 包/帧 → **~101k pps**，Windows 用户态 Npcap 必丢（故 Jumbo 强制）。

## 包头 `vc_pkt_hdr_t`（28 B，pack(1)）

| 字段 | 字节 | 说明 |
|---|---|---|
| `magic` | 4 | `0x56434150` ('VCAP')，抓包一眼识别 |
| `frame_id` | 4 | 单调递增，跨包关联/连续性判定 |
| `seq` / `total` | 2+2 | 本帧内序号/总包数；**单帧不跨帧**（一包只属于一帧） |
| `qpc_ns` | 8 | 发送端 QPC；**仅同机抖动分析，跨机不可比** |
| `payload_crc` | 4 | 本包载荷 CRC32（先分清传输损坏 vs 重组 bug） |
| `payload_len` | 2 | 本包有效载荷长（尾包不足整包） |
| `flags` | 1 | bit0: key frame（pattern 起点） |
| `pattern` | 1 | `vc_pattern_t`（`sender/pattern.h` 五档），原 reserved |

EtherType：`0x88B5`（实验段，无标准冲突，头内另有 magic 双保险）。
网线序列化时间：`614400 B / 2.5 Gbps ≈ 2 ms`，故端到端 <4.16 ms 预算极紧，
接收策略：缺包整帧丢、只留最新 1 帧、FIFO 堆积必须为 0。

## 发送 pacing（T4 定默认）

69 个 9K 包背靠背是 ~620 KB 微突发，直连网卡 Buffer 未必全吃。
对照 `pacing_us = 0 / 50 / 200` 包间隔，看 `dropped` vs CPU 的 trade-off 再定默认。

## Batch submit (T-10, docs/history/plan-history.md S23)
- sender --batch N: N=1 per-packet sendpacket; N>1 accumulates N packets in a sendqueue, one transmit per N, remainder flushed at frame end. N=69 means one full frame per transmit.
- Dual-host result (2400 motion frames, mtu9000, 0.3.7): batch1 ~205fps, 3 -> 230fps, 23/69 -> 240fps target cadence. Gains converge after 23 (~1/3 frame).
- Npcap pcap_sendqueue_transmit returns positive on success (not 0): send_errors must test <0 (fixed 2026-10-21 after a !=0 false-positive run).
- Batching changes submission only; wire format untouched (proto/ frozen).
