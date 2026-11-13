# 2026-11-13 PyroWave 开发链路本机验证（D24）

本记录只覆盖主机（PC1）本机功能与短时性能观察；**没有**双机、PC2 部署、长稳、画质或实时吞吐验收结论。
Raw 的格式/判据未变，本记录的任何数字都不能用于 Raw 门限或替代既有 PASS。

## 环境与代码

- 主机 PC1（192.168.2.100），Windows 11，MSVC 19.44，RTX 4070（Vulkan 1.3），未安装 Vulkan SDK（只用缓存头文件）。
- 基线提交 `3620030`（D23 移除 H.264/H.265）之上、尚未提交的 PyroWave 工作区改动；
  依赖固定为 PyroWave `a32b05e`、Granite `5309042`、volk `47cddf7`、Vulkan-Headers `6802bb4`
  （摘要见 `docs/runbooks/pyrowave.md`）。
- 构建：`cmake --build build --config Release` 走 `cmake/Pyrowave.cmake` 的 ExternalProject，
  产物 `libpyrowave-shared-1.dll` 自动复制到 exe 目录；`python scripts/verify.py` ALL PASS
  （255 passed / 4 skipped；含 sender/receiver selftest、pattern_ref、pytest）。
- 载荷：单个 PyroWave packet（每帧 1 个）套 48B VCAU，`codec=4`，再由原 L2 分片传输；28B 包头未改。

## 自检

```text
build/vc-sender.exe --selftest        → PASS（含 PYROWAVE 段）
build/vc-receiver.exe --pyrowave-selftest
PYROWAVE: PASS 640x640 bytes=399952 nonzero=203872
build/vc-sender.exe --capabilities    → codecs 含 pyrowave，pyrowave_revision=1
```

## 编码器探针（`build/_probe/pwbench`，产品构建之外，仅本机）

640×640 motion，CPU NV12 上传编码 + CPU YUV420P 解码，240 帧；packetize 按 8958B 边界模拟 L2 分片，
**实际产品路径按整帧 1 packet 输出**，因此 packetize 数字只作参考。

| 每帧预算 | 实际字节/帧 | 240fps 折算 | encode avg/p99 ms | packetize avg ms | decode avg/p99 ms | luma PSNR 平均 |
|---|---|---|---|---|---|---|
| 400000 | 182709 | 350.8 Mbps | 0.148 / 0.323 | 0.180 | 0.257 / 0.502 | 73.77 dB |
| 120000 | 119951 | 230.3 Mbps | 0.146 / 0.267 | 0.162 | 0.250 / 0.448 | 48.57 dB |
| 60000 | 59942 | 115.1 Mbps | 0.146 / 0.221 | 0.166 | 0.242 / 0.468 | 33.18 dB |

探针输出的 `psnr_y_min` 字段实际打印的是最大值（代码标签错误），上表只用平均值。
这些是同机、单进程、空载数字，不能推出双机链路上限或画质验收；PSNR 也不代表主观画质/色彩。

## 本机 Loopback 端到端（640×640，pattern=motion，batch=1，Loopback 网卡）

发送端 `--codec pyrowave --codec-backend pyrowave:vulkan --codec-hardware 0 --gop 1 --max-bitrate 200000000`，
接收端同 codec/后端，`--decode-in-flight 2`，不注册虚拟卡。

- **120 fps，120 帧**：sender 120/120、2483 包；receiver complete=120、compressed_complete=120、
  packets_lost=0、transport_errors=0、decode_errors=0、decode_resets=0、decode_queue_drops=0。
  压缩字节 166888～195240（平均 181338），sender 平均 153.9 Mbps；每帧预算 208333（200 Mbps÷8÷120）。
  transport 日志与 sender 的压缩 hash 120/120 一致；解码 hash 120/120 与压缩 hash 不同（有损，符合预期），
  frame_id 连续无缺。sender encode_wait avg 0.454 / p99 0.674 ms，capture_to_send avg 1.21 / p99 2.38 ms；
  receiver decode_pipeline avg 0.792 / p99 1.025 ms（首包至解码完成，同机段）。
