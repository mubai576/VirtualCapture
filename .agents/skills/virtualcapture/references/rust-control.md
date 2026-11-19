# Rust 产品与控制契约

产品仅Rust，不保留旧C++/Python运行回退。控制API重设计不解冻28B wire。当前开发验收状态见PLAN，默认Raw，显式开发编码仅PyroWave。

## 拓扑、入口与目录

- 被采集机 `vc-capture` 为REST v2 CLI，直接运行默认 `0.0.0.0:5143`；便携 `Start-Capture.cmd` 无参数默认15143。主机省略端口仍5143，必须填写实际 `IPv4:端口`。
- 主机 `vc-host` 为GPUI原生客户端，直接调用UI无关的 `vc-core::host::HostController`，没有本机REST或浏览器UI。
- 宿主通过 `vc_module_get_api` 加载 `vc_stream.dll` C ABI v1，list_adapters/start/status/stop/destroy/capabilities；start异步排队。DLL与独立诊断 `vc-stream.exe` 共用vc-source/vc-media/vc-transport实现，不拉起旧sender/receiver。
- DShow为Rust `vc_vcam.dll`；隔离工具 `vc-vcam-verify.exe` 也由Cargo构建，既支持只读能力探测，也支持实际COM graph验证，不注册、不回退注册枚举。vc-core自身的通用C ABI SDK外观尚未实现。
- 产品唯一workspace/锁为 `rust/Cargo.toml` / `rust/Cargo.lock`，构建/门禁/唯一完整包见 `docs/runbooks/build.md`。CMake仅构建固定PyroWave第三方库，旧根CMake已移除。
- 两入口接受 `--root` / `--native-dir` 与 `VC_ROOT` / `VC_NATIVE_DIR`，CLI优先。显式native目录缺文件即失败；通常按显式目录→Managed当前release→build→ROOT→exe目录查找Rust模块，不补旧产品。源码运行应显式指向Rust产物目录。
- 日常GUI入口 `VirtualCapture.exe`（Cargo产物vc-launcher）为Windows GUI子系统+图标，直接解析current并启动GPUI，不用CMD/PowerShell。回执/日志写data/run/host-*，无令牌；错误用原生消息框。采集控制台保留供Ctrl+C退出。
- ROOT固定。有 `app/current.json` 时：`app/releases/<build_id>/`程序、`data/vc-rust-config.json`配置、`data/run/`证据、`updates/{staged,worker,incoming}/`与history.jsonl。无current保留扁平配置/run，首次激活只复制配置/凭据，不搬动旧证据；回滚指针恢复旧布局。
- 不读取/迁移旧Python `vc-config.json`。Rust配置缺失才用默认；语法/版本错误明确失败，保存临时文件刷盘后原子替换。入口不自动部署/接管旧服务/注册真实虚拟卡。

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

SenderRequest示例（实际主机下发完整capture并逐字段确认，不凭缺省值确认）：

```json
{"capture":{"mode":"pattern","pattern":"motion","gpu_convert":false,"fps":60,"frames":120,
"mtu":9000,"mtu_auto":false,"width":416,"height":416,"transform":"crop","codec":"raw",
"dst_mac":"ff:ff:ff:ff:ff:ff"},"session_id":"s-example","source_mac":"02:01:02:03:04:05",
"auto_adapter":true,"adapter":""}
```

PreparedSession为 `{capture,batch_packets,session_id,source_mac,adapter:AdapterInfo}`；capture.batch不再是配置项。会话号3..80 ASCII字母/数字/连字符，不是路径；源MAC须本地管理单播，仅隔离流，不是认证，不改系统MAC。

## 网卡与MTU

- AdapterInfo有 `mtu: u32|null`（IPv4 NlMtu），旧对端缺字段可反序列化但不能据此证明MTU。查询只读Get-NetAdapter/Get-NetIPInterface，不Set、不加14、不改Jumbo。
- 产品只接受Windows明确 `hardware_interface=true`、`virtual=false` 且 `Status=Up` 的网卡，不把元数据未知当物理卡或已连接。列表/选择/prepare/start一致，不能手填隐藏设备绕过。保存的显式选择保持，失效后清除并要求重选。
- 不按厂商或型号偏置。采集端及本机主机按控制连接的出站IPv4精确匹配自动选择；对端仅在唯一已连接物理网卡时自动预览，多候选要求明确选择。明确清空/失效选择不反复自动替换。prepare后start固定精确网卡名，不重新静默选卡。
- 新Capture默认 `mtu_auto=true`；旧配置缺字段反序列化为false，保留旧显式mtu。自动值为 `min(本机接收NIC NlMtu, 被采集机发送NIC NlMtu, 9000)`，双端需先确定；缺报告、低于1500或Loopback均拒绝自动解析。无法预选被采集机网卡时须明确选择或使用显式MTU。
- 手动MTU范围1500..9000，不得超过实际网卡报告；物理卡未知报告同样拒绝，不能靠手动模式猜值。GUI显示双端报告值、生效值及自动/显式状态。
- 主机产生有效配置副本，冻结mtu并令 `mtu_auto=false`，镜像receive.mtu；保存草稿不变。预算/prepare/start/DLL批大小共用此值；响应MTU、mtu_auto、全部capture和网卡须精确确认；采集端按实际卡再次复核。
- `vc-capture --list-adapters`仍给完整诊断枚举；仅显式 `--allow-loopback-diagnostics` 且控制器为本机回环才允许Npcap Loopback。诊断用显式MTU、Raw batch1，不冒充物理网卡容量或产品性能。

