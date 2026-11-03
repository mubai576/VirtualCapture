# Agent API（冻结端点，dual-PC 通用）

Base: `http://<ctrl-ip>:5143`（2026-10-29 起产品固定端口；旧部署 18080 须更新并重启）。全部 JSON. `start` 幂等（先停旧会话）。
每个会话带 `session_id`，双端 stats 用它对齐。

| 方法 | 路径 | 入参 | 出参 |
|---|---|---|---|
| GET | `/health` | — | `{ok, version, role, running, updater, pid, staged, product_revision:2, stream_revision:2, product_role:capture|receive, root}`；root 为运行目录绝对路径，启动器据角色、能力与目录识别已有服务 |
| GET | `/version` | — | `{version[, git_sha]}` |
| GET | `/adapters` | — | `{text, names[], items[{name,description,label?,mac?,status?,receive_link_bps?,transmit_link_bps?,hardware_interface?,virtual?,addresses?[{ip,prefix}]}], exe}`；速率为数值 bit/s，虚拟网卡不视为物理容量，Npcap 列表与 Windows 网卡 GUID 关联，不启动采集；自动选择时使用完整 NPF 设备名 |
| POST | `/update` | 二进制 zip → `{staged, version}`（只 stage，不重启）；`{rollback:true}` → 经 activate 回滚 prev |
| POST | `/activate` | `{version}` 或 `{rollback:true}` | `{activated, version, rollback, restart}`（`restart=execv`，原地接管，无 detached 进程） |
| POST | `/sender/start` | `{mode, pattern, fps, frames, ethertype, mtu, pacing_us, adapter*, dst_mac, ...}`（*adapter 必填；P6-2 追加成对 width/height、fps、source_mac；sender 另有 transform，详见 stream-config.md） | `{session_id, args}` |
| POST | `/sender/stop` | — | `{session_id, stopped}` |
| GET | `/sender/stats` | — | `{session_id, running, sent_frames, sent_packets, pps, bps, fps_actual, send_errors, pattern, gpu_ms, gpu_dispatch_ms, gpu_dispatch_samples, pc1_seg_avg_ms, pc1_seg_p99_ms, pc1_seg_max_ms, gpu_convert, cpu_pct, cap_lag_avg_ms, cap_lag_p99_ms, cap_lag_max_ms}`（P4-1：gpu_ms=wall-clock；gpu_dispatch_ms=D3D11 timestamp 真 GPU 拷贝时间，0 样本时为 0） |
| POST | `/receiver/start` | `{adapter*, ethertype, mtu, expect_frames, timeout_s, shm}`（*adapter 必填；P6-2 追加成对 width/height、fps、source_mac；sender 另有 transform，详见 stream-config.md） | `{session_id, args}` |
| POST | `/receiver/stop` | — | `{session_id, stopped}` |
| GET | `/receiver/stats` | — | `{session_id, running, complete, dropped, packets_recv, packets_lost, pps, bps, gaps, hash_mismatch, inter_arrival_avg_ms, inter_arrival_p99_ms, rseg_avg_ms, rseg_p50_ms, rseg_p99_ms, rseg_max_ms, shm_published, vseg_avg_ms, vseg_p50_ms, vseg_p99_ms, vseg_max_ms, vcam_delivered, vcam_repeats, hash_queue_max}`（vseg=receiver 发布→vcam 交付，同机 QPC；shm 布局见 vcam/vcam_shm.h v3） |
| GET | `/receiver/report` | -- | {version, sender, receiver} (both-side stats snapshot) |
| POST | `/pcap/start` | `{seconds}` |
| POST | `/exec` | `{argv:[...], timeout_s}` (allowlist, see below) | `{exec_id, exit_code, stdout, stderr, timed_out, duration_s}` |
| GET | `/exec/log?lines=N` | - | `{entries:[{ts, client, argv, exit_code, timed_out, duration_s}]}` | 失败留证抓包（限时，避免 9K 包撑爆盘） |
| GET | `/files?path=<rel>[&offset=N][&limit=M]` | relative path + byte range | {path, size, offset, truncated, next_offset, content} (under ROOT; every response capped 1MB; default head 256KB; bad offset/limit 400; follow next_offset until null) |

