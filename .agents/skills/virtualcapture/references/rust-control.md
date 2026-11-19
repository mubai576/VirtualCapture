# Rust 产品路径（D26–D28，独立开发分支）

本文件只约束 `rust/`。旧 C++/Python 产品契约不变；允许重新设计 Rust 控制 API 不等于解冻 28B wire。
Rust 产品宿主通过版本化 `vc_stream.dll` C ABI v1 在进程内运行采集/接收，不再拉起
`vc-sender.exe`/`vc-receiver.exe`；独立 `vc-stream.exe` 诊断入口与 DLL 复用同一 crate 实现。
DShow 使用 Rust `vc_vcam.dll`；`vc-vcam-verify.exe` 只作隔离只读能力探测。旧 C++ 二进制不进入
Rust 发布包，也没有运行回退。Rust-R3 仍未验收，边界见文末。

## 拓扑与构建

- 被采集机：`vc-capture` CLI，默认 `0.0.0.0:5143`，无 GUI；由 `vc-core` 进程内加载 `vc_stream.dll` 启动 sender。
- 主机：`vc-host` GPUI 原生客户端，直接调用 `vc-core::host::HostController`，没有本机 REST 服务；
  本机 receiver 与虚拟卡管理同样使用 Rust 模块。
- `vc-core` 不依赖 GPUI，拥有配置/预算/会话/数据面模块/虚拟卡管理与主机编排。产品数据面不再是
  sender/receiver CLI 子进程；`vc-stream` 的 cdylib 导出 C ABI v1（`vc_module_get_api`），
  start/status/stop/destroy/capabilities，start 异步排队；stop/destroy 仅返回 0 才确认已释放，1 表示 pending 可重试。
  destroy 的其他返回码表示句柄状态未知，不得重试或卸载 DLL，保留到进程退出。Rust ABI 不导出。`vc-core` 自身的 C ABI SDK 外观仍未实现，与 DShow DLL 用途不同。
- `vc-source`（pattern/DDA/WGC 与四种整数变换）和 `vc-media`（Raw/MJPEG/PyroWave，动态加载固定
  TurboJPEG/PyroWave DLL）被 `vc-stream` DLL/CLI 与产品共用；DLL 与独立 EXE 共享同一实现。
- `vc-vcam` 是 Rust COM/DShow source filter `vc_vcam.dll`，复用兼容 CLSID/注册表语义并读取 SHM v3；
  注册前由兼容工具 `vc-vcam-verify.exe --dll <dll> --capabilities` 隔离子进程探测，不注册 COM。
- `cargo build --manifest-path rust/Cargo.toml --locked`；依赖锁在 `rust/Cargo.lock`。
  Windows 产物含 `vc_stream.dll`/`vc_vcam.dll` 与 `vc-host`/`vc-capture`/`vc-updater`/`vc-stream`/
  `vc-transport`/`vc-launcher`/`vc-vcam-diag` EXE。
- 两个入口都支持 `--root <runtime>`、`--native-dir <native-output>`。`VC_ROOT` / `VC_NATIVE_DIR` 也可指定。
  `--root` 优先；相对根自动沿 exe 向上找 AGENTS/version.json，便携布局回退 exe 目录。
  数据面模块按显式目录 → Managed 当前 release → build → ROOT → exe 目录解析 `vc_stream.dll`；
  不存在时明确失败，不搜索或回退旧 sender/receiver。
- 独立便携ZIP由 `python -I rust/packaging/package.py --allow-dirty` 构建，唯一dist目录；载荷以
  `rust/packaging/inventory.py` 为准：Rust EXE、`vc_stream.dll`、`vc_vcam.dll`、固定
  `turbojpeg.dll`/`libpyrowave-shared-1.dll`、`vc-vcam-verify.exe`、app-local MSVC DLL、许可与
  SHA-256 清单；不含Npcap驱动，也不含旧C++产品（出现即拒绝）。原生运行库由
  `rust/packaging/native/CMakeLists.txt` 单独构建，根 CMake 旧产品不变。
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

## REST v2