- **30 fps，60 帧**：60/60 解码，1268 包，所有错误/丢弃计数为 0，平均 184656 B/帧。
- **240 fps，120 帧（同一台机器同时跑两端）**：sender 实际 188.4 fps、120 帧全部发出；
  receiver compressed_complete=120 但 complete 仅 6，`decode_queue_drops=114`、`decode_resets=6`。
  这不是编解码器缺陷证据：同机 sender 以 HIGH_PRIORITY_CLASS/MMCSS 抢 CPU，receiver 解码工作线程被饿死，
  队列按设计丢弃并重置；120 fps / 30 fps 同条件零丢弃。按既有规则不放宽队列/在途上限，也不把 240 当作通过。

证据为本机 `run/pwtx*.tx.json(l)`、`run/pwrx*.rx.json(l)`、`run/pwrx*.rx.jsonl.transport.jsonl`；
运行时数据不入 git，需要时按 AGENTS.md 归档到 `run/archive.d/`。

## 专项脚本（`scripts/p6_codec_verify.py` 扩展后）

同一脚本新增 `--codec pyrowave --encoder/--decoder pyrowave:vulkan`，输出目录
`run/p6-codec-transport-<时间>-pyrowave-<id>`；除压缩 hash/解码/DShow 外，还核对
`encoder_configuration` 全部字段、每帧字节不超过预算、发送间隔与一秒载荷窗口。

- 640×640、120 fps、120 帧、`--max-bitrate 200`：PASS；120/120 帧与压缩 hash 全部一致，
  DShow 采样 680 次、其中不同解码 hash 120 个；decode_resets/discarded=0；
  `decode_pipeline_ms` avg 0.85 / p99 1.62 / max 2.78 ms；实测发送 119.99 fps、解码 120.23 fps。
- `--loss`（帧20的第0片丢弃，60 fps、60 帧）：PASS；transport 59 帧、解码 59/59，
  帧21～59 全部恢复，decode_resets=1、decode_discarded=0、queue_drops=0。

调试中发现并修复一个真实缺陷：最初每次流断档都重建整个 `VCDecoder`（含 Vulkan 设备），
设备创建时间超过 60fps 下的几个帧周期，接收队列反复溢出→再重置，出现 29/59 解码、9 次重置、
30 次队列丢弃的死循环。现改为断档只重建 PyroWave 解码器对象（保留设备），解码失败才整体重建；
修复后上述 loss 用例通过，MJPEG 的同类 loss 用例（416×416、120fps、60 帧）仍 PASS。

`scripts/p6_product_verify.py` 扩为 `--codec raw|mjpeg|pyrowave` 的产品编排分支
（隔离 ROOT/随机 HTTP 端口/私有 SHM，注册为替身，真实原生子进程与 DShow）：

- PyroWave 418×282、30fps、fill：PASS；发送 117、接收/解码 117 全对账，DShow 采样 92；
  会话 descriptor 含 `pyrowave_revision:1`，`encoder_configuration.max_bytes_per_frame=176814`
  （受原始 NV12 帧字节限制）。
- 同脚本 `--codec raw`：PASS（121/121、DShow 91）。原 `mjpeg_options` 重构后 raw 被误判为不支持，
  本次一并修复；这只是诊断脚本缺陷，不涉及产品 Raw 链路。


## 未验证 / 已知限制

- 只有阶段一 CPU 上传/回读路径；无 D3D11/Vulkan 外部内存互操作、GPU 缩放或零拷贝。
- 未在 PC2 部署或双机运行；Vulkan 1.3 设备与驱动是两端部署前提，未核实 PC2 的 GPU/驱动。
- 画质（PSNR/色彩/主观）、长稳、240fps 双机吞吐、与 vcam/DShow 注册链路均未验收。
- `scripts/p6_codec_verify.py` 与 `p6_product_verify.py` 均已支持 PyroWave；仍只是本机隔离证据。
