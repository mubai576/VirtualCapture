# 自动化测试方案（MVP：只验推流，不做虚拟卡/独占全屏）

> 范围裁剪（2026-10-21确认）：独占全屏延后（MVP只要求无边框/窗口DDA可用）；
> 网线9K全链测试列为TODO；PC2虚拟采集卡不做；自用（Npcap个人免费，无分发/签名负担）。
> 目标：无人值守验证 `640x640@240 NV12 614400B/帧 → 2.5G L2直连 → PC2重组` 是否跑满、无损传输、可量化性能。

## 1. 为什么必须双网分离

数据面（2.5G直连，自定义EtherType如0x88B5，不配IP）**不能跑TCP控制流量**。
控制面必须走带外：利用现有 `192.168.2.x` 局域网 / WiFi / 第二网口。

```
            数据面（无IP, MTU9000, EtherType 0x88B5）
   PC1 ————————————————————————————— PC2
    |                                       |
    | 控制面（TCP/HTTP, 192.168.2.x）        |
    └——————→ vc-ctrl（跑在PC1或笔记本）←————┘
```

要求：两台PC各留一个能进 `192.168.2.x` 的网口给控制面，数据网卡不配IP只设9000。
> SUPERSEDED-NOTE 2026-10-22: dual-NIC rationale above is void (shared-NIC proven, PLAN S11). Control plane rides the same NICs (PC1 .100 / PC2 .59). Test matrix and thresholds unaffected.

## 2. 四件套架构

| 组件 | 语言 | 职责 | 运行位置 |
|---|---|---|---|
| `vc-sender` | C++ | 发包：`pattern \| dda \| file` 三种源 → GPU（如有）→ Npcap L2裸发 | PC1 |
| `vc-receiver` | C++ | 收包：Npcap抓包 → 按frame重组 → 统计/哈希校验（MVP不落虚拟卡） | PC2 |
| `vc-agent` | Python（Flask/FastAPI单文件） | 受控端：更新、启停、查状态 | PC1+PC2各一个，开机自启 |
| `vc-ctrl` | Python CLI+pytest | 编排：推版本→起收→起发→采数→判分→出报告 | 控制机 |

## 3. Agent控制API（两端统一，先定死，开发不返工）

```
GET  /health                    → {ok, version, role}
GET  /version                   → {version, git_sha}
POST /update                    → 上传zip（二进制+version.json），只 stage（校验+prev 快照，不重启），返回 staged version；POST /activate {version} 或 {rollback:true} → 原地 execv 接管，轮询 /version 确认
POST /sender/start   {mode:"pattern|dda|file", fps:240, frames:0(无限), ethertype:"0x88B5", mtu:9000, pacing_us:0, pattern:"counter"}
POST /sender/stop
GET  /sender/stats              → {sent_frames, sent_packets, pps, bps, gpu_ms_avg, send_errors}
POST /receiver/start {ethertype:"0x88B5", mtu:9000, expect_frames:0, timeout_s:30, verify_hash:true}
POST /receiver/stop
GET  /receiver/stats            → {complete, dropped, packets_recv, packets_lost, pps, bps,
                                   gaps:[缺的frame_id], hash_mismatch, inter_arrival_avg_ms/p99_ms}
GET  /receiver/report           → 本轮CSV/JSONL下载地址
POST /pcap/start {seconds}      → 应急抓包（只在失败时抓前N秒，避免9K大包撑爆盘）
```

所有 `start` 必须幂等（重复调用先停旧会话），所有 `stats` 必须带 `session_id`，ctrl用session对齐双端数据。

## 4. 不依赖游戏的验证核心：高复杂度确定性测试源套件

先说透：裸NV12传输带宽恒为`147.5MB/s`（内容无关），静态画面测的是“假性能”——
DDA会用脏矩形跳过静态区、GPU帧缓冲压缩对纯色极友好、错误在低熵画面里还容易被掩盖。
所以复杂度源的真正作用：①逼出全帧脏矩形（每帧必须真干614KB的活）；②高频纹理专杀NV12色度错位/矩阵错
（纯色下矩阵写错都看不出来）；③最大熵噪声专杀链路里任何隐藏压缩/去重/缓存作弊。

五档（`sender/pattern.h`，纯整数、三端bit-exact，`ctrl/pattern_ref.py`为Python镜像）：

