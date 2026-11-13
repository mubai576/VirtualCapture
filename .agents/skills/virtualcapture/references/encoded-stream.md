# P6-3 编码链路开发契约（MJPEG / PyroWave；2026-11-13 移除 H.264/H.265）

本文件补充 stream-config/agent-api/wire-protocol：Raw 的格式、判据和对账均不变。
owner 于 2026-11-13 决定移除 H.264/H.265（含 Media Foundation 与原生 NVENC）及其测试结果，
只保留每帧独立的开发编码：D21 的 MJPEG 与 D24 的 PyroWave（Vulkan 计算小波，intra-only）。
两者都不代表 PC2 后端、双机、长稳、画质或实时吞吐验收完成。
GUI 默认仍为 Raw；二者都必须明确选择匹配的两端后端并启用开发功能，不能自动回退。

## 显式配置与能力

`capture` 追加：

| 字段 | 缺省 | 含义 |
|---|---|---|
| codec_experimental | false | 明确允许开发版编码；false 只保存草案 |
| encoder_backend / decoder_backend | 空 | 必须与 codec 匹配：MJPEG 精确 `turbojpeg:cpu`，PyroWave 精确 `pyrowave:vulkan`；其他身份拒绝 |
| encoder_hardware / decoder_hardware | false | 现有后端都不是厂商硬件编解码器，true 拒绝，不冒充硬件 |
| gop_frames | 60 | 现有编码必须为 1；每帧独立 |
| decode_in_flight | 2 | 1..8，解码在途上限；不能因超时自动加深 |
| max_au_bytes | 8388608 | 1024..16777216，含48B封装的访问单元内存上限；还须满足 uint16 分片数 |
| jpeg_quality | 80 | 仅 MJPEG：整数质量1..100，固定420/快速整数DCT |

原 bitrate_mbps / max_bitrate_mbps 保留；编码实现只接受 target<=max<=2000 Mbps。
目标码率不对现有后端生效：MJPEG 用恒定质量，PyroWave 只把 `max_bitrate÷fps` 当每帧字节上限；
最大值由 sender 按访问单元发送起点的一秒滑窗限制（计48B封装，不计L2开销）。
整个访问单元超过一秒预算或 max_au_bytes 时失败，不豁免、不降质量/尺寸/fps。
滑窗可能降低实际输出频率，也不消除单帧包的微突发；界面预算仍只是均摊估算。

- `/health.stream_revision=2` 保留；sender/receiver `--capabilities` 另报 `codec_revision=1`，
  codecs 只含 `raw` 与编入的 `mjpeg`/`pyrowave`；另有 `mjpeg_revision=1`、`pyrowave_revision=1`。
- `GET /stream/capabilities` 的 native 另含 codec_revision/mjpeg_revision/pyrowave_revision；当前角色的
  `hardware.encoder|decoder` 为 `{revision:1,status:candidates_only,backends:[{codec:"mjpeg",
  identity:"turbojpeg:cpu",name,frames_tested:0},{codec:"pyrowave",identity:"pyrowave:vulkan",...}]}`。
  这是枚举，不代表成功激活/媒体类型/帧验证。
- `POST /stream/codec-check {capture:{...}}`：当前角色实际初始化所选编码器或解码器。
  返回200 `{revision:1,configured:true,frames_tested:0,stream_validated:false,mjpeg_revision:1|pyrowave_revision:1,encoder_configuration:{...}}`，
  失败409及明确错误。无网卡收发、无配置写入、无注册、不停旧进程；原生子进程12秒总超时。
- `/stream/plan` 编码时核对 peer codec_revision 与对应 revision，实际检查两侧所选后端身份。
  `hardware_verified` 仍 false：类型协商不是实帧验证；可开始也不意味着验收通过。
- 原生结果以绝对路径/mtime/size/参数缓存10秒，最多33项；原生进程真正启动仍会重新初始化，失败可见并回滚。

