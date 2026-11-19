# Rust 控制面 R1 / R2（D26 / D27，独立开发分支）

本文件只约束 `rust/`。Python agent 原契约不变。允许重新设计控制 API 不等于解冻 28B wire；
本阶段仍使用原有 `vc-sender.exe` / `vc-receiver.exe` / `vc-vcam.dll`。

## 拓扑与构建

- 被采集机：`vc-capture` CLI，默认 `0.0.0.0:5143`，无 GUI。
- 主机：`vc-host` GPUI 原生客户端，直接调用 `vc-core::host::HostController`，没有本机 REST 服务。
- `vc-core` 不依赖 GPUI，拥有配置/预算/会话/原生进程/虚拟卡管理与主机编排。未来 C ABI DLL 应封装它，
  当前未导出 DLL 或声明稳定 Rust ABI。DShow 设备 DLL 和软件集成 DLL 是不同用途。
- `cargo build --manifest-path rust/Cargo.toml --locked`；依赖锁在 `rust/Cargo.lock`。
- 两个入口都支持 `--root <runtime>`、`--native-dir <native-output>`。`VC_ROOT` / `VC_NATIVE_DIR` 也可指定。
  `--root` 优先；相对根自动沿 exe 向上找 AGENTS/version.json，便携布局回退 exe 目录。
- 独立便携ZIP由 `python -I rust/packaging/package.py --allow-dirty` 构建，唯一dist目录，附app-local MSVC DLL、原生数据面、PyroWave DLL、许可与SHA-256清单，不包含Npcap驱动。
  主机日常入口`VirtualCapture.exe`（构建产物`vc-launcher.exe`）使用Windows GUI子系统与原生图标，直接解析current并拉起GPUI，
  不调用CMD/PowerShell；支持`--root`/`--native-dir`及对应环境变量，默认根为入口所在目录；在release目录内运行时识别外层固定ROOT。
  启动日志/不含令牌的PID回执写`<data>/run/host-*`，失败弹出原生消息框。0.2.0通过OTA进入本版时，新host首帧前补齐ROOT入口，
  已有入口保留。原`Start-Host.cmd`/PowerShell仅兼容诊断；`Start-Capture.cmd`直接运行CLI。
  采集启动器无参数默认`0.0.0.0:15143`，可直接传exe参数`--port`/`--bind`；exe本身与主机省略端口仍5143。
  CLI控制台保留供Ctrl+C退出；主机Release使用Windows图形子系统，自行分配隐藏控制台，避免启动器把GPUI主窗口一起隐藏。
  新包含`vc-updater.exe`，可通过Rust更新端点暂存；不兼容旧Python OTA，不自动部署或注册DShow。
- `--root`始终是固定运行根。有`app/current.json`的managed根使用`app/releases/<build_id>/`程序，
  `data/vc-rust-config.json`配置、`data/run/`证据、`updates/{staged,worker,incoming}/`与`updates/history.jsonl`更新事务。
  无current的扁平根保留`ROOT/vc-rust-config.json`与`ROOT/run/`，首次激活只复制配置/令牌到data，旧文件不移动；
  回滚首次更新移除指针即可恢复扁平布局，已有运行证据不迁移/覆盖。启动器按current选择程序，native-dir显式覆盖优先。
- 不读取/迁移/覆盖旧Python `vc-config.json`。配置不存在使用默认；语法损坏/版本未知明确失败，保存同目录临时文件刷盘后原子替换。

## REST v1

普通控制API仅用于受控直连网络；更新接口要求独立Bearer令牌。没有 `/exec`、`/files`、浏览器 `/ui`。
主机通过 `implementation=virtualcapture-rust`、`api_revision=1`、`role=capture`、`ok=true` 验证身份，
拒绝旧 Python 后台；HTTP 失败保留具体错误体。普通请求总超时5s，网卡枚举/prepare/start为40s，
容纳有界的原生枚举10s、PowerShell元数据15s和能力探测；超时仍按会话清理。