Impl: `agent/vc-agent.py` single-file zero-dep，同一份程序以 `--role host`（默认，主机收端）或 `--role capture`（被采集机后台）启动；兼容 receiver/receive 与 sender 别名。`--role` 对这几个产品角色已不再只是标签；其他自定义标签保留低层诊断配置行为。端口默认 5143，显式 `--port` 保留给隔离测试/诊断。C++ file IPC (atomic stats rewrite + hashlog append); agent starts/stops/passes through. Also: GET /files reads any file under ROOT with optional paging (0.4.0). Adapter rule (P1-1): `--adapter` 必填；agent 缺 adapter 直接 400 并附本机 adapters 列表，不拉起进程；C++ 空 adapter 按 pcap_findalldevs 顺序逐卡试发（首包成功锁定，全败 exit 2）；显式坏 adapter exit 2 并列卡；live 发送 sent_frames>0 但成功包为 0 则 exit 3。

## P5 GUI 端点（D16，自 0.5.0）
| 方法 | 路径 | 请求 | 返回 |
|---|---|---|---|
| GET | `/` `/ui` | — | 仅主机提供内置控制台（HTML，no-store）；采集后台返回 404，不渲染 UI |
| GET | `/config` | — | `{role, peer_url, adapter, capture{mode,pattern,gpu_convert,fps,batch,mtu,dst_mac,width,height,transform,codec,bitrate_mbps,max_bitrate_mbps},transport{headroom_pct}, receive{shm,mtu,vcam_autoregister}}`（ROOT/vc-config.json，缺省值见 agent；新流配置与实现边界见 stream-config.md） |
| POST | `/config` | 部分字段（未知键丢弃，子对象逐键合并） | `{saved, config}`；校验失败 400 `{error, details[]}`。产品角色由启动方式固定；主机 peer_url 只接受 `http://IPv4:5143`，UI 只填 IP。adapter 接受安全描述子串或完整 `\Device\NPF_{GUID}` / `\Device\NPF_Loopback`；运行时拒绝更改连接配置（409） |
| GET | `/vcam/status` | — | `{dll, registered, match}`（HKCU InprocServer32 是否指向本机 ROOT 下 dll） |
| POST | `/vcam/register` | — | regsvr32 /s 本机 dll；同步 FrameMap，Motion=0；200 需 exit=0 且 match。产品 UI 不提供手动注册入口 |
| POST | `/product/local/start` `/product/local/stop` | 采集后台 start 必须带 `{capture:{...},auto_adapter:true,stream:<descriptor>}`；成功回显 stream | host 起/停 receiver（固定默认 SHM，timeout 0），接收前注册 vcam、接收结束回收；capture 起/停 sender，参数由主机下发。auto_adapter 按请求来源 IP 的本机出站地址精确匹配系统网卡，匹配不唯一时 409，不随意选首卡 |
| POST | `/product/start` | — | 主机发起：检查采集后台 product_revision=2、stream_revision=2、角色、版本与实际原生能力 → 注册并启动本机接收 → 0.5s → 下发采集设置；被采集机无需配置 peer_url/网卡。获取到主机选定网卡 MAC 时自动单播。发端失败先尽力停发端，再停止并回收收端。`{ok, steps[{side,code,result,rollback?}]}`；采集后台不接受此编排命令（403） |
| POST | `/product/stop` | — | 先停发端再停收端并回收 vcam；对端失联也继续处理本机。`{ok, steps}`；部分失败时 ok=false |
| GET | `/product/status` | — | `{ts, local{online,config,version,sender,receiver,vcam}, peer{online,url,version,config,sender,receiver,vcam}|null}`；对端只调固定 GET 端点（无通用反代） |
- `/sender/start` 新增可选 `gpu_convert:true` → `--gpu-convert`。
- 访问控制同既有端点（无鉴权，直连网段内使用）；对端调用只走 `peer_url` + 固定路径。
- 控制台（P5-U2，2026-10-29，owner 批）：只有主机 UI；连接配置无角色/共享内存/注册选项/对端控制台链接，打开页面自动取网卡列表。开始/停止采集共用一个按钮；有残留 managed vcam 时仍可重试停止回收。离线/停止/桩进程不显示实时指标，换会话重建帧率基线。
- 主机产品配置固定 `receive.shm=Local\VirtualCaptureVcamFrame`、`vcam_autoregister=true`；旧 peer_url 的 IPv4 在读取时迁移至 5143，保存时校验。接收启动前注册，正常停止、启动失败回滚、接收子进程退出（后台每秒检查）与主机正常退出时回收；只注销本目录匹配的 DLL，不碰其他目录注册。`vcam.managed` 表示待回收资源；回收失败保留状态并重试。强制终止进程不保证清理，下游已打开的 COM 实例也不会被强制关闭。
- 产品模式缺少原生程序时直接失败，不起桩；原有低层诊断模式的桩语义保留。sender/receiver stats 可附 `adapter`（实际启动网卡）。未更改 raw L2 协议与性能判据。
- P5-U3：产品启动检查原生进程约 1 秒内是否提前退出；提前退出返回 500，附限长原生日志，并执行已有回滚。这是启动存活检查，完整帧到达由 stats 判断；界面在 complete=0 时显示等待首帧。低层 `/sender/start`、`/receiver/start` 的启动语义不变。
- stats 追加 `pid`、`exit_code`（运行中 null）；非零退出可附 `error`、ROOT 相对 `stdout` 与最多 2048 字符 `log_tail`。sender 附实际 `mode`、`pattern`，避免显示后台保存配置而非主机下发参数。
- `auto_adapter` 在控制连接为本机回环时精确选择 `\\Device\\NPF_Loopback`，仅用于本机诊断。回环测试使用 batch=1；产品默认 batch=69、MTU=9000 不变，本机证据不能代替双机验收。
- 控制台连接配置仅提供被采集机 IP、本机网卡和目标帧率；2026-10-29 按 owner 要求移除画面来源（含测试图案）、像素转换、发送批大小和 MTU 控件。保存时保留后台已有的隐藏参数，配置 API 与默认值不变。