编码 descriptor 保留 revision2 及原会话字段，追加 `codec_revision:1` 与上述全部编码字段与两种码率；
MJPEG 另加 `mjpeg_revision:1`、`jpeg_quality`，PyroWave 另加 `pyrowave_revision:1`（不带 jpeg_quality）。
字段集合、类型、值双端逐项确认；
Raw descriptor 不追加编码字段，继续兼容 P6-2。低层 `/sender/start`、`/receiver/start` 编码调用
也须明确提供开发开关、两端身份及参数；先校验/探测再停旧槽。Raw 显式提供编码参数仍拒绝。

## MJPEG 扩展（D21；mjpeg_revision=1）

sender/receiver 的 capabilities 及 agent native 追加 `mjpeg_revision:1`、codecs 中的 `mjpeg`，
候选为两端 `turbojpeg:cpu` / hardware=false / family=turbojpeg。
这表示编入固定 libjpeg-turbo 3.1.2 CPU/SIMD 后端，不代表已通过特定尺寸/fps；不是 FFmpeg，
不映射 MF 或自动选 GPU。
MJPEG descriptor 在原编码字段上追加 `mjpeg_revision:1`、`jpeg_quality:1..100`，要求双端同类型/同值回显。
双端 identity 必须精确为 `turbojpeg:cpu`、硬件类别 false、GOP 1；其他组合拒绝。
配置允许保留不活动的 JPEG 质量，但非 MJPEG 显式 CLI/低层 API 提供该项即拒绝。
旧 peer/原生程序无 mjpeg_revision、没有 mjpeg codec 或 codec-check 未确认完整参数时，停止/注册前拒绝；
不会自动切回 Raw。

`--jpeg-quality 1..100`（默认80），两端传递用于精确会话确认，实际编码仅使用质量，
不应用目标 bitrate 做 CBR；UI 在 MJPEG 下禁用目标码率，明确最大码率只是原一秒滑窗的发送上限。
编码量超过上限时等待，实际 fps 可能下降，不自动改质量或宣称预算保证吞吐。
源 NV12 拆 UV 后直接编码 JPEG YUV420，解码再交错回 NV12，无 RGB 中转或额外范围变换。
固定 baseline 8-bit YCbCr420、快速整数 DCT；每帧独立 keyframe、encoder 在途1，同步 CPU 解码无后续帧依赖。
网络队列仍最多 2AU，decoder 显式上限保持，不用加深队列跑通。
JPEG 头尺寸/采样/精度/颜色/非渐进/非算术严格校验，warning 视为失败，限制像素/扫描/中间内存；
固定编码输出缓冲不得重分配，超过 max_au 失败。

`--codec-check` 仍 configured/frames_tested=0/stream_validated=false，额外 mjpeg_revision 和 encoder_configuration。
两端均回显会话编码配置：family=turbojpeg、identity、version、quality、width/height/fps、
subsampling=420、dct=fast_integer、rate_control=constant_quality、target_bitrate_applied=false、
encoder_in_flight=1、config_submitted=true。
解码端回显 quality 用于确认会话，不表示 JPEG 解码器会再次量化或控制质量。sender stats 同样记录；
encoder_quality=null。
CPU 编码在 pump 执行，encode_wait_ns 包含同步 CPU 压缩，不是 GPU/MFT 等待。
构建/固定依赖摘要/许可证见 `docs/runbooks/mjpeg.md`；selftest 见 `--mjpeg-selftest`。
JPEG 质量 100 仍非无损，画质/色彩须另验收。

## PyroWave 扩展（D24；pyrowave_revision=1）

owner 于 2026-11-13 批准接入 PyroWave（Themaister/PyroWave，MIT；Granite 运行时子集，MIT）：
Vulkan 1.3 计算着色器的 CDF 9/7 小波 intra-only 编码，无熵编码，无 GOP、无参考帧。
sender/receiver 的 capabilities 及 agent native 追加 `pyrowave_revision:1`、codecs 中的 `pyrowave`，
候选为两端 `pyrowave:vulkan`、hardware=false、family=pyrowave。未编入 `VC_HAS_PYROWAVE` 的原生程序
不得声明该能力；缺能力时明确拒绝，不回退 MJPEG/Raw。

