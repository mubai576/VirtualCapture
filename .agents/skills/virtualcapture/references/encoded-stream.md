# P6-3 编码链路开发契约（2026-11-03）

本文件补充 stream-config/agent-api/wire-protocol：Raw 的格式、判据和对账均不变。
开发版已接通 H.264/HEVC → L2 → 解码 → SHM v3/DShow；不代表 PC2 后端、双机、长稳、画质或实时吞吐验收完成。
GUI 默认仍为 Raw；编码必须明确选择后端并启用开发功能，不能自动回退。

## 显式配置与能力

`capture` 追加：

| 字段 | 缺省 | 含义 |
|---|---|---|
| codec_experimental | false | 明确允许开发版编码；false 只保存草案 |
| encoder_backend / decoder_backend | 空 | MFT CLSID（带花括号）或 `name:精确friendly name`；名称必须唯一匹配 |
| encoder_hardware / decoder_hardware | true / false | 明确枚举硬件类或系统/软件类，不自动换设备/类别 |
| gop_frames | 60 | 1..1000，每隔此数量输入帧请求 IDR；未兑现请求直接报错 |
| decode_in_flight | 2 | 1..8，解码在途上限；不能因超时自动加深 |
| max_au_bytes | 8388608 | 1024..16777216，含48B封装的访问单元内存上限；还须满足 uint16 分片数 |

原 bitrate_mbps / max_bitrate_mbps 保留；编码实现只接受 target<=max<=2000 Mbps。
目标请求 MF CBR；最大值另由 sender 按访问单元发送起点的一秒滑窗限制（计48B封装，不计L2开销）。
整个访问单元超过一秒预算或 max_au_bytes 时失败，不豁免IDR、不降画质/尺寸/fps。
滑窗可能降低实际输出频率，也不消除单帧包的微突发；界面预算仍只是均摊估算。

- `/health.stream_revision=2` 保留；sender/receiver `--capabilities` 另报 `codec_revision=1` 和三种 codecs。
  DLL 只消费 NV12，无须声称实现压缩codec。
- `GET /stream/capabilities` 的 native 另含 codec_revision；当前角色的 `hardware.encoder|decoder`
  可为 `{revision:1,status:candidates_only,backends:[{codec,identity,name,hardware,frames_tested:0}]}`。
  这是枚举，不代表成功激活/媒体类型/帧验证；旧程序不提供时仍不可用。
- `POST /stream/codec-check {capture:{...}}`：当前角色实际激活指定MFT、设置会话媒体类型及编码属性。
  返回200 `{revision:1,configured:true,frames_tested:0,stream_validated:false}`，失败409及明确错误。
  无网卡收发、无配置写入、无注册、不停旧进程；原生子进程12秒总超时。
- `/stream/plan` 编码时返回 `backends.encoder/decoder` 候选，核对 peer codec_revision，实际检查两侧所选MFT。
  `hardware_verified` 仍false：类型协商不是实帧验证；可开始也不意味着验收通过。
- 原生结果以绝对路径/mtime/size/参数缓存10秒，最多33项；原生进程真正启动仍会重新激活，失败可见并回滚。

编码 descriptor 保留revision2及原会话字段，追加 `codec_revision:1` 和上述全部新字段及两种码率。
字段集合、类型、值双端逐项确认；Raw descriptor 不追加编码字段，继续兼容P6-2。
低层 `/sender/start`、`/receiver/start` 编码调用也须明确提供开发开关、两端身份/类别及参数；先校验/探测再停旧槽。
Raw 显式提供编码参数仍拒绝，未知组合不忽略。

## 原生 CLI

两端共同追加（Raw 旧CLI不变）：

```text
--codec h264|hevc --codec-backend <{CLSID}|name:exact> --codec-hardware 0|1
--bitrate 20000000 --max-bitrate 80000000 --gop 60 --decode-in-flight 2 --max-au-bytes 8388608
--codec-backends
--codec-check <会话尺寸/fps/MTU与codec参数>
--au-selftest
```

缺省仍 raw；选择编码必须给后端，其他数值缺省如例（bitrate单位bit/s）。
`--codec-backends` 仅列当前角色候选；`--codec-check` 实际协商但不送帧；错误参数退出2，后端或运行失败退出1。
sender额外原生诊断 `--test-drop-frame N`：只在显式编码模式丢弃第N帧的第0个分片，hashlog标fault_injected，
不暴露到产品配置/API；测试必须写明注入，不能将其计入正常零丢包证据。

编码sendqueue发送须返回完整queue.len字节数；0或短发送立即失败，不自动换batch。已安装的本机Loopback在batch69有短发送，正向开发测试显式使用batch1；Raw历史实现/产品batch69默认值不变，也不外推物理网卡结论。