## P6 配置、预算与显式会话（D19；P6-2，2026-11-03）

上述 2026-10-29 的精简 UI 已被本轮新增配置要求扩展；仍隐藏测试源/GPU/batch/MTU。
新增五档分辨率预设 + 自定义、crop/fit/fill/stretch、raw/h264/hevc、码率及预留余量。
开发版 Raw 可变尺寸与四种变换已接通；H.264/H.265 默认草案，明确启用开发功能、选择并协商两端后端后可运行。完整追加API/配置/descriptor/日志契约见 `encoded-stream.md`，双机与画质验收未完成。

| 方法 | 路径 | 请求 | 返回 |
|---|---|---|---|
| GET | `/stream/capabilities` | — | `{revision:2,presets[],native,hardware,adapter,adapter_error?}`；capture 按 HTTP 来源路由选择网卡并附数值收发速率；新原生程序返回当前角色MFT候选，另一侧unprobed；候选不代表实帧支持，不启动采集 |
| POST | `/stream/plan` | `{config:{...部分配置...},refresh:false}` | 200 `{revision:2,video,network,implementation,can_start,warnings[]}`；校验失败400，capture角色403；不写配置、不启停、不注册。详情、预算数学与缓存见 stream-config.md |

产品主机启动额外做新鲜链路预算；已知超线速、断开网卡、MTU 不一致在注册前拒绝，
速率未知只警告，但旧后台缺显式会话能力必须拒绝。低层接口支持成对 width/height、fps、source_mac；sender支持transform，显式新字段先探测原生能力再停旧进程。
Raw码率参数、不支持codec、收端缩放及错误几何拒绝。主机生成descriptor、收端先起、capture逐键确认并回显，不一致回滚双端。
两端stats追加 width/height/frame_bytes/fps_limit/codec，sender另有transform/source_skipped。28B包头与历史raw判据未变；SHM已升级v3，下游须重新打开，完整契约见stream-config.md。