PyroWave descriptor 在原编码字段上追加 `pyrowave_revision:1`，不带 `jpeg_quality`；
非 PyroWave 显式提供该 revision 或 PyroWave 显式提供 jpeg_quality 均拒绝。
双端 identity 必须精确为 `pyrowave:vulkan`、硬件类别 false、GOP 1，且两端 backend 与 codec 匹配。

阶段一（当前）只走 PyroWave 的 CPU 缓冲路径：sender 以 NV12 上传（`upload_path=cpu_nv12`），
receiver 同步解码：亮度直接写入会话 NV12 输出缓冲，色度平面（`decode_path=cpu_yuv420p`）解出后
交错为 NV12 UV——任何情况下不得发布未写亮度或只有色度更新的帧；没有 D3D11/Vulkan
外部内存互操作，也没有 GPU 侧缩放或帧池。解码输出仍是完整 NV12 帧，SHM v3/DShow 契约不变。
占用一个 Vulkan 逻辑设备；两端各自创建默认设备，不跨进程共享。

每帧预算是 `min(max_bitrate_mbps×10^6÷8÷fps, max_au_bytes−48, 一帧 NV12 字节数)`，在会话建立时固定并
双端回显（`max_bytes_per_frame`）；它只是 PyroWave 的每帧输出上限（超限时编码器在该帧内降低精度），
不是恒定码率、不保证画质，也不替代发送端一秒滑窗限流。目标码率不生效；没有质量旋钮。
单帧 bitstream 作为一个 PyroWave packet 输出，仍整体套 48B VCAU 封装再由传输层分片；
`compute_num_packets` 必须返回 1，否则明确失败。

PyroWave 会话配置回显：family=pyrowave、identity=pyrowave:vulkan、revision=1、width/height、
subsampling=420、max_bytes_per_frame、upload_path=cpu_nv12、decode_path=cpu_yuv420p。
解码端回显 max_bytes_per_frame 为 0（未设置编码预算），其余同构；双端逐项确认才 configured。

Vulkan 1.3 设备不可用（如无合适 GPU/驱动）时创建失败：`--pyrowave-selftest` 报 SKIP 且不冒充通过，
产品启动在协商阶段明确失败，不自动换后端。GPU 型号/驱动版本属于部署前提，不是本项目验收结论。
构建/固定提交摘要/许可证见 `docs/runbooks/pyrowave.md`；本机功能边界见
`docs/bringup/bringup-20261113-pyrowave.md`。

## 原生 CLI

两端共同追加（Raw 旧 CLI 不变）：

```text
--codec mjpeg --codec-backend turbojpeg:cpu --codec-hardware 0 --gop 1
--codec pyrowave --codec-backend pyrowave:vulkan --codec-hardware 0 --gop 1
--bitrate 20000000 --max-bitrate 80000000 --decode-in-flight 2 --max-au-bytes 8388608
--jpeg-quality 1..100          # 仅 MJPEG；PyroWave 提供即拒绝
--codec-backends
--codec-check <会话尺寸/fps/MTU与codec参数>
--au-selftest
--mjpeg-selftest
--pyrowave-selftest
```

缺省仍 raw；选择编码必须给匹配后端，其他数值缺省如例（bitrate 单位 bit/s）。
`--codec h264|hevc` 及其他编码名在打开网卡前以退出码2拒绝；不提供任何替代后端。
`--codec-backends` 列出编入的 MJPEG/PyroWave 候选；`--codec-check` 实际初始化但不送帧；
错误参数退出2，后端或运行失败退出1。`--pyrowave-selftest` 无 Vulkan 1.3 时报 SKIP（退出2），
不得计为通过。
sender 额外原生诊断 `--test-drop-frame N`：只在显式编码模式丢弃第N帧的第0个分片，hashlog 标 fault_injected，
不暴露到产品配置/API；测试必须写明注入，不能将其计入正常零丢包证据。

