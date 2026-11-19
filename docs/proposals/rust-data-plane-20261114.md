# Rust 数据面迁移起点与诊断验证（2026-11-14 / 11-19）

本提案起初记录新增 `rust/crates/vc-transport` 的冻结契约、离线抓包诊断与验证方式；2026-11-19 增补了独立 `vc-stream` Windows Npcap Raw 互操作探针。它不代表产品控制面/运行路径已接入或现用链路替换。新的本机证据与明确范围见[2026-11-19 bring-up](../bringup/bringup-20261119-rust-data-plane.md)。

## 范围

`vc-transport` 是 UI 无关、仅依赖 Rust std 的库，供未来的 Rust sender/receiver 复用：

- 28 B 冻结包头 `vc_pkt_hdr_t` 的逐字段编码/解析（`proto/vc_wire.h`）；
- 包载荷 CRC32（`common/crc32.h`，IEEE 反射多项式，初值/输出异或全 1）；
- 整帧 FNV-1a64（`common/fnv1a.h`）与两段拼接版本；
- Raw NV12 会话几何、`mtu-42` 载荷、每帧包数/尾包长度；
- 分片迭代器与完整以太帧写入；
- 有界乱序重组：会话源 MAC 过滤、尺寸/长度/CRC 校验、重复包/迟到包、
  新帧淘汰旧残帧、最多 8 个未完成帧与总缓冲上限；
- 完成帧整帧哈希、首包→完成与帧间到达统计窗口。

明确不在本块：Npcap FFI、DDA/WGC/GPU 采集、SHM/DShow 发布、pattern 逐像素自检、
发送 pacing/batch/发送队列、控制面配置。这些仍然由现有 C++ 实现负责。

## 与 C++ 的对应

| 契约 | C++ | Rust |
|---|---|---|
| 包头 28 B，小端、pack(1) | `vc_pkt_hdr_t` | `wire::Header::{encode,parse}` |
| EtherType / magic | `VC_ETHERTYPE` / `VC_MAGIC` | `wire::ETHERTYPE` / `wire::MAGIC` |
| `payload = mtu-42` | `payload_max` | `wire::payload_max_for_mtu` |
| `total=ceil(B/P)`，尾包剩余 | sender/receiver | `wire::packet_count` / `payload_len_at` |
| 每包 CRC 只覆盖 `payload_len` | `vc_crc32` | `checksum::crc32` |
| 整帧哈希小写 16hex | `vc_fnv1a64_hex` | `checksum::fnv1a64_hex` |
| 源 MAC 会话过滤（非认证） | receiver 在解析 VC 头前比较 | `Reassembler` 配置 `source_mac` |
| 缺包整帧丢、只留最新 | `Reassembler` 8 槽 | `Reassembler` 同规则 + 显式字节上限 |
| `packets_recv/bytes_recv/packets_lost/dropped/complete/gaps` | receiver stats | `reassembly::Stats` |

### 有意的差异

1. **帧号回绕**：Rust 使用 RFC 1982 序号算术（`is_after`），跨 `2^32` 回绕仍把新帧视为新；
   C++ 用数值比较。非回绕场景两者完全一致；正常会话远达不到回绕，但 Rust 行为有测试定义。
2. **容量**：C++ 固定 8 个缓冲槽；Rust 除 `max_inflight_frames` 外还强制
   `max_buffered_bytes`（默认 64 MiB 与至少一帧）。这不是验收指标，只是防无界内存。
3. **额外诊断计数**：`duplicate`、`crc_errors`、`ignored_other_source`、`late`。
   `packets_lost` 等原有口径不变；`crc_errors` 同时计入 `packets_lost`。
4. **完成帧所有权**：完成帧把缓冲区移交调用方，不实现 SHM 原地发布；
   未来 receiver 可选择复制进 SHM 槽或直接接管。
5. **pattern 校验**：不复制 `sender/pattern.h`。未来 Rust receiver 可另行实现并复用
   `CompletedFrame::hash` 做 hashlog 对账。

## 验证

```powershell
python -I rust/tests/transport_vectors.py          # C++ 向量 + Rust 测试
python -I rust/tests/transport_vectors.py --write  # 重新生成 fixture（协议/算法变更后）
```

脚本做两件事：

1. 用 MSVC（`vswhere` 定位 BuildTools）编译 `build/vc-transport-vectors/gen.cpp`。
   生成器直接包含真实的 `proto/vc_wire.h`、`common/crc32.h`、`common/fnv1a.h`，
   输出 CRC/FNV 向量、28 B 包头字节、640×640 几何（MTU 9000/1500）、
   以及一帧 64×64 Raw 的真实分片（乱序 + 坏 CRC 副本 + 重复包），
   写入/比对 `rust/crates/vc-transport/tests/vectors.txt`。
2. 把 crate 复制到 `build/vc-transport-check/` 并临时加 `[workspace]`，
   运行 `cargo test`，不修改根 `rust/Cargo.toml`/`Cargo.lock`。

结果（2026-11-14）：

- `cargo test`：27 个库单元测试（含 pcap 四种字节序/精度、pcapng 拒绝、截断/超限）
  + 4 个 bin 参数测试 + 6 个契约测试全部通过；
- `cargo clippy --all-targets -D warnings`：通过；`cargo fmt --check` 通过；
- C++ fixture 与 Rust 输出逐字节一致；重复生成 fixture 不再变化；
- pcap CLI 往返：真实 `vc-transport replay` 从 C++ 黄金包 pcap 得到
  `complete=1`、`crc_errors=1`、`duplicate=1`、帧哈希 `0edf612810c4d174`；
