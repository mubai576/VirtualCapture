# L2 wire 契约

真相源为冻结的 `proto/vc_wire.h`，仅 owner 可批准变更。Rust 实现在
`rust/crates/vc-transport/src/`；独立向量检查 `rust/tests/transport_vectors.py` 不依赖旧产品或 C++ 编译器。

## 包头（小端、pack(1)，28B）

| 字段 | 字节 | 含义 |
|---|---|---|
| magic | 4 | `0x56434150` |
| frame_id | 4 | 帧号，关联整帧分片 |
| seq / total | 2+2 | 帧内序号 / 总片数，一包只属于一帧 |
| qpc_ns | 8 | 发送端 QPC，只能用于同机分析 |
| payload_crc | 4 | 本包载荷 CRC32 |
| payload_len | 2 | 本包有效载荷长 |
| flags | 1 | bit0 按原始帧号 0 设置；不新增确认包位语义 |
| pattern | 1 | counter/checker/rings/noise/motion，桌面源为 255 |

EtherType `0x88B5`。像素宽高、fps、MTU、变换、编码与随机本地管理单播源 MAC 在控制面精确确认，不能挤入包头保留位。源 MAC 仅会话隔离，不是认证，也不改系统网卡 MAC。

## 分片与预算

`B=width×height×3/2`，偶数 NV12 几何；`P=MTU−42`，`N=ceil(B/P)`，N≤65535。
Raw 非尾片恰为 P 字节，尾片为余数；严格校验尺寸、序号、片数、长度、CRC、pattern 与覆盖完整性。

| 格式 | MTU | payload | 包/帧 | 240fps 包/秒 |
|---|---|---|---|---|
| 640×640 NV12，614400B | 9000 | 8958 | 69 | 16560 |
| 同上 | 1500 | 1458 | 422 | 101280 |

像素载荷约 1.18 Gbps，不含 L2 全部开销；2.5G 网线仅序列化该帧约 2ms。MTU1500 增加包率，但“必丢”不是普遍实测结论：历史物理卡 sender 仅 69fps/约28kpps，表现为慢而不丢。T3 的原预期与阈值保留在 test-matrix，不据此改门限。

新产品 MTU 默认只读双端 `NlMtu`，取有效最小值，不加14、不修改驱动 Jumbo；详细迁移与上下限见 rust-control。带宽预算不是实际吞吐保证。

## 提交与重组

- 产品 Raw 一整帧一个 sendqueue；编码按实际 `48B VCAU+bitstream` 长度一整 AU 一次提交，帧边界 flush。
- Npcap 返回字节数必须等于 queue.len；零/短发送立即失败，不降 batch、不降 fps 或改编码。
- 仅显式 Npcap Loopback 诊断固定 batch1、逐包提交；不能作为物理卡/双机性能证据。
- 丢片不发布部分帧；重复冲突、CRC/长度错误拒绝；已完成/过期帧不因迟到片再次发布。有界重组，禁止无界 FIFO 堆积。
- 发送上限按同机时钟执行，不追赶突发。DDA/WGC 只发新画面，不以旧帧补足 fps；下游 D12 可重复最后好帧。

Raw 整帧 FNV-1a64 初值 `14695981039346656037`、prime `1099511628211`，小写16位十六进制；对账只比 frame_id/hash，不跨机相减 QPC。有损格式和独立完整性规则见 `encoded-stream.md`。

历史 `--batch 1/3/23/69` 与包间 pacing 实验只代表当时实现；当前普通产品不提供手动 batch，也不因旧报告恢复旧进程。