编码 sendqueue 发送须返回完整 queue.len 字节数；0 或短发送立即失败，不自动换 batch。
已安装的本机 Loopback 在 batch69 有短发送，正向开发测试显式使用 batch1；Raw 历史实现/产品 batch69 默认值不变。

## 压缩载荷封装（不改28B包头）

`proto/vc_wire.h` 完全不改，EtherType 仍 0x88B5，不占任何保留 flags 或 pattern 值。
编码只由控制面会话约定；把下列 48B 封装与 JPEG 编码字节整体按 `P=mtu-42` 分片：

| offset | 长度 | 小端内容 |
|---|---|---|
| 0 | 4 | VCAU magic 0x55414356 |
| 4/5/6/7 | 各1 | revision=1 / codec(3=mjpeg,4=pyrowave) / kind(1=独立帧,2=EOS) / header_bytes=48 |
| 8/12/16 | 各4 | width / height / fps，必须与控制会话完全相同 |
| 20/24 | 各4 | 编码字节数 / frame_id（必须与外层相同） |
| 28 | 4 | 保留0 |
| 32 | 8 | 编码字节FNV-1a64（不含此封装） |
| 40 | 8 | 保留0 |

EOS 无编码字节且 kind=2，使用下一个 frame_id；有数据的 kind=2 或空图像帧拒绝。
bit0 仍只按原始 frame_id=0 设置，不作为编码关键帧；完整性由载荷内完整 JPEG 保证。
MJPEG 每个非 EOS 单元必须 kind1，载荷为完整 SOI..EOI JPEG，不能发送 kind0。
PyroWave 每个非 EOS 单元同样必须 kind1，载荷是完整单 packet bitstream（非空且至少 8 字节），
解码器自行校验其头部/尺寸/image count；封装布局和 28B 包头不变。
完整解码时继续验证 JPEG 头及精确 NV12 尺寸；PyroWave 输出尺寸由会话 w/h 校验。
编解码历史值 1/2（H.264/HEVC）已移除，遇到时按无效单元拒绝，不尝试兼容或回退。

重组最多 2 个未完成单元，每个分配不超过 max_au_bytes；严格检查 seq/total/长度/CRC 和同帧 pattern/flags/qpc 一致。
支持倒序分片和相同重复，冲突重复丢帧，完成帧不被迟到包再次发布。完整后再核对封装/长度/整体 hash。
以太 source MAC 继续用于会话隔离（不是认证），不改系统网卡配置。

## 有界解码与恢复

Npcap 线程只重组/校验；独立工作线程运行所选编码的解码，等待队列最多 2 个访问单元，不等于 2 帧额外必需延迟。
排队溢出丢弃待处理队列、下一单元触发重置；frame_id 断档/无效完整单元也重置解码器。
MJPEG/PyroWave 每帧独立：断档重建后立即可解码下一个完整帧，不等待 GOP，也不需要 IDR。
PyroWave 解码是同步 Vulkan 提交+CPU 回读并转 NV12；不会为追帧加深队列或在途上限，
GPU 忙/设备丢失按解码错误处理并重置，不静默换设备。
流断档/队列丢弃只清解码器状态并重建解码器对象（保留 Vulkan 设备）；只有解码失败才整体重建编解码器。
这样才能避免设备创建时间长于帧周期、接收队列反复溢出形成重置死循环（2026-11-13 实测并修复）。
gop_frames 固定为 1；源静止时没有恢复墙钟上限。
解码 pending 上限由 decode_in_flight 明确指定；5 秒无法输出则失败，不自动增加队列。
解码报错会记录并重置；不能偷偷换 codec/设备/尺寸。输出严格验证时间戳、顺序、NV12 尺寸/stride/aperture。
发布 SHM v3 与 DShow 契约不变；D12 只允许重复已验证的最后好帧，从无有效帧不造图。

## 日志与指标

