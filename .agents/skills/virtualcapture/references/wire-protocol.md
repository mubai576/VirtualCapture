# Wire protocol（L2 数据面）

真相源：`proto/vc_wire.h`，本文件是解读版，改协议先改头文件再同步这里。
校验实现统一在 `common/crc32.h`（包载荷）与 `common/fnv1a.h`（整帧）；发收端共用，算法、向量与包头不变。

## 一帧的账

- 会话帧为 `B=width*height*3/2`（P6-2 控制面协商；默认 `640×640`）。以下为历史默认预算，不是任意尺寸的吞吐保证。
- 默认帧：`640×640 NV12 = 614400 B`（`width*height*3/2`），`@240Hz → 147.5 MB/s ≈ 1.18 Gbps`。
- 包：MTU 9000，单包有效载荷 `P=mtu-42=8958 B`（以太头14 + VC头28另计）→ **~69 包/帧，16.5k pps**。
- 反例：MTU 1500 → 422 包/帧 → **~101k pps**，Windows 用户态 Npcap 必丢（故 Jumbo 强制）。

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

## P6-2 会话隔离与重组

包头完全不改：宽高/fps/MTU/raw-NV12/transform/随机源MAC在控制面显式确认。
每帧 `total=ceil(B/P)`；非尾包payload_len=P，尾包为剩余字节，receiver严格校验尺寸/seq/total/长度/pattern/CRC和完整覆盖。
源MAC只用于会话隔离（非认证），不改系统网卡MAC；收端在解析VC头前过滤，迟到帧不能重新发布。
`qpc_ns`保留同机含义；sender hashlog另有send_qpc_ns，用于验证发送开始间隔≥ceil(1e9/fps)，无追赶突发。
DDA只发新桌面图像，静态/鼠标更新不重放。旧低层默认640仍可诊断，新产品会话须双方stream_revision=2。

## P6-3 编码载荷

28B包头、EtherType、pattern/flags语义完全不变。显式编码会话在普通载荷内发送48B VCAU封装+MJPEG完整帧，按实际长度分片。Raw仍按固定会话NV12字节数严格检查，绝不尝试猜测或自动切换编码。封装/上限/整体hash/EOS/逐帧恢复完整契约见 `encoded-stream.md`。

## 发送 pacing（T4 定默认）

69 个 9K 包背靠背是 ~620 KB 微突发，直连网卡 Buffer 未必全吃。
对照 `pacing_us = 0 / 50 / 200` 包间隔，看 `dropped` vs CPU 的 trade-off 再定默认。

## Batch submit (T-10, docs/history/plan-history.md S23)
- sender --batch N: N=1 per-packet sendpacket; N>1 accumulates N packets in a sendqueue, one transmit per N, remainder flushed at frame end. N=69 means one full frame per transmit.
- Dual-host result (2400 motion frames, mtu9000, 0.3.7): batch1 ~205fps, 3 -> 230fps, 23/69 -> 240fps target cadence. Gains converge after 23 (~1/3 frame).
- Npcap pcap_sendqueue_transmit returns positive on success (not 0): send_errors must test <0 (fixed 2026-10-21 after a !=0 false-positive run).
- P6-3 encoded sender additionally requires the returned byte count to equal sendqueue.len; zero/short sends fail explicitly, without a batch fallback. The historical Raw path is unchanged. Installed Loopback batch69 returns a short send; local encoded verification therefore uses explicitly requested batch1, not a production NIC conclusion.
- Batching changes submission only; wire format untouched (proto/ frozen).
