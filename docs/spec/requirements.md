# VirtualCapture 当前需求

## 目标与边界

被采集机保持 2K240 正常使用，以 DDA/WGC 采集、硬件 GPU 裁切/缩放和 BGRA→NV12，经2.5G网线传到主机，输出 DirectShow NV12 虚拟采集卡。只负责采集与传输，不做推理或独占全屏 Hook。

产品统一 Rust：采集后台 `vc-capture`、主机 GPUI `vc-host`、进程内 `vc_stream.dll` 与 Rust COM `vc_vcam.dll`。不得回退旧 C++/Python 产品。

## 会话与性能约束

- 默认640×640@240、Raw、DDA、GPU转换；预设416/640/720p/1080p/1440p和自定义偶数尺寸，crop/fit/fill/stretch。选项存在不代表该组合达标。
- 桌面转换仅硬件 GPU，无 CPU/WARP 降级。PyroWave 可显式启用 Vulkan1.3 GPU 计算；CPU MJPEG 已移除。仍允许 Npcap、hash、SHM、GPU上传/回读等必要 CPU 工作，不声称全链零CPU/零拷贝。
- 数据为 Npcap raw L2 EtherType `0x88B5`，冻结28B头；同一物理网卡保留IPv4控制，不要求移除IP。
- 正常枚举 Windows 状态为 Up 的物理非虚拟网卡，不按厂商或型号偏置；本机自动选择按控制连接出站路由精确匹配，对端仅在唯一候选时自动预览，多候选须明确选择。MTU可手动配置，新配置默认读取双端网卡有效值，不修改驱动参数。
- Raw NV12整帧614400B（640²）、@240约1.18Gbps像素载荷；MTU9000为69包/帧。预算包含L2开销与预留；超过容量拒绝，不能偷偷改编码/尺寸/fps。
- Raw完整像素FNV对账；有损只要求传输完整和解码/出口一致，画质单独验收。缺包不发布半帧，有界队列，停止后D12仅可重复最后好帧。

## 验收

冻结T0–T5与D1门限见 `.agents/skills/virtualcapture/references/test-matrix.md`。性能用motion并记录尺寸/fps/batch/adapter；跨机不相减QPC，Loopback不当双机证据。

Rust完整交付仍需：物理网卡整帧发送、双机240fps/长稳、DDA实采、编码画质、完整DShow编排、GUI非空白硬门及完整包/OTA。旧双机自动编排工具已移除，Rust等价工具缺口必须补齐后才可自动判T1–T5；当前缺测不放宽门限。

历史实用版本与证据保留，不因本重构自动部署或切换。现状以 PLAN 和带日期的 bring-up 报告为准。