普通控制API仅用于受控直连网络；更新接口要求独立Bearer令牌。没有 `/exec`、`/files`、浏览器 `/ui`。
主机通过 `implementation=virtualcapture-rust`、`api_revision=2`、`role=capture`、`ok=true` 验证身份，
拒绝旧 Python 后台；HTTP 失败保留具体错误体。普通请求总超时5s，网卡枚举/prepare/start为40s，
容纳有界的原生枚举10s、PowerShell元数据15s和能力探测；超时仍按会话清理。

| 方法 | 路径 | 请求 / 返回 |
|---|---|---|
| GET | `/health` | `ok,implementation,api_revision,version,role,running,pid,root,session_id,build,layout`；pid 是后台 pid，build为active标识或null，不含更新令牌 |
| GET | `/version` | Rust crate version + implementation/api_revision；不以旧 version.json 充当 Rust 版本 |
| GET/POST | `/config` | GET 完整 Config；POST 部分对象合并校验，失败不写，运行中409；成功 `{saved,config}` |
| GET | `/adapters?refresh=true` | `{items:[AdapterInfo...]}`，refresh 强制重新探测；普通查询缓存10s |
| POST | `/sender/prepare` | SenderRequest → PreparedSession；校验实际模块能力、路由、网卡、几何/编码参数和预算，不启停/写配置；Raw 的 `batch_packets=ceil(NV12 frame bytes/(MTU−42))`；编码为 null，由数据面在编码帧完成后按实际AU包数整帧提交 |
| POST | `/sender/start` | 同请求/响应；已有活动 sender 或同会话运行证据时409，拒绝覆盖；启动存活检查350ms不代表首帧验收 |
| POST | `/sender/stop` | `{session_id}`；匹配后停止，错误会话不得停当前活动进程；停止后可幂等重复 |
| GET | `/sender/stats` | 数据面 stats 合并 session_id/running/adapter/pid/exit_code；异常含 error/log_tail |

SenderRequest 全部字段：

```json
{"capture":{"mode":"pattern","pattern":"motion","gpu_convert":false,"fps":60,"frames":120,
"mtu":9000,"width":416,"height":416,"transform":"crop","codec":"raw",
"dst_mac":"ff:ff:ff:ff:ff:ff"},
"session_id":"s-example","source_mac":"02:01:02:03:04:05","auto_adapter":true,"adapter":""}
```

capture 其他字段采用 Config 默认；主机实际下发完整 capture 并逐字段比对响应，不凭省略值确认。
PreparedSession 为 `{capture,batch_packets,session_id,source_mac,adapter:AdapterInfo}`。`capture.batch` 已删除；Raw 物理网卡 batch 按准确尺寸/MTU计算为一整帧并一次提交，编码按实际AU整帧单次queue。Npcap Loopback 只用于显式本机诊断，因其不支持大 sendqueue，Raw 固定 batch1 并按包提交；该例外不用于产品性能或双机结论。该控制契约不兼容 API revision 1；旧C++ sender CLI 的 `--batch` 契约不变，二者不混用。
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

- Raw 是默认；MJPEG/PyroWave 必须 `codec_experimental=true` 且两端后端精确为 `turbojpeg:cpu` /
  `pyrowave:vulkan`，否则启动拒绝，不静默回退/换后端/降尺寸/降帧率。
- 数据面模块能力必须声明 `stream_revision=2`、`variable_resolution=true`、`shm_version=3`、
  `sendqueue=frame`、`codecs=[raw,mjpeg,pyrowave]`、`sources=[pattern,dda,wgc]`；实际选择的
  codec/source 未声明时拒绝启动。独立 `vc-stream.exe` 诊断 CLI 仍是 Raw pattern 诊断；DDA/WGC
  与编码经同一 crate 的 DLL ABI 在进程内运行。
- Raw 由 `vc-core` 按准确几何与 MTU 计算 `batch_packets`，prepare 回显，start 就绪回显必须一致；
  DLL 每次 sendqueue 提交一整帧。编码不发送固定 batch，DLL 按实际编码AU包数整帧提交并在帧边界
  flush。Npcap Loopback 诊断每包提交（batch1），不得用于产品性能/双机结论。旧C++ CLI 契约不变。