## 压缩载荷封装（不改28B包头）

`proto/vc_wire.h` 完全不改，EtherType仍0x88B5，不占任何保留flags或pattern值。
编码只由控制面会话约定；把下列48B封装与Annex-B编码字节整体按 `P=mtu-42` 分片：

| offset | 长度 | 小端内容 |
|---|---|---|
| 0 | 4 | VCAU magic 0x55414356 |
| 4/5/6/7 | 各1 | revision=1 / codec(1=h264,2=hevc) / kind(0=依赖帧,1=IDR,2=EOS) / header_bytes=48 |
| 8/12/16 | 各4 | width / height / fps，必须与控制会话完全相同 |
| 20/24 | 各4 | 编码字节数 / frame_id（必须与外层相同） |
| 28 | 4 | 保留0 |
| 32 | 8 | 编码字节FNV-1a64（不含此封装） |
| 40 | 8 | 保留0 |

EOS无编码字节且kind=2，使用下一个frame_id；有数据的kind=2或空图像帧拒绝。
bit0仍只按原始frame_id=0设置，不作为编码IDR；真实IDR在载荷内。
IDR须同时带Annex-B H264 SPS/PPS/type5，或HEVC VPS/SPS/PPS/type19/20；只有CleanPoint标记不够。

重组最多2个未完成单元，每个分配不超过max_au_bytes；严格检查seq/total/长度/CRC和同帧pattern/flags/qpc一致。
支持倒序分片和相同重复，冲突重复丢帧，完成帧不被迟到包再次发布。完整后再核对封装/长度/整体hash。
以太source MAC继续用于会话隔离（不是认证），不改系统网卡配置。

## 有界解码与恢复

Npcap线程只重组/校验；独立MF线程解码，等待队列最多2个访问单元，不等于2帧额外必需延迟。
排队溢出丢弃待处理队列、下一单元触发重置；frame_id断档/无效完整单元也重置解码器并等待IDR。
等待期间不解码依赖帧；定期强制IDR完成刷新，无新反馈包/控制面IDR请求。
`gop_frames`按输入帧计，源静止时不存在恢复墙钟上限；未声称零延迟或无损恢复。

解码的pending上限由decode_in_flight明确指定；5秒无法输出则失败，不自动增加队列。
本机HEVC路径常留1帧等待后续输入；EOS/正常停止drain回收尾帧。静态采集没有新输入时末帧可能滞后，必须显示pending。
MF解码报错会记录并重置等待IDR；不能偷偷换codec/设备/尺寸。输出严格验证时间戳、顺序、NV12尺寸/stride/aperture。
发布SHM v3与DShow契约不变；D12只允许重复已验证的最后好帧，从无有效帧不造图。

## 日志与指标

- 编码sender hashlog：frame_id/source_hash/transport_hash/encoded_bytes/keyframe/fault_injected/qpc_ns/send_qpc_ns。
- receiver `<hashlog>.transport.jsonl`：每个完整有效图像单元的frame_id/transport_hash/encoded_bytes/keyframe；不记EOS。
- receiver hashlog：每个真实解码输出的frame_id/decoded_hash/transport_hash/qpc_ns/decode_pipeline_ms（接收首包至解码完成，同机）。
- 正确性：对比sender与transport日志的压缩字节hash；另核对decoded_hash与SHM/DShow输出。
  **不能**把 source_hash==decoded_hash 或旧raw reconcile作为有损PASS门；画质另验收。
- receiver complete=真实解码数；compressed_complete不含EOS。追加transport_errors、decode_errors/resets/discarded/queue_drops/pending、
  decode_in_flight_limit、waiting_idr、failed；保留packet/drop/SHM与vseg计数。不伪造raw hash_mismatch=0。
  transport_errors包含坏分片/CRC与无效完整AU；packets_lost计入已丢不完整AU的缺片及坏包，不声称能数出完全未到达的AU。frame_id断档由解码reset/discard显式反映。
- sender追加encoded_bytes/keyframes/bitrate/max_bitrate/max_au_bytes/gop/rate_wait_ms，encoder仅1帧in-flight。
  两侧codec/宽高/fps_limit与真实会话一致。GUI区分Raw像素对账与压缩字节校验，并显示解码等待/重置/丢弃/错误。

验证脚本：`scripts/p6_codec_verify.py`（显式--codec/--encoder/--decoder，--matrix/--loss/--dda/--batch/码率/--decode-in-flight/--max-au-bytes参数）；
`scripts/p6_product_verify.py`支持同样明确的codec/encoder/decoder产品编排诊断。仅本机隔离、无注册、不部署。
完整证据见 `docs/bringup/bringup-20261103-p6-codec.md`；历史Raw判据原封不动。