| pattern | 内容 | 杀什么 | 备注 |
|---|---|---|---|
| `counter` | 缓变梯度 | 烟雾测试 | 最弱，只证链路通 |
| `checker` | 8x8棋盘逐帧滚1px | NV12 4:2:0错位 | 黑白交界把色度bug放大到肉眼可见 |
| `rings` | 同心环锯齿+滚动 | 下采样混叠/矩阵 | 整数近似zoneplate，不用sin（HLSL各卡sin有差） |
| `noise` | 全帧splitmix64噪声 | 隐藏压缩/作弊 | 熵最大，不可压缩，帧间平均绝对差~85（counter恒为1） |
| `motion` | **默认压力源**：滚背景+4弹跳块+中央256噪声窗 | 综合：脏矩形+运动+熵 | 最接近游戏负载，T1/T2默认用它 |

校验方式（内容无关，DDA/file同样适用）：sender记每帧FNV1a64到JSONL，receiver记收到帧哈希，
ctrl对账——不需要传参考帧；pattern确定性（seed=frame_id）只用于丢包重放debug。
receiver在线用4K点抽查（`verify_samples`），全量hash对账离线做，240Hz下CPU吃得消。

另两种源：`file`（录一段真2K游戏NV12到内存盘回放，最高真实度，用于放行前终检）、
`dda`（真实桌面，只看gpu_ms/pps/CPU摸底，不判hash）。

## 5. MVP测试矩阵（ctrl一键跑）

```
T0 loopback   ：同机 sender→receiver（内存/Npcap本机），pattern=motion 240Hz×10s，hash必须0错。无PC2也能开发。
T1 linked     ：双机 240Hz×60s，MTU9000，五档pattern各跑一轮（counter只冒烟，motion/noise/checker/rings全判），
                判 complete==sent, dropped==0, hash_mismatch==0。默认放行判据以motion为准。
T2 soak       ：双机 pattern 240Hz×10min，判 pps≈16560±2%, bps≈1.18Gbps, inter_arrival p99<8.3ms(2帧)。
T3 mtu1500反例：MTU1500×10s，预期 pps≈101k且丢帧>0（证明文档反例，跑一次归档即可）。
T4 pacing对照 ：pacing 0us vs 50us vs 200us burst间隔，看dropped/CPU trade-off，选默认。
T5 dda摸底   ：无边框2K桌面裁缩640，240Hz×30s，只看 gpu_ms/pps/CPU，不判hash（画面不可预期）。
```

判分阈值（初版，跑3轮后按实测收紧）：
`dropped==0(T1), hash_mismatch==0, inter_arrival_avg 4.166±0.3ms, senderCPU<25%, receiverCPU<30%`。

跨机延迟说明：QPC跨机不可比，MVP不测绝对延迟，只测 `inter-arrival抖动` + 网线序列化理论值2ms。绝对端到端延迟等虚拟卡联调时再用帧内嵌时间戳+双机PTP同步做。

## 6. 跨机对时与日志规范

* 双端日志统一 `session_id + frame_id + qpc_ns + event`，JSONL落盘，ctrl按session拉回合集。
* 控制机与双端用NTP对时（192.168.2.x内已有路由即可，ms级够用）；数据面不跑任何对时协议。
* 失败自动 `pcap/start` 留证 + `report` 打包（stats+日志+版本+网卡MTU/驱动截图文本）。

## 7. 目录结构

> 2026-10-23：原建议树已落地并演进，现行目录以根目录 `AGENTS.md` 的 Repo map 为准（单一真相源，不在此重复）。

## 8. TODO（不进MVP，但先占位）

* [ ] 网线全链9K验证：`scripts/check_mtu.ps1`（双端ping -f -l 8972 + Npcap 9K收发计数）。
* [ ] 独占全屏优化：Present Hook（DXGI Hook / SpecialK思路），MVP后单独立项。
* [ ] PC2虚拟相机：待推流稳定 + 确认推理侧DShow/MF后再做。
* [ ] 10分钟soak基线入库：CPU/pps/抖动三次结果归档为后续回归红线。

## 9. 下一步（最小开工集合）

1. 冻结 `proto/vc_wire.h` 包头（先定frame_hdr，再写收发两端）。
2. 写 `agent/vc-agent.py` stub（health/version/start/stop/stats假数据 tun 通ctrl）。
3. 写 `ctrl` 的T0（单机pattern自环），不需要第二台PC就能开始迭代。
4. 再联调T1双机。
