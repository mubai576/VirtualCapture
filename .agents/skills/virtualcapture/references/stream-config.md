# P6 配置、会话与能力契约（D19；P6-2，2026-11-03）

## 当前实现边界

开发版 `stream_revision=2`：Raw NV12 可变尺寸、crop/fit/fill/stretch、发送帧率上限、SHM v3 与动态 DShow 已接通。
稳定部署仍为封存的固定 640 管线，不自动更新。P6-3 编码/L2/解码/SHM/DShow已接通，默认仍只保存编码草案；明确启用开发功能并选择/协商两端后端后可运行。完整编码追加契约以 `encoded-stream.md` 为准；候选和类型协商不是实帧/双机/画质验收。

- 预设：416×416、640×640、1280×720、1920×1080、2560×1440；默认保留 **640×640@240 / crop / raw**。
- 自定义：偶数宽高 2..8192，整数 fps 1..1000；单帧包数不得超过 uint16。范围是软件参数校验，不是显存/吞吐/硬件认证。
- `capture`：`width,height,fps,transform,codec,bitrate_mbps,max_bitrate_mbps`，其余 GPU/batch/MTU 等隐藏值保留。
- 采集后端：`capture.mode=dda|wgc|pattern`；`capture.output`（0..15，默认0）选择显示器；`capture.wgc_window`（<=128 字符标题子串，空=显示器）仅 WGC 使用。WGC 仍经 DWM 合成，窗口模式包含光标，独占全屏不可采集；切换后端必须显式配置，不自动回退。
- `codec=raw|mjpeg|pyrowave`；H.264/H.265 已于 2026-11-13 移除。码率整数 1..10000 Mbps。Raw/非当前编码后端保留历史校验 max>=target；MJPEG/PyroWave 的目标码率不生效（GUI 中字段禁用），允许 max<target，原生分别独立限制每个值（100 kbps..2 Gbps）并保留描述符中的原值，最大码率仍限流。MJPEG/PyroWave 的质量或每帧预算、能力与精确会话追加见encoded-stream；Raw 不压缩、不静默切编码或降尺寸/帧率。
- `transport.headroom_pct`：0..50，默认20，仅预算建议，不修改历史验收门限。

## 变换与限速

源为 `capture.output` 指定的显示器（DDA/WGC，默认0）或 WGC 指定窗口，传输/虚拟卡尺寸与源桌面分辨率分离。不修改显示设置；DDA 旋转输出拒绝。
WGC以帧ContentSize为有效源范围；源变大/变小先释放旧帧并按ContentSize重建帧池，丢弃过渡帧，
不改变协商输出。crop变小到不足输出尺寸则明确失败；空/最小化暂不送帧。
捕获项Closed、原始窗口句柄/进程身份失效和FrameArrived异常为终止错误，不再伪装成无新帧超时；
窗口关闭不重新按标题寻找替代窗口。回调共享状态跨撤销/排队存活，
重建与取帧串行，回调无法退出时明确失败，不回退DDA。

- crop：居中 1:1，源必须不小于目标；偏移 `(source-output)/2` 下取整，保持历史 640 裁切逐字节结果。
- stretch：源整幅映射至目标整幅。
- fit：按宽高比选择较小缩放，活动宽/高下取到偶数且至少2；活动区域的居中偏移向下对齐偶数；黑边为 full-range Y=0/UV=128。
- fill：按宽高比选择源内裁切矩形，裁切宽/高下取偶数且至少2；源居中偏移向下对齐偶数，再铺满目标。
- 所有重采样：整数最近邻 `src=offset+floor(local*source_extent/dest_extent)`；2×2 输出 RGB 均值后沿用整数 BT.601。
  精确公式唯一实现在 `common/stream.h`，GPU shader 同构；仅 UNORM 解码沿用 D13 狭窄浮点例外。
- 帧率上限约束**每帧开始分包发送**的同机 QPC 间隔，至少 `ceil(1e9/fps)` ns，不补发赶进度。
  采集前等待至接近下个发送槽（减去上次转换成本，最多半帧）；DDA 合并中间 presents，只有一个当前帧，不排 FIFO。
  转换完成后另有最终发送闸，不能将阻塞的 AcquireNextFrame 起点当成实际发送时间。
- DDA timeout、pointer-only、同一 LastPresentTime 不发送，不递增 frame_id，不重放零帧或旧帧制造 FPS。
  `source_skipped` 包括 DXGI 合并帧和无图像更新；它不是收端丢包计数。fps 是上限，不保证源/设备达到该值。
- sender hashlog 保留 `qpc_ns`（采集/生成开始），追加 `send_qpc_ns`（开始分包发送）；两者不跨机相减。
  stats 双端追加 `width,height,frame_bytes,fps_limit,codec`，sender 另有 `transform,source_skipped`。