## 几何、变换与预算

默认DDA/硬件GPU/640²@240/Raw/crop；偶数宽高2..8192、fps整数1..1000、显示器output0..15。范围是软件校验，不是吞吐认证。WGC可指定≤128字符单行窗口子串；窗口关闭/身份失效即终止，不换替代窗口或DDA，resize重建帧池但不改会话输出。无DXGI输出是DDA BLOCKED，不能换源补PASS。

- crop：居中1:1，源不能小于输出；stretch：整幅映射；fit：等比缩小活动范围、偶数尺寸/偏移、黑边Y0/UV128；fill：居中偶数裁切后铺满。
- 全部为整数最近邻及整数BT.601；真相源vc-source/transform.rs与desktop/gpu.rs，GPU逐字节对照独立整数参考。UNORM解码例外不授权浮点图案生成。
- DDA/WGC强制gpu_convert=true且只用硬件D3D11；pattern须false，是显式合成诊断。旧配置只改mode=pattern却遗留gpu_convert=true时启动明确拒绝，不静默修改。
- fps是发送上限，不保证源/设备达到；无桌面更新不重放旧图制造FPS。帧开始发送间隔至少ceil(1e9/fps)ns，不补发追赶。所有QPC分段只在同机比较。

Raw预算：`P=mtu−42; B=w*h*3/2; N=ceil(B/P); last=B−(N−1)*P; wire_frame_bytes=B+N*66+max(0,18−last)`；66含以太头/VC头/FCS/preamble/IFG。`wire_bps=wire_frame_bytes*8*fps`；`usable=min(host_rx,capture_tx)*(100−headroom_pct)/100`，预留默认20%。不含VLAN/其他业务，也不是实测。

两端速率都已知才称fits；超过usable但未超物理为margin，超任一已知物理速度为exceeds，明确断开为down，未知只警告；exceeds/down拒绝。PyroWave按最大码率估算分包，is_estimate=true，不能证明吞吐/画质。非法几何、MTU不一致及>65535分片等均硬错误。

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

## 能力、批提交与生命周期

- Raw默认不压缩；MJPEG始终明确拒绝，旧字段仅用于兼容解析/错误提示。PyroWave必须实验授权、精确pyrowave:vulkan、GOP1、实际集成/独立GPU Vulkan≥1.3，CPU/虚拟/未知设备拒绝，无后端/源/尺寸/fps静默降级。
- 本版DLL/包声明 `stream_revision=2, variable_resolution=true, shm_version=3, pixel_format=nv12, sendqueue=frame, codecs=[raw,pyrowave], sources=[pattern,dda,wgc]`。产品还逐项检查实际选中能力；旧DLL多报MJPEG不授予启动许可。
- Raw物理网卡batch=ceil(B/(MTU−42))，一整帧一个sendqueue；编码按实际AU整帧单次提交/边界flush，不配置固定batch。零/短发失败，无batch降级。只有显式Loopback诊断为每包提交/batch1。
- PyroWave正式ready前以同几何/后端/帧预算合成NV12预热，接收端临时编码合法AU给正式解码器；不进入正式帧号/hashlog/SHM/FPS。每步5秒粘性看门狗不退出宿主、不展开/join挂起调用。
- stop/destroy仅0确认释放，1表示pending可重试；destroy其他码为状态未知，不盲重试/卸载。pending保留worker/句柄/DLL，并锁配置、OTA与虚拟卡操作；不安全对象有意留至进程退出。
- 真实注册前必须由 `native_exe("vc-vcam-verify.exe") --dll <实际DLL> --capabilities` 隔离能力探测；只用Cargo工具/正常Rust载荷路径，没有旧C++兄弟目录回退。SHM/动态格式/D12细节见docs/spec/shm-v3.md与runbooks/vcam.md。

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

`scripts/verify.py`现为Rust总闸：fmt/workspace tests/全target Clippy/Release/独立Python图案和wire向量/DLL self-check，默认不采集/收发/注册。专项脚本与前提见docs/runbooks/rust.md。

DLL Raw/PyroWave Loopback、私有WGC GPU窗口、Rust隔离COM、REST随机端口/独立ROOT与GUI均分项验收，互不补PASS。GUI必须真实交互且截图非空白，黑屏硬失败。旧三编码报告只代表当时版本；当前CPU MJPEG已移除。

旧双机自动T1–T5编排/完整取证工具已移除，Rust等价工具尚未实现；REST没有/files或receiver/report，不可声称自动完整保全双端hashlog。物理NIC/双机/长稳/画质/完整DShow与完整包/OTA仍须专项；缺测/BLOCKED/失败不算通过。历史报告不回填新结果，不自动部署或注册。