- 测试覆盖：正常/乱序/缺片/坏 CRC/重复/错误 total/错误长度/错误 magic/
  捕获截断/源 MAC 不符/迟到包/旧残帧淘汰/容量上限/帧号回绕。

这些都是本机功能与契约证据，不是吞吐、延迟、双机或长稳结论。

## pcap 诊断 bin

crate 自带独立 bin `vc-transport`（`src/bin/vc-transport.rs`，零第三方依赖），用
`src/pcap.rs` 离线读取抓包。它不打开网卡、不依赖 Npcap、不接入 capture CLI，也不参与部署：

```powershell
vc-transport inspect run/capture.pcap [--json] [--max-packets N]
vc-transport replay  run/capture.pcap --width 640 --height 640 --mtu 9000 `
  [--source-mac aa:bb:cc:dd:ee:ff] [--framewise] [--hash-out FILE] [--json]
```

- `inspect`：格式、包数/字节数、捕获长度、时间戳、EtherType 分类、源 MAC、
  VC 头 magic/`total` 概况，不要求知道会话尺寸。
- `replay`：按 `--width/--height/--mtu` 建 [`Config`]，逐包调用
  `push_ethernet_at`，输出完成帧、丢包/CRC/重复/迟到、残帧、rseg/帧间隔统计；
  `--framewise` 打印 `frame <id> <hash> <bytes> <rseg_ms>`，JSON 模式为 NDJSON；
  `--hash-out` 流式写 `<frame_id> <fnv1a64-hex>`，便于与 receiver hashlog 对账。
- 支持经典 pcap 的小端/大端 × 微秒/纳秒四种组合，只接受 `LINKTYPE_ETHERNET(1)`；
  pcapng 魔数会明确报错并提示转换方式，不做猜测解析。
- 输入有明确上界：单包捕获长度上限 1 MiB（分配前检查），默认最多读 2000 万包
  （`--max-packets`），记录流式读取不整包载入；半条记录头/载荷、未知魔数、版本/链路类型
  不符都会返回带记录号的错误。退出码 0 成功、2 用法/读取/格式错误（错误写 stderr）。
- 时间差使用抓包时间戳并相对首包归一；跨机仍不可比较。

roundtrip 证据（同一条命令内完成）：

```powershell
python -I rust/tests/transport_vectors.py
# C++ vectors match tests/vectors.txt
# pcap CLI roundtrip ok: frame 77 0edf612810c4d174, complete=1,
#   crc_errors=1, duplicate=1, pcapng_rejected=true
```

脚本把 C++ 生成器输出的 7 个真实以太包写成临时
`build/vc-transport-check/run/transport-golden.pcap`（二进制只在 build/，不入 git），
再运行真实 CLI 的 `inspect`/`replay` 并要求 JSON 统计与 C++ 整帧哈希一致；另写一个
pcapng 文件确认 CLI 拒绝。大端/纳秒路径由 `pcap.rs` 单元测试覆盖，并另做过一次手工 CLI 验证。

## 独立 Raw Npcap 诊断工具（2026-11-19）

新增 `rust/crates/vc-stream`，复用本 crate 的冻结分片/重组逻辑，以独立 Windows bin 实现 Npcap Raw send/receive、帧 hashlog 和可选 SHM v3 发布。它目前不是 `vc-capture`/`vc-core` 的后端；包构建器不收集该 exe，也没有产品生命周期、虚拟卡或 OTA 接线。

- `send` 使用 integer-only pattern（默认 motion）、QPC 帧 pacing、Npcap `pcap_sendpacket` 或 sendqueue；仅固定 EtherType `0x88B5`，短发送失败关闭，不重试/回退。
- `receive` 使用 `pcap_next_ex`、冻结 Reassembler、逐帧 FNV/独立 pattern 检查；可选私有 SHM v3 写入器，不接管已存在的同名映射。
- `rust/tests/raw_stream_smoke.py` 以两个并行 receiver 分别验证 C++ TX 与 Rust TX，逐帧比对每条 hashlog、独立 Python pattern 参考，并在 writer 存活时打开 SHM mapping、核对最新实际 NV12 frame 与哈希。
- Windows/Npcap Release Loopback 证据、运行命令与限制见[bring-up](../bringup/bringup-20261119-rust-data-plane.md)。它是本机功能互操作，不是物理 NIC、吞吐、延迟、双机、DShow 或长稳验收。

## 接入下一步

1. `vc-transport` 已是 workspace crate；保留 C++ 黄金向量、契约测试与离线 pcap replay 门禁。`vc-stream` 是隔离诊断工具，与这些库级验证分开。
2. 当前已验证的 Npcap sender/receiver 与 SHM writer 尚未归入产品 controller；迁移时需先定义并验证产品会话/启动停止、网卡能力与失败清理边界，再明确接入。不得因 standalone 诊断 PASS 自动切换产品路径。
3. DDA/WGC 与 GPU 采集尚未由 Rust 数据面接管。Rust 产品路径接入每一步仍需按项目既有 Raw 判据和双机流程验证；性能只用 motion 并注明 adapter/batch，回环不能作物理容量或延迟证据。
4. SHM 探针的最终内存布局与最新帧已做只读外部核验，但完整 DShow/vcam 消费端、注册/回收、格式生命周期仍未验收。稳定实现保持不动，发布/OTA纳入与部署切换待独立授权。

## 文件

- `rust/crates/vc-transport/src/{lib,wire,checksum,fragment,reassembly,pcap}.rs`
- `rust/crates/vc-transport/src/bin/vc-transport.rs`（独立 inspect/replay 诊断 bin）
- `rust/crates/vc-transport/tests/{contract.rs,vectors.txt}`
- `rust/tests/transport_vectors.py`（C++ 交叉验证、隔离测试、pcap CLI 往返）