## 能力与预算 API

### GET /stream/capabilities

`{revision:2,presets[],native,hardware,adapter,adapter_error?}`。
`native` 含 `codecs,transforms,variable_resolution,dda_fps_limit,shm_version:3,errors[]`。
不能只凭 agent 版本断言原生支持：实际 sender/receiver `--capabilities` 和 DLL 的导出能力必须匹配。
DLL 由隔离 verifier 进程探测，不在 agent 内长时间加载；探测失败不回退旧二进制。结果按绝对路径/mtime/size 缓存。

capture 按 HTTP 来源控制路由精确选择网卡；host 不擅自选另一张快卡。`/adapters.items[]` 数值
`receive_link_bps/transmit_link_bps` 单位 bit/s，另附 `hardware_interface/virtual`；0、缺失、回环或虚拟网卡不冒充物理速率。
`hardware.encoder/decoder` 对新原生程序返回当前角色的候选清单（无帧验证），另一侧仍unprobed；编码时另实际协商类型，见encoded-stream.md，不把开发机结果外推到部署两端。`/health.stream_revision=2`，product_revision 仍为2。

### POST /stream/plan

请求 `{config:{...部分配置...},refresh:false}`；只读，不写配置、不启停进程、不注册设备、不改网卡。
校验失败400，capture角色403；成功计算200，即使配置不可运行。

- `video`：尺寸/fps/codec/transform/raw_frame_bytes。
- `network`：`status,is_estimate,host,capture,payload_bytes,packets_per_frame,payload_bps,wire_bps,link_bps,known_upper_bound_bps,headroom_pct,usable_bps,max_raw_fps`。
- `status=fits|margin|exceeds|down|unknown`；采用 host RX 与 capture TX 较慢者，两端已知才称 fits；一端慢速已足以证明 exceeds。
- `implementation={supported,reasons[],hardware_verified:false}`，`can_start` 还要求实际原生能力、peer revision=2、MTU 一致和无 exceeds/down。
  速度未知只警告，但旧 peer 缺会话协商是硬错误；margin 不自动降级。
- NIC/peer 探测按 ROOT/IP/网卡缓存10秒（最多16项），refresh 强制重取；启动必须 refresh。
  UI 防抖600ms、串行请求、旧回复不覆盖新草稿。编码均摊预算不保证 IDR 突发/编码吞吐/画质。

## 线速预算

保持现有分包规则 `P=mtu-42`（MTU9000 → P8958）。对 raw：

```text
B = width * height * 3 / 2
N = ceil(B / P)
last = B - (N-1) * P
wire_frame_bytes = B + N*66 + max(0, 18-last)
wire_bps = wire_frame_bytes * 8 * fps
usable_bps = min(host_receive_bps, capture_transmit_bps) * (100-headroom_pct) / 100
max_raw_fps = floor(usable_bps / (wire_frame_bytes * 8))
```

66 = 以太头14 + VC头28 + FCS4 + preamble/SFD8 + IFG12；很小的尾包补到最小以太帧。
不计 VLAN、隧道及其他业务流量；连接速率也不等于可持续吞吐，预留余量只提供工程建议。

MJPEG/PyroWave 用最大码率均摊成每帧字节数，再估计分包开销；`is_estimate=true`，
`max_raw_fps=null`。码率窗口不保证单帧大小或瞬时突发，更不能证明编码器吞吐/画质。
PyroWave 的每帧字节上限另受 `max_au_bytes−48` 与原始 NV12 帧大小限制，见 encoded-stream.md。

## 显式产品会话与低层兼容

28B `proto/vc_wire.h` 完全不改。控制面 descriptor：

```json
{"revision":2,"pixel_format":"nv12","codec":"raw","width":416,"height":416,
 "fps":240,"mtu":9000,"transform":"crop","source_mac":"02:12:34:56:78:ab"}
```

主机每次生成随机本地管理源 MAC 作为流隔离标识；它不是认证/加密，不替换系统 NIC MAC 配置。
sender 只改发出的以太帧 source，receiver 在解析 VC 包前精确过滤 source；不占用包头字段或保留 flags。

1. 主机检查 health/product/stream revision、版本、对端角色、两端实际原生能力和新鲜预算；失败必须在注册前拒绝。
2. 本机 receive 使用保存的 capture 尺寸/fps 与 receive MTU 校验 descriptor；注册原有 vcam，起 receiver。
3. `/product/local/start` 下发 `{capture:{...},auto_adapter:true,stream:<descriptor>}`；capture 校验全部键/类型/值，缺字段或不匹配409。
4. 双端启动结果回显 `stream`；sender 未确认同一 descriptor 时主机停止两侧并回收。旧后台不因同为0.4.1而被接受。
5. 一次会话中禁止热改尺寸；改配置先停止。自定义诊断角色保留旧 640 桩测试，不允许偷偷忽略可变尺寸配置。

