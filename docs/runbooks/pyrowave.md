# PyroWave 后端、构建与配置（D24，开发版）

## 路线

PyroWave 是 [Themaister/PyroWave](https://github.com/Themaister/PyroWave)（MIT）的 Vulkan 1.3 计算着色器
intra-only 小波编码：CDF 9/7 DWT、64×64/32×32 系数块、无熵编码、每帧独立、无可变 GOP。
本仓库以固定提交构建 `libpyrowave-shared-1.dll` 并静态/动态组合其运行时依赖 Granite（MIT）子集。

阶段一（当前）只使用 PyroWave 的 CPU 缓冲路径：sender 以 NV12 上传，receiver 同步解码到 YUV420P 后
交错回 NV12；没有 D3D11/Vulkan 外部内存互操作，也没有 GPU 侧缩放、帧池或零拷贝。
Raw 与 MJPEG 保留，默认仍为 Raw，旧部署不自动更新；H.264/H.265 已于 2026-11-13 按 D23 移除。
PyroWave 需要两端匹配的新 agent 和原生程序；缺能力直接拒绝，不回退 MJPEG/Raw。

## 设置

- 双端后端 `pyrowave:vulkan`，`encoder_hardware=false` / `decoder_hardware=false`，`gop_frames=1`。
- 没有质量旋钮。每帧预算 `min(max_bitrate_mbps×10^6÷8÷fps, max_au_bytes−48, 一帧 NV12 字节数)`
  在会话建立时固定并双端回显；PyroWave 用它限制单帧输出（超限时在该帧内降低精度），
  不保证达到、也不是恒定码率或画质。目标 `bitrate_mbps` 不生效，界面在编码模式下禁用。
- `max_bitrate_mbps` 仍限制发送起点的最近一秒压缩载荷预算（含48B封装）；超过预算会等待并降低实际 fps，
  绝不自动降质量、降尺寸或降帧率。
- 单帧 bitstream 必须作为 1 个 PyroWave packet 输出（`compute_num_packets` 返回非 1 即失败），
  整体套 48B VCAU 封装后再由传输层分片；单 AU 超过 `max_au_bytes` 或一秒预算立即失败。
- 解码只接受完整、自包含的帧；设备丢失/解码错误按错误计数并重置，不静默换设备。
- 丢失帧只需等下一个完整帧即可恢复，不依赖 GOP，也不等待后续参考帧。
- 两端都必须有 Vulkan 1.3 设备与驱动；无合适设备时创建失败（selftest 报 SKIP），不换后端。

原生命令示例（两端还需各自指定网卡、源MAC、尺寸、输出日志）：

```text
--codec pyrowave --codec-backend pyrowave:vulkan --codec-hardware 0
--gop 1 --decode-in-flight 1 --max-bitrate 400000000
```

本机隔离自检（不注册、不部署）：

```powershell
build\vc-sender.exe --pyrowave-selftest   # 或 vc-receiver.exe，输出 PYROWAVE: PASS/SKIP/FAIL
build\vc-sender.exe --codec-backends
```

`--selftest` 也包含 PyroWave 段；无 Vulkan 1.3 时报 SKIP 且不计为通过。

本机隔离诊断（Loopback/batch1，不注册、不部署）：

```powershell
python scripts/p6_codec_verify.py --codec pyrowave --encoder pyrowave:vulkan --decoder pyrowave:vulkan `
  --width 640 --height 640 --fps 120 --frames 120 --max-bitrate 200 --decode-in-flight 2 --batch 1
# 丢失恢复：--loss（帧20缺片，要求帧21起全部恢复），需至少45帧
python scripts/p6_product_verify.py --codec pyrowave --encoder pyrowave:vulkan --decoder pyrowave:vulkan
# 隔离HTTP→真实原生→SHM/DShow，注册为替身，不写真实注册表
```

脚本核对压缩字节 hash、真实解码 NV12 到 SHM/DShow、发送最小间隔、一秒最大载荷窗口，
并逐项核对 `encoder_configuration`（含 `max_bytes_per_frame`）与每帧上限。
流断档/丢帧只重建 PyroWave 解码器对象、保留 Vulkan 设备；解码失败才重建整个编解码器。

## 构建与离线缓存

从根目录正常运行 `cmake -S . -B build`、`cmake --build build --config Release` 或一键 `python scripts/package.py`。
`cmake/Pyrowave.cmake` 下载并逐次校验固定档案，用独立 ExternalProject 只构建 `pyrowave-shared` 目标
（`PYROWAVE_DEVEL=OFF`、`PYROWAVE_UTILS=OFF`），把 Granite 的 CMake 图隔离在本项目之外。
构建要求 Vulkan-Headers（随缓存提供），不要求系统安装 Vulkan SDK；运行要求目标机 Vulkan 1.3 驱动。
MSVC 需 C++17；缺源码/摘要不符直接构建失败，不静默跳过。

| 缓存路径（相对构建目录） | 来源 | SHA-256 |
|---|---|---|
| `_deps/downloads/pyrowave-a32b05e….tar.gz` | `https://codeload.github.com/Themaister/pyrowave/tar.gz/a32b05e03d70826b19d4d11ff74fe166ed74371b` | `38245f2e76c6656907d39cd50b13dcb3d73ea8cc174eb28b6b38d7106db3e2f8` |
| `_deps/downloads/granite-5309042….tar.gz` | `https://codeload.github.com/Themaister/Granite/tar.gz/53090429539bf3404250d18fe58db971f0907c04` | `51765130fa3a476e1a7c82d1656d9d5f58a53a2e7642483df9b09d0d7dd8d7a8` |
| `_deps/downloads/volk-47cddf7….tar.gz` | `https://codeload.github.com/zeux/volk/tar.gz/47cddf7ed97b94118a08aacb548a411188e016cc` | `ed771f9132ea077af0abc74d29f415a81f181f83c3ac5e0f67b2f2149976fe2d` |
| `_deps/downloads/vulkan-headers-6802bb4….tar.gz` | `https://codeload.github.com/KhronosGroup/Vulkan-Headers/tar.gz/6802bb4733b63ed5efd3adb308a6c885ef180ea1` | `9044fbb356160e43201dc2094acb44c8a906722a63a35f48fc0f051579db9bb6` |

离线构建预放上述四份字节一致的档案。源码解压在 `_deps/pyrowave-src`（Granite/volk/Vulkan-Headers
分别位于其 `Granite/`、`Granite/third_party/volk`、`Granite/third_party/khronos/vulkan-headers`）；
构建目录 `_deps/pyrowave-build`，产物 `Release/libpyrowave-shared-1.dll`。缓存都是产物，不入 git。
构建脚本不关闭证书校验；来源下载或 SHA 失败不继续编译。

`libpyrowave-shared-1.dll` 必须与 exe 同目录（构建后自动复制到目标目录和 build 根目录）；
便携包 Runtime install 一并携带它与两份许可证原文。

## 许可证与分发

PyroWave 与 Granite 均为 MIT。CMake Runtime install 随成品携带
`licenses/pyrowave-LICENSE.txt` 与 `licenses/granite-LICENSE.txt` 原文；一键打包缺其中任一项即失败。
源文件与许可证保持上游原样，未修改上游实现；VOID/volk/Vulkan-Headers 仅构建期使用。
不得把 PyroWave 依赖档案作为运行证据提交进 git。

接口/载荷/能力契约见 skill `references/encoded-stream.md`；本机实现及测试证据见
`../bringup/bringup-20261113-pyrowave.md`。

## 已知边界（未验收）

- 只有本机 Loopback/隔离产品编排的功能与短时运行证据；PC2 部署、双机、长稳、画质（PSNR/色彩）与实时吞吐未验收。
- 两端各创建一个 Vulkan 逻辑设备；GPU 驱动崩溃/设备丢失只表现为解码/编码错误与重置，不自动恢复设备。
- 640×640@240 本机回环已在软重置修复后通过（480/480、零丢弃），但仍不代表双机 240fps 吞吐或画质验收；
  早期 240 丢弃死循环的根因是断档重建 Vulkan 设备，已修复（见证据文档）。
- 解码亮度必须直接写入会话 NV12 缓冲、色度交错为 UV；`--pyrowave-selftest` 分别检查亮度/色度非零，
  `p6_codec_verify.py` 要求 motion 源的每个解码帧哈希互不相同。
