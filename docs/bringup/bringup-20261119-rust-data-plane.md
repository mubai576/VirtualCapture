# Rust Raw Npcap 互操作诊断（2026-11-19）

## 范围

在独立 `vc-stream` 工具中验证 Rust 对 Windows Npcap 的 Raw NV12 发送、接收与可选 SHM v3 发布，并和现有 C++ sender/receiver 双向交叉对账。未修改 `proto/vc_wire.h` 或 C++/Python 数据面实现；未接入 `vc-core`、Rust host/capture 控制器、产品启停生命周期、绿色包/OTA；未接触部署、PC2、物理网卡或 DShow 注册。

`vc-stream` 是诊断工具，不代表 Rust 产品数据面已切换。工作区绿色包构建器只收集 `vc-host`、`vc-capture`、`vc-updater`、`vc-transport` 等明确产品入口，不包含 `vc-stream`；本次 Release exe 只留在被忽略的 `rust/target/release/`。

## 诊断实现

- `rust/crates/vc-stream` 新增独立 Windows CLI：`send` 以整数 `motion` pattern 生成并发送 Raw NV12；`receive` 用 Npcap `pcap_next_ex` 收帧、交 `vc-transport::Reassembler` 重组、计算逐帧 FNV，并可选发布私有 SHM v3。
- Npcap `wpcap.dll` 从 Windows System32/Npcap 安全搜索路径显式加载；接口列表按 Npcap SDK `pcap_if` 的 C 布局解释，并要求 adapter 选择唯一匹配。接收开 promiscuous 模式，配置 128 MiB capture buffer 与 `pcap_setmintocopy(0)`；发送 batch=1 走 `pcap_sendpacket`，batch>1 走 sendqueue。送队列短发送直接失败，不重试或静默降级。
- CLI 拒绝非冻结 EtherType `0x88B5`。尺寸/fps/MTU 用冻结 `vc-transport` 几何与分片校验；发送按目标帧起点间隔限速，可选 `--pacing-us` 是包间间隔。
- `--shm <name>` 使用独立、Windows 条件编译的 SHM v3 writer；映射名冲突硬失败，不接管已有 writer。此处验证布局与发布协议，不等于 vcam/DShow 的生命周期接线。
- 非 Windows 构建可运行 CLI help，但 live Npcap I/O 明确退出码 2 并报平台不支持。

## 复现

要求 Windows、已安装 Npcap、MSVC/CMake Release 原生 sender/receiver、Rust MSVC toolchain。命令从仓库根目录运行：

```powershell
cmake --build build --config Release
cargo build --manifest-path rust/Cargo.toml -p vc-stream --release --locked
python -I rust/tests/raw_stream_smoke.py --self-check
python -I rust/tests/raw_stream_smoke.py
```

冒烟 harness 只选精确 `\\Device\\NPF_Loopback`，默认 batch=1；若显式传 `--batch`，原值直接传递，不做 fallback。每次运行在唯一的 gitignored `run/rust-raw-stream-smoke-<id>/` 保存两个阶段全部 sender/receiver stats、stdout/stderr、完整 hashlogs、私有 SHM 校验结果和 `report.json`。禁用 SHM 可显式传 `--no-shm`。

## 结果

最终 Release 二进制运行 `run/rust-raw-stream-smoke-b51323d48c`，Npcap 1.88 / libpcap 1.10.6，adapter `\\Device\\NPF_Loopback`；两阶段配置相同：640×640 NV12、60 fps、120 帧、MTU 9000、payload 8958 B、69 包/帧、batch 1、`motion`。

| 阶段 | 发送端 | 接收端 | 逐帧结果 |
|---|---|---|---|
| 1 | C++ `vc-sender` | C++ `vc-receiver` + Rust `vc-stream receive` | 三份 120 帧 hashlog 全量一致 |
| 2 | Rust `vc-stream send` | C++ `vc-receiver` + Rust `vc-stream receive` | 三份 120 帧 hashlog 全量一致 |