CLI 追加 sender `--width/--height/--transform/--source-mac/--capabilities`，receiver
`--width/--height/--fps/--source-mac/--capabilities`；旧无参数默认为640×640@240。
低层 `/sender/start`、`/receiver/start` 接受成对 width/height、fps、source_mac（sender 另有 transform）；显式新字段必须先探测原生能力。
receiver 可只给 fps（几何沿用640×640）；编码请求即使不带几何也会把默认尺寸/帧率显式下发，避免原生缺省漂移。
sender 的 `output`（0..15）与 `wgc_window`（仅在 `mode=wgc`、<=128 单行文本）在此阶段严格校验，receiver 不接受采集选择字段；
校验失败在停旧进程之前返回 409，坏值不会静默回退默认显示器/窗口。
Raw 的码率参数、编码模式、收端缩放、错误几何等拒绝，不忽略。没有新字段的历史诊断调用保留原语义。
receiver 按会话严格校验 total、seq、每包 payload_len、pattern 一致性与完整字节覆盖，迟到包不得重新发布已完成帧。

## SHM v3 与 DShow

- 布局唯一来源 `vcam/vcam_shm.h`，magic `VCA3`，version3；64B header 中 offset32/36/40 为 width/height/fps，其余为保留。
  原12槽、64B槽头、data_off4096、seqlock、异步整帧哈希和交替事件保持；总长度 `4096+12*frame_bytes`。
- 每槽数据步长为 header.frame_bytes，不是编译常量614400。`hash_fid==frame_id` 才能使用异步 hash。
- writer 不复用已经存在的同名 mapping：提示关闭旧 receiver/预览/下游设备后重试，不覆盖其他会话的尺寸或活动映射。
- 新 reader 接受 v2 固定640×640@240 和合法 v3，检查实际映射容量；旧 v2 reader 遇 VCA3 判无信号。
  Python `scripts/vc_shm.py`、monitor 使用同一校验/动态步长，不能截断读取成640。
- DShow 在 filter 实例创建时从有效 mapping 固定格式，仅公布该会话一档尺寸/fps，allocator/样本长度跟随实际帧字节数。
  不再公布工作线程实际上不执行的假帧率。没有 mapping 时默认640但遵守D12不输出；先开始采集再在下游打开设备。
- 切尺寸必须在下游重新打开设备；已连接 filter 不偷偷换格式。D12 保留：从无有效帧不输出，有过有效帧后才可静默重复最后好帧。
- verifier 可 `--dll <绝对路径> --shm <隔离映射> --width W --height H --fps F --seconds N --hashlog ...`，
  在进程内加载 DLL、连接真实 allocator/IMediaSample sink，无 regsvr32；`--capabilities` 仅探测导出能力。
  诊断进程环境 `VCAM_FRAME_MAP` 可覆盖映射名，不修改 HKCU/系统环境；产品不设置此变量。

## 验证边界

`python scripts/p6_raw_verify.py` 为开发专用 motion/batch1/Loopback 全链路矩阵，不随便携包交付，也不调用5143或注册DLL。
`--dda` 要求真实变化桌面；测试不会自动启动动画源。实际报告见 `docs/bringup/bringup-20261103-p6-raw.md`。
本机证明不替代双机、物理链路吞吐或长期使用验收；编码开发模式按encoded-stream.md显式启用，不能套用 Raw 像素哈希门。

## P6-3 MJPEG 开发链路（非产品支持）

`vc-codec-probe` 与 `vc-codec-roundtrip` 已随 H.264/H.265 在 2026-11-13 一并移除，不再构建或交付。
MJPEG 只通过 sender/receiver 的 `--codec mjpeg --codec-backend turbojpeg:cpu --codec-hardware 0 --gop 1`
显式启用；能力/精确会话/载荷/日志契约见 encoded-stream.md。
验证脚本 `scripts/p6_codec_verify.py` 只接受 `--codec mjpeg`，不注册 DLL、不调用 5143；结果不代表双机、
长稳、画质或实时吞吐验收。

PyroWave（D24）同样只通过 sender/receiver
`--codec pyrowave --codec-backend pyrowave:vulkan --codec-hardware 0 --gop 1` 显式启用，
需要两端 Vulkan 1.3；`--pyrowave-selftest` 可单独验证编解码，无 Vulkan 报 SKIP 而不是通过。
构建与运行前提见 `docs/runbooks/pyrowave.md`，本机证据见 `docs/bringup/bringup-20261113-pyrowave.md`。