- 编码 sender hashlog：frame_id/source_hash/transport_hash/encoded_bytes/keyframe/fault_injected/qpc_ns/send_qpc_ns/encode_wait_ns/prepare_lead_ns/prepare_work_ns。
  encode_wait_ns 为输入提交返回至取得输出的同机 wall-clock，含同步 CPU 压缩等待。
  prepare_lead_ns 为本帧采集前使用的预测提前量；prepare_work_ns 为实际源生成/转换+提交+编码等待+封装耗时，DDA 不含 AcquireNextFrame 阻塞。
  两者均不是额外队列或纯 GPU 时长；旧日志缺字段不补造证据。
- receiver `<hashlog>.transport.jsonl`：每个完整有效图像单元的 frame_id/transport_hash/encoded_bytes/keyframe；不记 EOS。
- receiver hashlog：每个真实解码输出的 frame_id/decoded_hash/transport_hash/qpc_ns/decode_pipeline_ms（接收首包至解码完成，同机）。
- 正确性：对比 sender 与 transport 日志的压缩字节 hash；另核对 decoded_hash 与 SHM/DShow 输出。
  **不能**把 source_hash==decoded_hash 或旧 raw reconcile 作为有损 PASS 门；画质另验收。
- receiver complete=真实解码数；compressed_complete 不含 EOS。追加 transport_errors、decode_errors/resets/discarded/queue_drops/pending、
  decode_in_flight_limit、waiting_idr、failed；保留 packet/drop/SHM 与 vseg 计数。不伪造 raw hash_mismatch=0。
  transport_errors 包含坏分片/CRC 与无效完整 AU；packets_lost 计入已丢不完整 AU 的缺片及坏包，不声称能数出完全未到达的 AU。
  frame_id 断档由解码 reset/discard 显式反映。
- sender 追加 encoded_bytes/keyframes/bitrate/max_bitrate/max_au_bytes/gop/rate_wait_ms，encoder 仅 1 帧 in-flight。
  encoder_quality=null；encoder_configuration 为所选编码的会话参数（MJPEG 或 PyroWave 回显）。两侧 codec/宽高/fps_limit 与真实会话一致。
  PyroWave 的 encode_wait_ns 含 CPU 上传、Vulkan 提交等待与 packetize，不是纯 GPU 时间；也不含解码端。
  GUI 区分 Raw 像素对账与压缩字节校验，并显示解码等待/重置/丢弃/错误。
- sender `timing_ms` 为最近10000个已发送帧的各段 `{samples,avg,p99,max}`（单位ms，window_frames=10000）：
  prepare_wait（采集前等待）、source（生成/采集）、sample_submit（sample 创建拷贝+提交）、
  encode_wait（提交返回至取得输出）、au_pack（验证/封装/预测更新前）、rate_wait（发送限速）、transmit（分包发出）、
  audit_hash（发送后完整源 hash 及压缩 hash；MJPEG 复用本帧封装已计算的压缩 hash，不再二次扫描 JPEG）、
  log_io（逐帧日志与 flush）、capture_to_send（采集/生成开始至发送开始，含上述部分阶段，不能再求和）。
  只记录已发帧；DDA 无新帧的超时、初始化、EOS/drain 和 stats 写盘不计各段，不等于进程 CPU 或纯 GPU 时间。
- MJPEG 内存优化不改 CLI/API/descriptor/载荷：sender 只在本帧发送完成前借用 TurboJPEG 固定输出，下一次 pump 前结束借用；
  复用封装/分片缓冲，sendqueue 只按需增容且保持完整发送断言。
  receiver 将 JPEG 直接解码到已 acquire、seq 为奇数的帧槽；全部解码与 UV 合并成功才 publish。
  解码异常/重置必须 abandon 未发布槽，不暴露部分像素；pending/队列上限、逐帧 CRC/压缩 hash/源与解码 hash 证据均保留。
  缓冲复用/固定质量的同条件对照与故障注入见 `docs/bringup/bringup-20261103-mjpeg-opt.md`，不是零拷贝或双机延迟保证。