总计六条 stream 的 frame id 均为 `0..119` 且无重复；所有 FNV-1a64 与独立 `ctrl/pattern_ref.py::gen_native` `motion` 参考 120/120 一致。两个阶段的 C++/Rust receiver 均 `complete=120`、`dropped=0`、`packets_lost=0`、`crc_errors=0`、`hash_mismatch=0`；Rust `gaps=0`、`hash_rows=120`、`shm_published=120`。sender 各报告 120 帧 / 8280 包 / 0 send errors；六个进程退出码均为 0。

两个 Rust SHM v3 mapping 均由 harness 在 writer 存活时打开并持有；校验 12 槽、614400 B frame、640×640@60、`latest_seq=120`、`latest_frame_id=119`，读取最新完整帧并以 FNV 与对应 Rust receiver hashlog 对账，`hash_fid` 标记有效。两轮最新帧 hash 均为 `619a549cf792079d`，`slot.hash` 与实际 payload 重算一致，读取前后 seqlock 未变化。这只验证本探针 writer 与内存布局，不声称 DShow/虚拟卡消费端已接入。

### 240 fps 同机性能探针

性能 harness 的复现命令：

```powershell
python -I rust/tests/raw_stream_perf.py --self-check
python -I rust/tests/raw_stream_perf.py --batch 1
```

Release `vc-stream` 双进程在同一 Npcap Loopback (`\\Device\\NPF_Loopback`) 运行 Raw NV12、640×640@240、MTU 9000、`motion`、batch 1、SHM v3，共 2400 帧（约 10 秒）。最终有效运行 `run/rust-raw-perf-0205373e29/` 使用同机 QPC，将发送端每帧 `send_qpc_ns`（pattern 已生成、发送准备完成）与接收端 `qpc_ns`（末片校验并完成 payload 重组后）配对；不涉及跨机时钟。结果：发送帧起点平均间隔 4.166667 ms（steady cadence 239.999996 fps），接收完成 cadence 240.001583 fps；2400 帧发送/接收 hash 全匹配，零丢包、CRC 错、重复、gap 或 hash mismatch，SHM 发布 2400 帧。准备完成到重组完成延迟 p50 0.4373 ms、p99 4.3892 ms、最大 4.4946 ms，2400 帧均低于 10 ms。该次 requested cadence/latency 指标通过。

本次并非 T0 全项通过：sender CPU 37.51%（T0 要求 <25%，失败），receiver CPU 23.95%（要求 <30%，通过），所以 `t0_resource_gates.all_pass=false`。另一次显式 batch 69 Loopback 对照 `run/rust-raw-perf-aa1dbd4544/` 以 `pcap_sendqueue_transmit short send: 0/618402 bytes for 69 packets; no batch fallback` 失败，不计作性能样本，也未降级重试。完整 hashlogs、stats 与 JSON 报告保留在各 gitignored `run/rust-raw-perf-<id>/` 目录。

原 C++ 总验证 `python scripts/verify.py`（含 CMake Release 构建、双自检、pattern 参考、pytest）通过；Rust workspace fmt、全量 tests 与 `clippy --all-targets -- -D warnings` 通过；Python smoke/performance harness 自检通过。完整易失性功能互操作证据保留在 gitignored `run/rust-raw-stream-smoke-b51323d48c/`。

## 验收边界 / 后续

这些结果证明当前 Windows/Npcap Loopback 上冻结 Raw 帧格式的双向功能互操作、私有 SHM v3 探针发布，以及上述特定配置下的本机 240 fps/延迟目标。Loopback readiness 报告的 datalink 为 0；不能外推为物理 NIC 或双机链路条件。性能数据使用整数 `motion`，adapter、batch 与尺寸/fps 均如上注明。

未验收物理 NIC、双机 T1/T2、batch 69 性能、线速吞吐、额外约 10 ms 延迟、长稳、DDA/WGC 真实采集、DShow/vcam 接线或编码画质。T0 因 sender CPU 超限仍未通过；QPC 不跨机比较。稳定产品仍使用既有 C++ Npcap/SHM/DShow 路径；任何产品控制器集成、绿色包/OTA 纳入或部署切换都要另行明确授权，并走对应生命周期、物理网卡与项目验收门禁。
