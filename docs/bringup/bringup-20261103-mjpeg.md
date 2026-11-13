# MJPEG 实现与本机验证（2026-11-03）

## 结论与范围

owner要求“先转为 mjpeg 吧”（D21）。开发版新增可选MJPEG，保留Raw；采用libjpeg-turbo 3.1.2 CPU/SIMD，不是FFmpeg或GPU编码。
发送编码、L2封装、接收解码、SHM、DShow及GUI质量配置已打通。28B包头、Raw行为与历史验收门不变。
总闸`python scripts/verify.py --live Loopback`实际退出0，**401 passed / 2 skipped，ALL PASS**；不是双机/长期稳定/完整画质验收。

本轮640×640 motion合成源、Loopback、batch1下，两轮MJPEG实际约239.9fps，生成开始至DShow调用下游Receive前的同帧首次交付平均约1.7ms、P99约2ms。
真实本机DDA冒烟只有约34.5fps，不能把合成源低延迟外推成PC2桌面240fps通过。

当前稳定两端5143未切换，PC2未部署，网卡/注册表未修改，未关闭用户应用、未提交代码。
P6-3总体保持IN PROGRESS，双机并存与正式切换仍按D20另行确认。

## 实现与配置

- 双端精确身份`turbojpeg:cpu`，hardware=false、GOP1、编码在途1、同步CPU解码；接收网络队列仍最多2AU，不靠扩队列或重复帧凑fps。
- JPEG质量整数1..100，默认80；baseline 8-bit YCbCr420、快速整数DCT。NV12仅拆/合UV平面，不经过RGB或额外色域转换，100也不代表无损。
- 固定质量，不按公共目标`bitrate_mbps`做CBR；GUI禁用该目标输入并明确说明。最大码率仍限制发送起点最近一秒的压缩载荷（含48B封装）。超预算等待，不偷偷降质量/尺寸/fps配置。
- 原最大码率80Mbps默认不自动提高。本轮无瓶颈诊断显式使用800Mbps上限；内容复杂度不同，实际带宽也不同，不能把141Mbps当所有画面的保证值。
- 独立JPEG丢帧后下一完整帧即可恢复。注入丢失20后从21恢复。
- 解码拒绝截断、警告、非SOI/EOI完整帧、错尺寸、非420/8bit/YCbCr、渐进或算术编码；限制像素/扫描数/库中间缓冲，禁止编码输出缓冲自动扩容。
- agent/native能力追加`mjpeg_revision=1`；仅MJPEG descriptor包含revision/quality。旧peer、身份/质量不匹配或不支持的组合前置拒绝，无自动后端回退。
- GUI显式选MJPEG才使用CPU双端/GOP1/解码上限1预设，显示JPEG质量；不自动启用开发授权或切换运行中的会话，默认仍为Raw。

主要入口：`common/mjpeg_codec.h`、`cmake/Mjpeg.cmake`、`sender/encoder_backend.h`、`receiver/decoder_backend.h`、`agent/vc-agent.py`；契约见skill的encoded-stream/stream-config，构建与操作见`../runbooks/mjpeg.md`。

## 同机构建与延迟

所有组：NV12 640×640，pattern/motion/crop，Loopback/batch1、MTU9000、pacing0；同一最终构建。
240档每组7200帧（约30秒），另600帧验证60上限。这里“长测”相对短测，**不是长稳**。

- MJPEG：双端turbojpeg:cpu，Q80/GOP1/最大800Mbps/解码上限1。
- 保留p95分型recent64+0.1ms准备预测与帧率硬上限，无临时优先级/GPU参数实验。

延迟单位ms；由同frame_id的生成开始与首次下游交付入口QPC相减，不含显示器/应用后处理，不跨机相减QPC。
首次交付以vcam反馈环真实记录为准，排除停流重复样本；fps按真实发送起点全程间隔，分位独立重算，未删启动或尾部慢帧。

| 顺序 | 编码/上限 | 帧数 | 实际fps | 平均延迟 | P99 | 最大 | 压缩载荷Mbps |
|---|---|---:|---:|---:|---:|---:|---:|
| A1 | MJPEG/240 | 7200 | 239.860 | 1.734 | 2.155 | 3.474 | 141.147 |
| A2 | MJPEG/240 | 7200 | 239.968 | 1.675 | 2.007 | 3.224 | 141.211 |
| C1 | MJPEG/60 | 600 | 59.999 | 1.783 | 2.309 | 2.548 | 35.315 |

15600/15600帧完成发送、接收、DShow首次交付，反馈无溢出、未跳过接收帧；每帧压缩传输hash一致，解码与虚拟卡hash一致。
各组的对应源frame_id/hash一致，同codec两轮长测压缩hash/长度/keyframe逐帧一致；有损源与解码像素不套Raw相等门。
MJPEG长测编码等待平均0.530～0.542ms，发送至接收平均0.728～0.731ms。

本轮未结束任何用户应用。各组argv、完整hashlog、反馈、统计、GPU快照和原始输出全部保留在证据根`latency/`。

## 专项与回归

五项串行专项全部通过（`native-regressions.json`）：

| 项目 | 结果 |
|---|---|
| MJPEG尺寸矩阵 | 416²/640²/720p/1080p/1440p/418×282，各60帧@30，共360帧完整传输/解码/DShow核对 |
| Q60与Q90 | 各60帧@30、640²；对应源hash全部一致，全部压缩hash不同，压缩总字节3343076与5636497 |
| 最大80Mbps限流 | 640²@240/480帧，实际144.785fps；最高一秒载荷9998576B，小于10000000B；与800Mbps长测前480帧源/压缩hash/长度完全一致，只等待不降质 |
| MJPEG产品贯通 | 418×282@30/fill/Q86；真实HTTP→原生→DShow，收发121/121、DShow91样本；实际sender stats确认质量86。仅注册替身，未动注册表 |

