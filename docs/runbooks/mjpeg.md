# MJPEG 后端、构建与配置

## 路线

MJPEG 使用 **libjpeg-turbo 3.1.2 的 CPU/SIMD TurboJPEG API**，不是 FFmpeg 或 GPU 零拷贝。
NV12 的 UV 拆成 I420 平面后直接编码 JPEG；解码到 YUV 平面后交错回 NV12，不经过 RGB/额外色域转换。
每帧为独立 baseline 8-bit YCbCr 4:2:0 JPEG，无前视、B帧、参考帧或等待后续输入；仍须实测吞吐、带宽、画质与延迟。

主 GUI 选择 MJPEG 时显示 CPU 后端、GOP1、解码上限1及 JPEG 质量；这是用户明确选择新模式的预设，不自动启用开发授权或替换运行中的会话。
Raw 保留，默认仍为 Raw，旧部署不自动更新。H.264/H.265 已于 2026-11-13 按 D23 移除。MJPEG需要两端匹配的新 agent 和原生程序；缺能力直接拒绝，不回退。

## 设置

- 双端后端 `turbojpeg:cpu`，`encoder_hardware=false` / `decoder_hardware=false`，`gop_frames=1`。
- `jpeg_quality`：整数1..100，默认80；固定4:2:0、快速整数DCT。100仍不等于无损，快速DCT的高质量行为也不等同准确DCT，不宣称完整画质通过。
- 质量固定，不应用目标 `bitrate_mbps` 进行CBR控制；该公共配置字段保留，但界面在MJPEG下禁用并明确说明不适用。
- `max_bitrate_mbps` 仍限制发送起点的最近一秒压缩载荷预算（含48B封装），不是JPEG编码器目标。超过预算会等待并降低实际fps，绝不自动降质量。
- MJPEG带宽往往明显高于Raw的压缩预期。原来的80Mbps上限可能不足以承载640@240；按真实统计和网卡预算显式设置，不自动提高上限。
- 单AU超过`max_au_bytes`或一秒预算立即失败。固定编码输出缓冲禁止库重分配；单帧在途，接收排队上限仍为2，不扩队列掩盖性能。
- 解码只接受完整SOI/EOI、精确会话尺寸、8-bit YCbCr420 baseline、非算术JPEG，警告按错误处理。限制图像像素、扫描数及库中间缓冲，不自动缩放或换采样。
- 丢失帧只需等下一个完整JPEG即可恢复，不依赖GOP，也不等待后续参考帧。

原生命令示例（两端还需各自指定网卡、源MAC、尺寸、输出日志）：

```text
--codec mjpeg --codec-backend turbojpeg:cpu --codec-hardware 0
--gop 1 --jpeg-quality 80 --decode-in-flight 1 --max-bitrate 800000000
```

本机隔离诊断（Loopback/batch1，不注册、不部署）：

```powershell
python scripts/p6_codec_verify.py --codec mjpeg --encoder turbojpeg:cpu --decoder turbojpeg:cpu `
  --encoder-hardware 0 --width 640 --height 640 --fps 240 --frames 1200 `
  --jpeg-quality 80 --max-bitrate 800 --decode-in-flight 1 --test-seconds 12
python scripts/p6_product_verify.py --codec mjpeg --encoder turbojpeg:cpu --decoder turbojpeg:cpu --jpeg-quality 86
```

诊断max-bitrate单位Mbps。`--loss`使用帧20丢失/帧21恢复，至少45帧。
`--mjpeg-selftest`验证NV12平面/全范围、2×2及非MCU尺寸、重复调用、截断/坏帧/尺寸不符及固定输出容量。
所有有损结果只对压缩传输hash和真实解码/虚拟卡hash，不套Raw源像素相等门。

## 构建与离线缓存

从根目录正常运行 `cmake -S . -B build`、`cmake --build build --config Release` 或一键 `python scripts/package.py`。
`cmake/Mjpeg.cmake`下载并逐次校验固定档案，使用独立ExternalProject构建静态库，不修改thirdparty、不全局安装NASM/库、不增加运行时DLL。
要求SIMD开启，缺NASM/SIMD直接构建失败，不静默编成慢速版本。MSVC静态库使用与产品一致的动态CRT。

| 缓存路径（相对构建目录） | 来源 | SHA-256 |
|---|---|---|
| `_deps/downloads/libjpeg-turbo-3.1.2.tar.gz` | `https://codeload.github.com/libjpeg-turbo/libjpeg-turbo/tar.gz/refs/tags/3.1.2` | `560f6338b547544c4f9721b18d8b87685d433ec78b3c644c70d77adad22c55e6` |
| `_deps/downloads/nasm-2.16.03-win64.zip` | `https://www.nasm.us/pub/nasm/releasebuilds/2.16.03/win64/nasm-2.16.03-win64.zip` | `3ee4782247bcb874378d02f7eab4e294a84d3d15f3f6ee2de2f47a46aa7226e6` |

离线构建预放上述两份字节一致的档案。源码/工具解压在`_deps/libjpeg-turbo-3.1.2`和`_deps/nasm-2.16.03`，静态库在`_deps/libjpeg-turbo-build`；缓存都是产物，不入git。
构建脚本不关闭证书校验；来源下载或SHA失败不继续编译。版本、摘要和依赖声明随源码记录；便携包载荷清单包含实际EXE与许可证。

## 许可证与分发

This software is based in part on the work of the Independent JPEG Group.

静态链接仍须遵守IJG、Modified BSD及相关zlib许可证。源文件与许可证保持上游原样，未修改上游实现。
CMake Runtime install随成品携带`licenses/libjpeg-turbo-LICENSE.md`及`licenses/libjpeg-turbo-README.ijg`原文；一键打包缺其中任一项即失败。
NASM仅用于构建，不随产品分发；工具本身许可证保留在构建缓存的LICENSE。不得把MJPEG依赖或NASM档案作为运行证据提交进git。

接口/载荷/能力契约见skill `references/encoded-stream.md`；本机实现及测试证据见 `../bringup/bringup-20261103-mjpeg.md`。

## 缓冲优化与回归

开发版已去掉sender的临时JPEG复制和receiver解码后的整帧复制，复用封装/分片缓冲及本帧压缩hash；sendqueue只按需增容，不改变batch或短发送失败规则。
直接解码只写未发布槽，JPEG错误时abandon，成功后才publish；仍不是GPU或全链路零拷贝。
本机固定Q80交叉对照只有小幅平均延迟改善，P99与总CPU收益未证实，不以降低质量、删校验或扩队列冒充优化。
证据见`../bringup/bringup-20261103-mjpeg-opt.md`。`python -m pytest ctrl/tests/test_mjpeg_buffers.py -q`编译并注入实际sender/receiver的内存Npcap替身，验证分配/发送失败清理、sendqueue复用及连续坏JPEG后的槽回收；不需要网卡或注册表操作。