| 方法 | 路径 | 请求 / 返回 |
|---|---|---|
| GET | `/health` | `ok,implementation,api_revision,version,role,running,pid,root,session_id,build,layout`；pid 是后台 pid，build为active标识或null，不含更新令牌 |
| GET | `/version` | Rust crate version + implementation/api_revision；不以旧 version.json 充当 Rust 版本 |
| GET/POST | `/config` | GET 完整 Config；POST 部分对象合并校验，失败不写，运行中409；成功 `{saved,config}` |
| GET | `/adapters?refresh=true` | `{items:[AdapterInfo...]}`，refresh 强制重新探测；普通查询缓存10s |
| POST | `/sender/prepare` | SenderRequest → PreparedSession；校验实际原生能力、路由、网卡、Raw参数和预算，不启停/写配置 |
| POST | `/sender/start` | 同请求/响应；已有活动 sender 或同会话运行证据时409，拒绝覆盖；启动存活检查350ms不代表首帧验收 |
| POST | `/sender/stop` | `{session_id}`；匹配后停止，错误会话不得停当前活动进程；停止后可幂等重复 |
| GET | `/sender/stats` | 原生 stats 合并 session_id/running/adapter/pid/exit_code；异常含 error/log_tail |

SenderRequest 全部字段：

```json
{"capture":{"mode":"pattern","pattern":"motion","gpu_convert":false,"fps":60,"frames":120,
"batch":1,"mtu":9000,"width":416,"height":416,"transform":"crop","codec":"raw",
"dst_mac":"ff:ff:ff:ff:ff:ff"},
"session_id":"s-example","source_mac":"02:01:02:03:04:05","auto_adapter":true,"adapter":""}
```

capture 其他字段采用 Config 默认；主机实际下发完整 capture 并逐字段比对响应，不凭省略值确认。
PreparedSession 为 `{capture,session_id,source_mac,adapter:AdapterInfo}`。
会话号3..80 ASCII字母/数字/连字符，不接受路径；源MAC必须本地管理单播。
两端产品列表仅保留 Windows 明确 `hardware_interface=true` 且 `virtual=false` 的设备，显式排除回环；
元数据未知不当作物理设备。展示、自动选择、手动选择、prepare/start统一使用该列表，不允许手填隐藏设备绕过。
主机枚举失败清空旧列表；已保存接收选择若不再合格则清除，要求手动重选，不静默换到另一张卡。
没有合格的已选物理接收网卡不能启动。被采集机自动选择按控制连接出站IPv4唯一匹配，不选首卡。
`vc-capture --list-adapters`保留完整诊断枚举；后台显式`--allow-loopback-diagnostics`后才允许本机回环控制器使用
`\Device\NPF_Loopback`，不把回环速率冒充物理容量。普通GUI不会选择或启用诊断回环。
主机先 prepare，再将确认的精确网卡名固定在 start 中，不在启动时静默换卡。

## Rust OTA v1（D27）

更新端点全部要求 `Authorization: Bearer <64hex>`，普通控制请求不携带令牌。采集端首次创建
`data/update-token.json`（legacy根目录）；仅显式`--print-update-token`可打印，health/日志不泄漏。
主机一次配对存`update-peer-token.json`，不进普通配置/包。错误令牌401。

| 方法 | 路径 | 请求 / 返回 |
|---|---|---|
| GET | `/update/status` | `{ok,layout,active,running,staged,history}`，active/staged含build_id/version，历史有界 |
| POST | `/update/stage` | 原始ZIP、`X-VC-SHA256`；≤512MiB；成功暂存，重复同内容幂等，不切current |
| POST | `/update/activate` | `{build_id}`；停止状态202，返回helper PID后优雅退出 |
| POST | `/update/rollback` | `{build_id}`；指定保留版本，停止状态202，经同helper校验/切换 |