其中Q60、Q90为两项；计五项。80Mbps证明滚动一秒上限，不以不到完整窗口的短测平均值冒充CBR。
另有MJPEG独立loss测试90帧@60/GOP1：故意丢帧20，89/89完整传输/解码，reset1/discard0，21至末帧连续恢复。
原生`--mjpeg-selftest`覆盖平面/全范围、2×2/非MCU尺寸、重复调用、错尺寸/截断/坏帧及输出容量；Python覆盖GUI质量/严格协商/旧peer拒绝/CLI互斥及打包必需许可证。

### 真实本机DDA冒烟：未达到240fps

本机当前2560×1440桌面，DDA/GPU转换、输出640²/fit/Q80、上限240/最大800Mbps，120/120传输/解码通过，源与解码各116个不同hash。
实际发送34.495fps，不是240fps；脚本未创建或移动动画窗口，也未用重复旧帧凑发送。

| 同机段 | 平均ms | 说明 |
|---|---:|---|
| 获取源帧/转换 source | 25.245 | 含AcquireNextFrame等待，不能当纯GPU计算 |
| JPEG编码等待 | 0.564 | CPU压缩发生在pump，计时包含该工作 |
| 接收首包后至解码完成 | 0.714 | 不含跨机时钟差 |
| capture开始至发送 | 25.859 | 含等待新桌面帧，不是采集完成后的帧龄 |
| 准备预测work | 1.049 | 排除AcquireNextFrame阻塞的诊断统计，不是另一种全链路延迟 |

这支持主要时间在获取/等待新桌面帧，而不是JPEG编码；没有核实显示器实际刷新率或完整隔离原因，不能说已确定30Hz显示器。
该机器不是PC2实际240Hz采集机。本轮没有测本机DDA完整首次交付延迟，也不外推PC2吞吐、实际双机单向或光到光延迟。

## 构建、依赖、许可证与交付边界

libjpeg-turbo不支持作为普通add_subdirectory依赖，使用独立ExternalProject，固定3.1.2源码和NASM2.16.03档案的SHA-256，TLS校验开启。
真实构建使用NASM/x86_64 SIMD=1，REQUIRE_SIMD=ON，静态库动态CRT；未向系统安装工具/库，未增加运行时DLL。
Runtime install实际验证7个原生目标/运行库/两份许可证：`licenses/libjpeg-turbo-LICENSE.md`、`licenses/libjpeg-turbo-README.ijg`，字节摘要与上游原文一致。
IJG归属声明及来源/摘要/离线缓存见`../runbooks/mjpeg.md`。不分发构建用NASM。

| 实际被测文件 | SHA-256 |
|---|---|
| vc-sender.exe | `481c3ac93bfcc4c7091a843c8962c32b4e14497b2ae6beeb055c13717ffc33bd` |
| vc-receiver.exe | `05aa5583e5818f2530a801a44ea9ae47f30af303acc23d9feca3401fc892f9ba` |
| vc-vcam-verify.exe | `dafde556d3247cffef92c6a1754d09e9e5995a80407487290b50f45e58a13e0d` |
| vc-vcam.dll | `d636edadef416b7dff35e6907980b0d0af89440bbf2b9e4f5b2ce2d76ad26f4f` |

使用现有一键脚本生成独立开发包，预定目录`dist/VirtualCapture-P6-MJPEG-20261103`，不覆盖旧包；实际交付与逐项复核结果以证据根`delivery.json`为准：

```powershell
python scripts/package.py --allow-dirty --output dist/VirtualCapture-P6-MJPEG-20261103
```

源码在开工前已含同会话的未提交诊断改动；保留并核对来源，不声称这些都是MJPEG新实现。未混入陌生并发改动，不commit或OTA。
独立包不是已上线；若5143已有别的目录运行，不应直接双击企图复用。需按D20/`../runbooks/p6-cutover.md`另行批准并存与正式切换。

## 证据索引

根：`run/mjpeg-20261103-154157/`（`run/mjpeg-current.txt`指向该目录）。二进制/日志/完整hashlog均为ignored运行证据，不入git。

- `baseline/`、`baseline.diff`、`baseline-status.txt`、`health-before.json`：开工状态与源码副本（tools未含在副本中，tracked差异另由baseline.diff保全）。
- `configure.log`、`build.log`、`tested-build.json`、`tested-build/`：实际构建与被测原生文件。
- `latency-summary.json`、`latency/`、`measure.py`：六组30000帧原始测量。
- `audit.py`、`independent-audit.json`：逐帧关联、完整性、分位/带宽独立重算。
- `regressions.py`、`native-regressions.json`、`quality-and-rate-audit.json`、`loss-mjpeg.log`：质量/限流/矩阵/恢复/产品与旧编码回归。
- `dda-640.log`：本机真实桌面冒烟，实际34.5fps。
- `verify-live-final.log`：最终总闸401 passed / 2 skipped，ALL PASS，退出0。
- `install-smoke.log`、`install-smoke/`：真实Runtime install及上游许可证原文。
- `delivery.json`：独立包、全量载荷/来源/被测哈希/旧包与常驻agent保护复核（仅成功交付后生成）。
