# PyroWave 编码与有损完整性契约

当前产品只允许 Raw 与显式开发后端 `pyrowave:vulkan`。CPU MJPEG 实现/运行库已移除；旧 codec 配置必须明确报 GPU-only 错误，不自动改 Raw。历史有损报告保留为当时版本证据。

## 选择与 GPU 约束

- Raw 默认，不压缩；PyroWave 必须 `codec_experimental=true`，双端 encoder_backend/decoder_backend 精确 `pyrowave:vulkan`，GOP=1。
- PyroWave 是 Vulkan1.3 CDF9/7 计算小波、intra-only，无熵编码和参考帧；`hardware=false` 仅指不是厂商固定功能视频引擎，不是 CPU 编码。
- 使用已选同一个 VkPhysicalDevice 的真实属性确认 integrated/discrete GPU 和 Vulkan≥1.3；CPU、virtual、other、未知属性拒绝，不换设备、不另建探测设备蒙混过关。
- settings 报 `compute=vulkan,gpu_compute=true,vulkan_min_version=1.3,zero_copy=false` 与实际设备 name/type/vendor/device/API/driver/UUID/LUID；无法确认设备不能开始编码。
- 阶段一 `upload_path=cpu_nv12`、`decode_path=cpu_yuv420p`，仍有 CPU 上传/回读和 UV 交错，不是零拷贝。亮度必须真实写入 NV12，不能只更新色度。
- `jpeg_quality` 等旧字段仅供旧配置反序列化及明确拒绝，不是可用 MJPEG 开关。

## 预算与有界执行

每帧 PyroWave bitstream 预算固定为
`min(max_bitrate_bps/8/fps, max_au_bytes−48, width×height×3/2)`；至少1024B。
编码器按此帧预算调整该帧精度，不提供质量旋钮；目标码率字段不驱动编码。
发送端一秒滑窗另外限制完整 AU 字节，单 AU 超预算/内存限制直接失败，不自动降尺寸/fps或换后端。
`max_au_bytes` 默认8MiB、范围1024..16MiB，并受 uint16 分片数限制。

编码在途1帧；接收网络重组最多2AU，等待解码队列最多2AU；`decode_in_flight` 显式1..8，默认2，不为追帧加深。必须输出单个 PyroWave packet，否则拒绝。完整 AU 按实际片数单次 sendqueue 提交，零/短发送失败；仅显式 Loopback 诊断逐包提交。

ready 前以同几何/后端/预算合成 NV12 预热；接收端临时编码生成合法 AU 再真实解码。输出丢弃，不计正式帧号/hashlog/SHM/稳态统计。
创建、预热、编码、解码、重置、销毁各受5秒同步看门狗保护；超时只把会话 OperationState 置为粘性失败，不退出宿主、不强制展开或 join 挂起线程。父层保留 worker/句柄/DLL，cleanup_pending 锁配置/OTA/虚拟卡，安全停止未确认不得卸载。

## 48B VCAU（载荷内，不改28B包头）

| 偏移 | 字节 | 小端内容 |
|---|---|---|
| 0 | 4 | magic `0x55414356` |
| 4/5/6/7 | 各1 | revision=1 / codec=4(PyroWave) / kind=1独立帧或2EOS / header_bytes=48 |
| 8/12/16 | 各4 | width/height/fps，精确匹配会话 |
| 20/24 | 各4 | 编码字节数/frame_id，与外层一致 |
| 28 | 4 | 保留0 |
| 32 | 8 | 编码 bitstream 的 FNV-1a64，不含封装 |
| 40 | 8 | 保留0 |

整个封装+bitstream 按 MTU−42 分片；非EOS必须完整非空的单packet（至少8B），EOS kind2 无数据、用下一帧号。
历史 codec ID 1=H264、2=HEVC、3=MJPEG 不重编号或复用；底层历史解析常量不代表产品支持，产品验证拒绝这些编码。Raw 不猜封装、不自动识别编码。

重组严格检查 seq/total/CRC/长度及同帧元数据；相同重复可接受，冲突重复丢帧，迟到片不能重发完成帧。完整后再校验 VCAU/整体 hash。源 MAC 仅会话隔离，不是认证。

## 解码、恢复与 SHM

断档、无效 AU 或队列溢出触发重置；intra-only 下一个完整帧即可恢复，无 GOP/IDR 等待。断档只重建 decoder、保留 Vulkan 设备，避免设备冷启动造成反复溢出；真正后端失败明确记录。禁止静默切 codec/设备。
只有完整 NV12 输出才发布 SHM v3；失败 abandon 未发布槽。D12 无好帧不产样本，停流后只可重复最后好帧；不能以重复冒充新解码。

## 哈希与时间

- sender：frame_id/source_hash/transport_hash/encoded_bytes/qpc_ns/send_qpc_ns 等；传输 hash 针对 bitstream。
- receiver transport 日志核对完整 AU 的 transport_hash；解码日志另记 decoded_hash。
- 先比 sender/receiver 压缩 hash，再核对 decoded_hash 与 SHM/DShow。**不以 source_hash==decoded_hash 作为有损通过条件**；画质另验收。
- `pc1_seg` 源准备完成→末包提交返回；`rseg` 首片→完整 AU，不含后续解码；`decode_pipeline_ms` 首片→解码输出。均同机，不能拼成跨机 QPC 差。
- PyroWave encode_wait 包含 CPU 上传、Vulkan等待与 packetize，不是纯GPU时间；初始化/预热不混入稳态。

本机诊断入口 `rust/tests/dll_stream_smoke.py --codec pyrowave`，需显式参数/运行库；不继承旧脚本验收。Rust CLI 仍仅 Raw pattern 诊断，编码通过同 crate 的 DLL ABI。REST 入口见 rust-control，构建前提见 `docs/runbooks/build.md`。物理链路/双机/长稳/画质尚待专项验收。