ZIP须单一顶层目录、`package-manifest.json` product/schema/version/build_id/files完整；拒绝路径穿越、
绝对路径、盘符、重复、符号链接、加密、数量/总大小越界和逐文件大小/哈希不符，不把SHA-256当发布者签名。
运行中可暂存，采集或待清理时不能激活。helper从`updates/worker/`运行，等待原PID结束，复核载荷、原子切current、
启动新程序；capture确认Rust身份/ROOT/build与就绪，host通过`--update-ready <唯一path>`首帧后回执pid/ROOT/build。
失败恢复旧指针并重启，旧程序/配置/历史证据保留；暂存或HTTP202不等于激活成功。

主机更新入口在折叠面板，选择同一ZIP可分别暂存本机/上传采集机；应用本机更新正常退出后由helper重开。
令牌可重新配对，更新错误必须可见；回滚只选择校验通过且保留的历史build。
上传预检/哈希/HTTP均在工作线程，上传总超时180s；采集端激活后主机有界确认新PID/build。
旧0.1.0程序尚无OTA，首次进入新通道需要一次bootstrap；后续使用固定ROOT更新，不上传到旧Python `/update`。
目录与事务设计见`docs/proposals/rust-ota-layout-20261114.md`。

## 能力、预算与生命周期

R1 仅 Raw 可运行；MJPEG/PyroWave 草稿可保存但启动明确拒绝，不降级。DDA/WGC/pattern 与四变换继续交给原生。
双方实际 exe 必须报告 stream_revision=2、variable_resolution=true、shm_version=3。
自动注册虚拟卡前，通过 `vc-vcam-verify.exe --dll <实际DLL> --capabilities` 隔离探测相同能力，
不只相信文件存在；工具查找 native-dir/普通原生路径、DLL所在构建目录的兄弟vcam_verify/Release，
及 ROOT/build/vcam_verify/{Release,Debug}。
预算沿用 `stream-config.md`；超过物理速度拒绝，侵入预留余量为 margin，未知速率警告。
非法几何/fps/MTU、单帧65535分片上限、两端MTU不一致等均为硬错误。

开始：验证 Rust 对端 → fresh 网卡/prepare/预算 → 注册本目录虚拟卡 → receiver → sender → 参数回显。
发现已有虚拟卡注册则拒绝接管，开发验证不覆盖正在使用的部署。
停止/失败/正常窗口关闭：按会话号停发端 → 停本机 receiver → 只回收本控制器注册的匹配 DLL。
HTTP start 超时也保留待清理会话，不能误当成没开始；清理未确认显示残留并可重试停止。
进程退出/对端失联执行同样的本机回收，配置与连接锁定至清理完成。
强杀/断电不保证用户态清理。NativeSlot 正常释放会停子进程，CTRL_BREAK 2s后强杀。

## 验证边界

`python scripts/verify.py` 继续验证旧实现，不能代替 Rust tests。
`cargo test --manifest-path rust/Cargo.toml --workspace` 验证 Rust 契约。
独立 `vc-stream` Npcap Raw 双向 Loopback 冒烟与 SHM v3 诊断证据见[数据面 bring-up](../../../../docs/bringup/bringup-20261119-rust-data-plane.md)；该工具未进入本控制面、产品启动生命周期、绿色包或 OTA。
`python -I rust/tests/control_smoke.py` 在随机回环端口、独立ROOT、私有SHM跑 Raw motion/batch1全量哈希对账；
不注册DShow、不碰稳定5143。`rust/tests/gui_smoke.py` 打开真实GPUI窗口、输入随机回环端口并点击连接，
要求本机错误桩收到 `/health` 并返回503，保存初始/错误截图，正常关闭退出0；每次使用独立目录。
本机回环不代替双机/240fps/长稳验收。`vc-transport`为产品集成起点（冻结wire/分片/校验/有界重组）；独立`vc-stream`仅实现Raw Npcap诊断与可选SHM v3发布，未接入控制器或产品生命周期，产品采集、Npcap、SHM与DShow仍走原生路径。物理NIC/双机、DDA/WGC、DShow接线、SDK DLL与编码控制迁移未完成。OTA按本节独立验证，不继承旧通道验收。