P6-3追加只读 `POST /stream/codec-check {capture:{...}}`，实际激活/媒体类型协商，仍frames_tested=0；失败409。编码plan含backends清单并核对双端codec_revision=1；必须显式开发授权和后端，校验在停旧槽/注册之前。GUI区分压缩字节完整性与解码像素hash，详情以encoded-stream.md为准。

## /exec policy (added 0.2.0, allowlist-bound, NOT a general shell)

- `python` only runs script files under agent ROOT (no `-c`); frozen exe falls back to PATH python.
- `pwsh/powershell` only as `-NoProfile -ExecutionPolicy Bypass -File <script-under-ROOT/scripts> [args]` (no `-Command`).
- Bare names resolve under ROOT/`build`/`scripts` only; absolute paths outside ROOT denied; `..` denied.
- Canned diagnostics: `ipconfig [/all]`, `ping` (max 8 args, `-t` denied).
- `timeout_s` default 30, max 300; stdout/stderr truncated at 128KB each; cwd=ROOT; shell never used.
- Every call appended to `run/exec.log` (ts/client/argv/exit/duration; outputs stay inline-only).
- Server is threaded (`ThreadingHTTPServer`), so a running /exec does not block /health|stats.

## /update + /activate channel (P3，自 0.4.0；detached-helper 已删除）

- `POST /update` 只做 stage：raw zip bytes（`Content-Type: application/zip`，max 100MB）
  校验 → 解压到 `staging/mirror-<ver>` → 全树快照当前文件到 `prev/mirror` → 应答 `{staged:true, version}`，不重启、不碰 live 文件。
- `POST /activate {version}`（缺省用已 stage 版本）或 `{rollback:true}`（用 prev）：拷贝适用条目到 live 位 →
  应答 `{activated, version, rollback, restart}`（`restart=execv`）→ 原地 `os.execv` 接管（同 argv；冻结模式切 `version.json` exe 指针到版本化 exe 并 GC 旧版）。
- 交接期 `health/version` 短暂不可达，轮询确认；`run/updates.log` 记录每次 stage/apply/rollback。
- Zip allowlist：顶层 `vc-agent.exe|vc-agent.py|vc-sender.exe|vc-receiver.exe|vc-vcam.dll|version.json` 加 `scripts/*.ps1`；`version.json` 必带；zip-slip（`..`/absolute）400。
- stage 包不含自更新条目时（只换 sender/receiver exe）直接 live 应用并返回 `{updated, version, restart:none}`（先停 sender/receiver 会话）。
- 自更新条目（`vc-agent.exe|vc-agent.py`）若落不到 ROOT 下，整包 400 拒绝（不静默跳过）；冻结模式 ROOT 默认为 exe 所在目录，生产布局不受影响。
- 回滚用 `prev/mirror` 走同一 activate 路径；空备份 → 409；stage 版本不符 → 409；staged agent.py 编译失败 → 500；中途失败一律 500 JSON（不断连接），轮询 `/version` 为准。
- 调用方：`scripts/update.ps1` 与 `ctrl/run_remote.py --update/--rollback` 做 stage→activate→poll `/version`；
  `health.updater` 非 ps-ok 拒绝（该处走 install-pc2.ps1）。同版本重复 stage 保留更旧的 prev 备份。
- Windows 下 CPython execv 表现为子进程接管 + 父进程退出（非同 PID）；以 version 翻转 + 单监听 + updates.log 链为准，不以 PID 不变论。同版本冻结重装时目标即运行中镜像，apply 自动改用时间戳唯一文件名并更新指针（老文件下次启动 GC）。