- PyroWave 在 sender/receiver 宣布 ready 前，用同几何、同后端、同帧预算的合成 NV12 显式预热；
  接收端用临时编码器生成合法 AU，再让正式解码器真实解码，输出丢弃。冷启动预热耗时另记，
  不进入正式帧号/hashlog/SHM/帧率统计。创建/预热/编码/解码/重置/销毁各受5秒同步看门狗覆盖；超时只把该会话 `OperationState` 置为
  粘性超时并让后续原生调用失败，不 `process::exit`、不强制展开或 join 挂起线程。父层保留 worker、
  句柄与 DLL；`cleanup_pending` 期间配置、OTA 与虚拟卡操作锁定；不能安全停止时不得自动卸载
  （有意保留到进程退出），停止未确认不得当作已释放。
- 自动注册虚拟卡前，通过 `vc-vcam-verify.exe --dll <实际DLL> --capabilities` 隔离探测相同能力，
  不只相信文件存在；工具查找 native-dir/普通原生路径、DLL所在构建目录的兄弟vcam_verify/Release，
  及 ROOT/build/vcam_verify/{Release,Debug}。
- 预算沿用 `stream-config.md`；超过物理速度拒绝，侵入预留余量为 margin，未知速率警告。
  非法几何/fps/MTU、单帧65535分片上限、两端MTU不一致等均为硬错误。

