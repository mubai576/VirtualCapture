# P6 配置、预算与能力边界（D19，2026-11-02）

## 已实现与未实现

本契约的 `revision=1` 是控制面扩展，不是新数据面协议。当前原生程序、28B 包头、SHM v2、
DShow 都未改，仍只运行 `640×640 NV12 / 居中 1:1 裁切 / raw`。H.264/H.265 与可变分辨率
已获 owner 开发授权，但本阶段仅支持配置草案与预算；保存不意味着能启动。

- 预设：416×416、640×640、1280×720、1920×1080、2560×1440；自定义宽高均为 2..8192 的偶数整数。
- 旧配置合并默认值后仍为 640×640、240fps、crop、raw；保留 GPU 转换、batch、MTU 等隐藏值。
- `capture` 追加 `width,height,transform,codec,bitrate_mbps,max_bitrate_mbps`。
- `transform`: `crop` 居中 1:1 裁切；`fit` 保全画面等比缩放加黑边；`fill` 等比铺满后裁切；`stretch` 拉伸。
- `codec`: `raw|h264|hevc`；后两项未接入后端。目标/最大码率为整数 Mbps，范围 1..10000，最大不得低于目标。
- `transport.headroom_pct`: 0..50 的整数，默认 20；只影响预算，不改变历史性能判据。
- 帧率沿用 1..1000 整数的配置范围，不能据此宣称硬件达到该帧率。DDA 尚不按目标帧率硬限速。

## API

### GET /stream/capabilities

两种产品角色均可调用，返回：

```json
{
  "revision": 1,
  "presets": [{"width": 416, "height": 416}],
  "native": {"codecs": ["raw"], "width": 640, "height": 640,
             "transforms": ["crop"], "variable_resolution": false, "dda_fps_limit": false},
  "hardware": {"encoder": "unprobed", "decoder": "unprobed"},
  "adapter": null
}
```

示例省略其余四档预设。采集后台按该 HTTP 请求来源 IP 的控制路由精确选择网卡，填充 `adapter`；
失败保留 null 并附 `adapter_error`。主机没有自动选择另一张卡。能力描述是当前软件实现边界，
不是 GPU、原生二进制或分辨率/帧率组合的实机能力认证。`/health` 另加 `stream_revision=1`，
现有 `product_revision=2` 不变。

`/adapters.items[]` 和上述 `adapter` 新增 `receive_link_bps`、`transmit_link_bps`：
Windows `Get-NetAdapter` 的数值速率（bit/s），不是解析本地化的 `LinkSpeed` 文本。缺失/0 不假定为 2.5G。
同时附 `hardware_interface`、`virtual`；虚拟/非硬件网卡的标称速率不当作物理链路容量。

### POST /stream/plan

请求 `{config:{...部分配置...},refresh:false}`，两个字段均可省略；refresh 必须为布尔值。
按已保存配置合并并校验草稿，**不写配置、不启动进程、不注册虚拟卡、不更改网卡**。
采集后台调用返回 403，应在主机计算双端预算。校验失败 400，成功计算 200（即使组合不可运行）。

- `video`: 目标尺寸/fps/codec/transform 与 `raw_frame_bytes`。
- `network`: `status`, `is_estimate`, `host`, `capture`, `payload_bytes`, `packets_per_frame`,
  `payload_bps`, `wire_bps`, `link_bps`, `known_upper_bound_bps`, `headroom_pct`, `usable_bps`, `max_raw_fps`。
- `network.status`: `fits` 预算容纳；`margin` 未超物理线速但侵入余量；`exceeds` 已知端口足以证明超限；
  `down` 网卡未连接；`unknown` 信息不足，不能确认。
- `host` 用接收速率，`capture` 用发送速率；两端已知才给 `link_bps=min(rx,tx)`，仅一端已知保留 null。
  单端已知且需求更大仍可判 exceeds。回环及已识别的虚拟/非硬件设备不提供物理链路容量。
- `implementation`: `supported`, `reasons[]`, `hardware_verified:false`。预算 fits 不等于 supported。
- `can_start`: 原生路径已实现、MTU 一致、单帧包数不超 uint16，且没有 exceeds/down。
  未知速率只警告，不破坏既有 raw 的兼容启动；margin 只建议调整，不静默降级。
- `warnings[]`: 包含速率未知、旧后台缺探测接口、编码突发、DDA 帧率未硬限速、未实测吞吐等说明。

探测仅调用本机枚举和对端固定 GET `/stream/capabilities`；按 ROOT/IP/网卡缓存 10 秒（最多 16 项），
refresh=true 强制重取。UI 编辑防抖 600ms，单次请求串行，过时回复不覆盖新草稿。
产品启动重新探测，不使用预算缓存作为最终准入依据。

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

H.264/H.265 用最大码率均摊成每帧字节数，再估计分包开销；`is_estimate=true`，
`max_raw_fps=null`。码率窗口不保证单个 IDR 帧大小或瞬时突发，更不能证明编码器吞吐/画质。

## 启动保护与兼容

- `/product/start`、`/product/local/start` 在调用注册/拉起原生程序之前拒绝未实现的尺寸、transform 或 codec（409）。
- 主机产品启动若确定超过物理线速/网卡断开，或收发 MTU 不一致，注册前 409 拒绝。
- 低层 `/sender/start`、`/receiver/start` 仍是原有冻结 CLI；显式携带新尺寸/变换/codec/码率字段时 409，
  避免底层程序忽略字段后偷偷按 640 运行。没有新字段的历史调用不变。
- UI 允许保存待实现草案，明确显示“待接通/待接入”；总览禁止启动这些组合，停止/清理入口仍可用。
- 原有 raw 的帧哈希、性能门限与错误回滚不变。本阶段不引入编码流、不改 packet flags、不分配可变尺寸 SHM。
- 后续可变尺寸/编码需要显式会话能力协商与版本化 IPC；旧后台不能因版本号相同就被当成支持新模式。