- 编码 sender 进入管线前与 Raw 一样调用发送线程调度设置（MMCSS Capture，失败时沿用 TIME_CRITICAL；进程 HIGH_PRIORITY_CLASS）。
  不更改系统全局/GPU 优先级，不改变后端、编码参数、队列或帧率/码率闸。
  帧率/码率闸采用同机截止等待（长等待粗睡，最后短段精旋），发送间隔仍至少 ceil(1e9/fps)，不追赶。
  采集准备时间按最近64次实际耗时，采用 nearest-rank p95（ceil(N×0.95)-1 下标）+0.1ms，最多提前一个帧周期。
  空窗口提前量0；少于20个样本时该分位仍取最大值，64个样本最多排除3个高值；持续变慢不会被当成单次异常忽略。
  若预测不足则晚到后立即发送，但仍服从帧率/码率闸；不跳过输入、不追赶、不静默改 fps 或画质，只允许一帧编码在途。
  stats 追加 preparation：policy=p95_recent64_by_frame_type、window_samples=64、margin_ns=100000 与 inter/idr 的 samples/lead_ns。
  最终 lead_ns 只是当前预测，不是实际延迟保证。
- 编码 receiver 首包时刻取 Npcap 读取返回后，不能取可能阻塞的读取之前。

验证脚本：`scripts/p6_codec_verify.py`（显式 `--codec mjpeg --encoder turbojpeg:cpu --decoder turbojpeg:cpu`，
支持 `--matrix/--loss/--dda/--batch/码率/--decode-in-flight/--max-au-bytes/--jpeg-quality`）；
另支持成对 `--width/--height`（不能与 matrix 同时使用）、`--fps/--frames/--transform`、
`--encoder-hardware/--decoder-hardware` 固定0 和 DDA 显式 `--cpu-convert`，缺省仍 GPU 转换。
`--gop` 固定1；`--loss` 使用帧20丢失、要求帧21至末帧全部恢复/解码丢弃0。
`--test-seconds` 为 receiver 总存活秒数 3..122，DShow 观察两秒更短；frames/fps 必须严格小于 test-seconds-2。
这是理想最短时长校验，不保证设备吞吐足够；不足或任一子进程失败即报错，统一有界等待后只清理本轮子进程。
诊断结果区分发送/真实解码实测 fps、源/解码/DShow 的不同 hash 数量与含停顿重复的 DShow 样本总数，
另记 send_interval_ms、sender_capture_to_send_ms 和 sender_timing_ms；均为单端 QPC，不跨机相减。
sender_preparation 逐帧重放并严格核对 p95/64次淘汰/周期上限；另报预测与实际 work 分布、work 超过预测的帧数及 cold_frames。
DDA 的 pattern=null，不冒充 motion。ROI 静止时即使解码 hash 因有损重编码变化，也不能据此证明新画面。
`decode_pipeline_ms` 只统计 receiver 首包到解码输出的同机段，不含编码/跨机时延/DShow 出口，不是端到端延迟。
`scripts/p6_product_verify.py` 支持 `--codec raw|mjpeg` 的产品编排诊断。仅本机隔离、无注册、不部署。
MJPEG 诊断需明确两端 turbojpeg:cpu 且 hardware=0；原 H.264/HEVC 参数、GOP15/帧30 恢复规则及相关报告已移除。
`scripts/p6_codec_verify.py` 也接受 `--codec pyrowave --encoder pyrowave:vulkan --decoder pyrowave:vulkan`
（无 `--jpeg-quality`，GOP1，hardware0；`--loss` 同样要求帧20丢失后从帧21起恢复），
额外核对 `encoder_configuration` 的全部字段与每帧字节上限，并对 motion 源断言每个解码帧哈希互不相同
（缺失亮度等"只更新色度"的缺陷会因此失败）。`scripts/p6_product_verify.py` 也已接受
`--codec pyrowave --encoder/--decoder pyrowave:vulkan`（隔离 HTTP/真实原生/DShow，注册为替身）。
两者都只是本机功能证据，不得据此声称产品级/双机验收。