开始：验证 Rust 对端 → fresh 网卡/prepare/预算 → 注册本目录虚拟卡 → 进程内 receiver → 进程内 sender → 参数回显。
虚拟卡归属由匹配 DLL 路径、HKCU CLSID 的 `VirtualCaptureOwner` 全局记录与本根的一份有效
本地证明共同确定（schema 2：DLL/运行根/PID/进程创建时间必须一致）。镜像固定为
`ROOT/data/run/vcam-owned.json`（current）与 `ROOT/data/run/vcam-owned.pending.json`，不随
Legacy/Managed 布局或 OTA 激活/回滚切换；查找兼容旧 `ROOT/run/vcam-owned.json`，不搬动旧 run 证据。
只选择与全局 owner 完全一致的记录，不因两端属主均已退出而接受不一致记录。Windows 路径比较
对普通 Win32 路径消解 `.`/`..` 与分隔符；现存路径 canonicalize，OTA 删除的旧 DLL 通过最深存在祖先 canonicalize 后拼接缺失后缀，以匹配 junction 等根路径别名。
只有 `NotFound` 可按缺失组件继续向上；访问错误、悬空重解析点及含 `/`、`.`、`..` 的歧义 verbatim 路径保持未证明并拒绝恢复，不退回词法相等。
支持盘符、UNC 与合法 `\\?\Volume{GUID}\` 文件系统路径；比较时仅折叠盘符/UNC 的 verbatim 前缀，卷 GUID 保留完整命名空间，不接受任意设备命名空间。若 Windows 将卷 GUID 路径规范化为已分配盘符，必须查询盘符根对应的卷 GUID，并确认重建后的完整路径一致；查询失败或路径不同均拒绝。
全局 owner 的 DLL/根路径必须仍是规范拼写，新记录写入 canonicalize 得到的运行根/DLL。
共享 `--native-dir` 下 DLL 路径不代表运行根相同。其他运行根、其他存活宿主或无法确定归属/进程存活/
注册表读取结果时均拒绝接管或注销；尚无 InprocServer32 的全局 owner 预约也不能被另一宿主当作空闲。
缺少全局 owner 值的旧注册不自动恢复。同运行根的本进程或确认已退出属主可恢复。
所有注册/恢复/注销的检查、owner 记录写入、regsvr 与结果复核使用按 CLSID 命名的跨运行根
Windows mutex 串行化；获取失败或超时先报错，不修改注册。持锁写入顺序：若有旧全局 owner，
先将其匹配证明保存为 current，再写入并刷盘新 pending，提交注册表，最后将 pending 提升为 current。
注册表写失败保留旧 current；提交后退出或最终 rename 失败则保留匹配新全局值的 pending。
下一次重试先保存该精确证明，不能覆盖唯一匹配记录后再尝试注册表写入。该日志保护进程中断恢复，
不承诺断电时文件系统/注册表的跨介质原子提交。只读 `check_available` 是预检，`register` 在持锁后
再次检查。开发验证不覆盖正在使用的部署。

停止/失败/正常窗口关闭：按本机 claim 的精确会话号停发端 → 停本机 receiver → 只回收本控制器注册的匹配 DLL。
HTTP start 超时也保留待清理会话，不能误当成没开始；stop 失败后仅用 stats 只读对账：确认 idle 或被另一
session 替代时取消旧 claim，不停止新 session；缺少/非布尔 `running`、缺少会话号或请求失败均保留未知状态。
显式 Stop 可先验证 Rust health，再发现当前精确 session 并停止；请求失败保留该目标供重试，每次重试重新
只读发现，不能把空/未知会话号发给 stop。自动清理/关窗不发现或停止非本机创建的远端会话。
`Snapshot.remote_pending` 覆盖本机 sender claim、已发现停止目标和显式 Stop 未确认状态；任一未确认时
保持 `cleanup_pending`，阻止配置/连接/OTA/虚拟卡操作。UI 只有确认不存在远端 claim 时才用 `CleanupLocal`
重试本机 receiver/虚拟卡回收；含远端 claim 时走显式 Stop。对端失联只保留精确目标，不以失联当作已停止。
NativeSlot 正常释放会 join DLL 工作线程，并读取最终 stats；停止未确认时保留会话与模块句柄，
不 Drop 活动句柄。PyroWave ready 等待上限30秒，其他会话10秒；冷启动与每步5秒原生看门狗分别计时。
ABI start 的输出文件预约或 worker spawn 失败只回滚本次新建的空文件，不覆盖已有证据。
强杀/断电不保证用户态清理。

## 验证边界

`python scripts/verify.py` 继续验证旧实现，不能代替 Rust tests。
`cargo test --manifest-path rust/Cargo.toml --workspace` 验证 Rust 契约。
`python -I rust/tests/dll_stream_smoke.py --package <解压包>` 在同一进程用 ctypes 加载真实
`vc_stream.dll`，覆盖 C ABI v1 布局/版本、start/status/stop/destroy、Raw 与按 `--codec` 显式选择的
MJPEG/PyroWave hash 及 SHM v3；不启动 sender/receiver/vc-stream/vc-vcam-verify 子进程，
默认只用 Npcap Loopback。它证明本机模块功能，
不是物理网卡、吞吐、延迟或双机验收。三编码 Release DLL 的本机 Loopback 240fps 数据和明确范围见[Rust-R3 bring-up](../../../../docs/bringup/bringup-20261119-rust-r3-local.md)。
独立 `vc-stream.exe` 仍是不接产品控制 API 的 Raw pattern 诊断 CLI（DLL 与 CLI 共用实现）；其
Loopback 冒烟和性能证据见[数据面 bring-up](../../../../docs/bringup/bringup-20261119-rust-data-plane.md)，
不冒充产品/双机结论。
`python -I rust/tests/control_smoke.py` 以 Rust `vc-capture` REST 控制面 + 同一进程内
`vc_stream.dll` 接收端运行（不启动 sender/receiver 子进程），不由 Rust 打包默认执行
（`--extended-smokes` 才追加）；它在随机回环端口、独立ROOT、私有SHM跑 Raw motion/batch1
全量哈希对账，另查错误会话停止、运行中改配置、复用会话证据保留与 DLL stop/destroy 生命周期，
不注册DShow、不碰稳定5143。`rust/tests/gui_smoke.py` 打开真实GPUI窗口、
输入随机回环端口并点击连接，要求本机错误桩收到 `/health` 并返回503，保存初始/错误截图，正常关闭退出0；
每次使用独立目录。
本机回环不代替双机/240fps/长稳验收。`vc-transport` 是冻结 wire/分片/校验/有界重组的共享库；
Rust 产品数据面已由 `vc_stream.dll` 进程内承载，`vc-source`/`vc-media`/`vc-vcam` 实现已落地，但
物理NIC与双机性能、长稳、编码画质、完整 DShow 编排仍未完成；当前主机无 DXGI 输出，DDA实采未验证；WGC窗口实采与隔离vcam DLL场景通过但不等于完整产品编排。GUI非空白截图门失败，因此便携完整验收未通过。`vc-core` 的 C ABI SDK 外观未实现。OTA 按本节独立验证，不继承旧通道验收。
