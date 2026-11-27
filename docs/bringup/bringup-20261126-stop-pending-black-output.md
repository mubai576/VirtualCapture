# 远端停止未确认与黑屏现场诊断（2026-11-26）

## 状态与边界

**现场故障未关闭。** 用户反馈“无法停止远端采集，画面全黑，未见视频流量”。本轮已保全本机接收证据、核对停止链路、修正目标/实际帧率混用及待清理状态展示，并完成本机隔离回归。尚未获得远端发送日志、线程栈或现场像素，不能把 GPU、Npcap 或下游格式假设写成根因，也不把本机测试写成双机或画质 PASS。

初始本机证据采集阶段没有请求对端 REST、操作 PC2、点击现用会话开始/停止、终止进程、真实注册/注销虚拟卡、修改物理网卡或部署；未读取旧更新令牌，未修改 `staging/`、`prev/`。后续只读 HTTP 查询另见本记录“只读现场接口快照”。当前会话的新注册不属于 D37 已清理的旧注册，停止未确认时不得再次删除。

## 本机现场证据

- 运行主机：PID `6504`，现场版本 `20261126-002714-cbcb20`，Managed ROOT 为 `D:\Share\VirtualCapture`；对应 EXE、数据面与虚拟卡 DLL 已与 dist 核对。
- 会话：`s-26bc9b1be54e2f58`，DDA、Raw、crop、416×416、目标 240 fps；本机接收网卡 `\Device\NPF_{7406EA46-5837-40C4-8598-D3B5063E5684}`，生效 MTU 1500（双端只读报告值取小，非保存输入 9000）。
- 原始接收文件保留在 `D:\Share\VirtualCapture\data\run\`；副本与截图在 `run/stream-incident-20261126-011016/`，`evidence-inventory.json` 记录逐文件 SHA-256 与长度。未删除或覆盖原文件。
- 接收 `complete=361`、`hash_rows=361`、`shm_published=361`，帧号 0…360 连续；`packets_recv=64619`、`packets_lost=0`、`crc_errors=0`、`hash_mismatch=0`、`decode_errors=0`，最终 `failed=false`、`error=null`、`cancelled=true`。
- 每帧 259584 B，MTU 1500 的应用 payload 1458 B，ceil(259584/1458)=179 包/帧；361×179=64619，与完成帧及报包数相符。不能据此声称源像素画质正确。
- 累计 `bytes_recv=96423822`，最终 `fps_actual=2.7805131129013807`、`uptime_s=129.8321516`；不是从启动起一包都没收到，也没有达到目标 240 fps。此值是全会话平均，不是稳态/瞬时性能验收。

接收日志（墙钟换算 UTC+08:00）：

| 时间 | 事件 |
|---|---|
| 00:59:39.302 | 启动本机 receiver |
| 00:59:39.328 | 数据面就绪，进程内 PID 6504 |
| 约 01:00:08.875 | 最后一个完成帧（本机 QPC 与本机墙钟相关换算） |
| 01:01:49.144 | 请求停止本机 receiver |
| 01:01:49.169 | 本机 receiver 已停止并完成回收，约 25 ms |

最后帧的本机 `last_completed_qpc_ns=12758683997100`，首帧 `12731007632200`，首末 span=27.6763649 s；帧 0→1 的 20.7214924 s 长间隔包含在 span 内，不能重复相加。最后帧到停止请求约 100.3 s 没有新的完成帧。DDA 静态桌面无新帧、循环中的原生调用阻塞、停止时 teardown 阻塞均未排除，不能从停流时刻断言挂起起点。

Hashlog 有 38 种 hash，未与已检查的纯黑/全零基准相等；**这不证明视觉非黑**。原始像素未记录，接收已回收后只读打开 `Local\VirtualCaptureVcamFrame` 返回不存在（错误 2）；没有创建现用映射或补造像素证据。

## 停止未确认的定位

`host-events-focused.png` 显示 01:01:49、01:01:59、01:04:55（UI 旧事件时钟显示 UTC 的 17:xx）三次远端响应：

```text
/sender/stop 返回 HTTP 409:
停止未确认：数据面会话句柄已保留，未释放；请重试停止。
```

这是 `vc-capture/src/state.rs::Agent::stop` 的 Pending 分支，不是会话号不符或没有发出请求。`vc-core/src/process.rs::NativeSlot::stop` 先设停止请求并在两秒宽限内每 25 ms 重试；`vc-stream/src/abi.rs::Session::stop` 在 worker 的 `JoinHandle::is_finished()` 为 false 时返回 pending。控制 API 能继续返回 409，说明控制面仍响应，但这些尝试中数据面线程没有结束。

按本次 Raw 路径，Npcap `pcap_sendqueue_transmit`、D3D11 `Map(READ)` 以及 pcap/D3D 资源 Drop 都可能进入不能由停止标志中断的原生调用。现有证据不能选择其中一个，也不能把最后帧后无流量全部归因于挂起。需要发送端完整日志/状态与线程栈才能进一步区分；本轮未操作远端。

主机按精确 session_id 停远端→停本机 receiver→回收自有虚拟卡。远端 pending 时仍保留 claim、DLL/归属与配置/OTA 锁，是安全契约，不以清注册、强卸载或伪造 stopped 绕过。当前停止故障未由 UI 修正解决。

## 黑屏与格式时序的隔离检查

当前 HKCU64 `InprocServer32` 指向新 release 的 `vc_vcam.dll`，`FrameMap=Local\VirtualCaptureVcamFrame`、`Motion=0`，owner 为当前 PID 6504；不是 D37 的失效旧路径。只读模块观察发现本机下游进程已加载这份新 DLL，但未读取或修改其业务状态，不能据此确定它的 graph 格式或创建顺序。

虚拟卡 filter 在创建时固定媒体类型：有效 SHM 的几何优先，没有 SHM 时默认 640×640@240；旧实例不动态换档。worker 拒绝几何不一致的映射；从无好帧不输出（D12）。因此应先开始接收、确认完成帧增长，再在下游新建/重开设备；改尺寸也需重开。主机深色画幅框是布局示意，非实时预览。

使用已打包 DLL 的隔离结果（全部为私有映射、直接 DLL COM，不注册、不用现用 SHM、不接触网卡或远端）：

| 项目 | 证据 | 结果/限制 |
|---|---|---|
| 无映射与 416×416@60 | `run/rust-vcam-dll-smoke-513cfcdafa4f/report.json` | 无数据 0 样本；416@60 交付 71 样本；不代表现场 240 fps |
| 416×416@240 | `run/rust-vcam-dll-smoke-ce67c8d8e8c3/report.json` | 517 样本、8 个发布帧全部按序/哈希相等、其余重复最后好帧；repeat gap p90=5.005 ms。合成源每 50 ms 发布，不是 240 fps 全链路吞吐验收 |
| 打开顺序 | `run/vcam-format-order-9dd06f923801/report.json` | 先开 filter 得 640×640、614400 B；创建 416 映射后同实例仍 640；新建 filter 得 416×416、259584 B，均 240 fps。复现格式固定行为，不证明现场根因 |

测试 DLL SHA-256：`efa9fd850ee6850f5fc51ff4506de6c3509ebe64ff744ce4b0cea997433d4543`。

## 本轮修正与回归

- `host_ui.rs` 的接收帧率主读数和明细原先读取/优先读取 `fps`，会把目标 240 显示为实际帧率；现只读 `fps_actual`，缺失显示未知，不用目标回填。
- `running=false && cleanup_pending=true` 时状态改为“待清理”，增加优先于普通预算/断连警告的本机/远端待停止提示，完整 HTTP 响应继续保留在最近活动；不改变停止协议、资源归属或锁。
- 新增 actual_fps 与待清理提示单测、主机连续三次 409 保留精确 claim/虚拟卡单测、ABI 阻塞 worker 连续 stop/destroy 返回 pending 并在解除阻塞后才回收的单测。
- `run/stream-regression-20261126-013613/`：vc-core 164、vc-stream lib 31、vc-host 3，共 198 项单测通过。最终待清理文案格式化后的 `run/stream-final-check-20261126-014637/report.json` 记录 fmt check、vc-host 3 项与三个修改 crate 全 target Clippy（`-D warnings`）均为 exit 0；`git diff --check` 通过。

D38 完成时未提交、打包或部署；现用主机不会因源码修改自动改变。后续 D39 的实际修改、回归与失败证据见下一节；这些结果不回填 D38 的历史通过结论。

## 实际修复迭代（D39）

用户进一步要求“修复好他，就像我预期的一样”。本轮目标是实际出图、传输与可靠停止，不以提示修改关闭故障。**当前仍未完成现场修复验收**；没有部署、操作 PC2、真实注册/注销或强制释放，现用会话不受本机隔离测试影响。

### 源码修正

- `vc-source/desktop/gpu.rs`：原 staging `Map(READ, 0)` 会阻塞等待 GPU；改为提交后 `Map(READ, DO_NOT_WAIT)`，仅对 `DXGI_ERROR_WAS_STILL_DRAWING` 重试，Y/UV 共用 1 秒绝对期限。每次重试检查本会话停止标志，取消的半成品不发送；超时/设备错误致命，不换 CPU/WARP/源。任意驱动调用、`Flush`、资源 Drop 仍可能不返回，此期限不授权强制展开或卸载。
- `desktop/mod.rs`、DDA/WGC 与 sender：传递同会话停止标志；DDA 仍用约 20 ms 有限取帧等待，不重放静态帧。DDA `capture_diagnostics` 增加取帧尝试/超时/取得/转换计数，区分没取得源图与取得后未发送。
- `desktop/wgc.rs`：FreeThreaded 帧池使用同一 D3D11 设备前启用并确认即时上下文的 `ID3D11Multithread` 保护。此项与回读修改是实际代码修正，不代表已证明它们是现场卡点。
- `vc-core/host.rs`：保留只读归属/能力预检，但将真实注册移到 receiver 创建 SHM 并确认几何之后，再启动远端 sender。失败走原精确回收；已打开的旧 graph 仍不自动改变固定格式。
- sender 等新帧、receiver 等包/忽略包时也周期更新实际计数和运行分母；源/网络空闲不再冻结最后一次 `fps_actual`。主机自动清理前保留自身精确会话的终结错误，避免 GPU/Npcap 错误被清成普通待开始。
- `source_dll_smoke.py`：增加 DDA/WGC 无限会话至少发/收 10 帧后的停止回归；stop+destroy 共用 2 秒绝对期限，迟到的返回 0 也失败。销毁成功后立即标记句柄失效，失败路径保全部分组合/请求/fixture/逐会话清理，不重试死句柄。fixture 在居中 crop 内加入 16 级灰阶，保证原有 >=8 种 Y 值非空白门覆盖，不下调门限。
- 新增单测覆盖回读重试/取消/超时、GPU 与整数 CPU 参考逐字节相等、receiver→注册顺序、失败阶段和远端终结错误保留；连续 Pending 仍保留 worker/句柄/DLL/归属。

### 实际回归结果

以下均为本机、私有 ROOT/SHM、显式 Loopback/batch1 或隔离直接 DLL COM；不收发物理网卡，不注册真实虚拟卡。

| 项目 | 证据 | 结果与限制 |
|---|---|---|
| Rust 完整总门 | `run/stream-repair-20261126-final/workspace-verify.log` | fmt、22 组/344 项 Rust 测试、全 target Clippy `-D warnings`、Release、独立 Python 图案/wire 向量和 DLL self-check 全通过。Cargo 仍报告已有 bin/lib 同名 PDB 输出碰撞 warning；没有把它记作 Clippy 错误或无警告构建 |
| REST 精确会话 | `run/rust-smoke-bed69a0387/` | motion/416²@60/120 帧/MTU9000/Loopback/batch1，120/120、hash 相等；错会话停止、运行中改配置/替换、证据复用被拒，完成 receiver stop/join/destroy；不是活跃 DDA 停止 |
| Raw DLL | `run/rust-dll-stream-smoke-1f97d4d2d7e8/report.json` | motion/640²@60/120 帧/MTU9000/Loopback/batch1，全量独立参考与 TX/RX 哈希相等，Pending probe `[1,1,0]` 后才销毁；不是 240 fps 性能门 |
| WGC 真实动态窗口专项 | `run/wgcdiag-eadb053da6f4/report.json` | stretch 45/45；crop/resize 90/90，源从 482×392 变为 626×473、输出仍320×240@30；关闭窗口前9帧，关闭后真实 `WGC capture item closed` 错误并结束；均无替代源 |
| WGC 活跃停止 | 同上 `wgc-gpu-stop` | 320×240@30/Raw/crop/motion/MTU9000/Loopback/batch1，无限会话发/收17帧；TX stop+destroy=10.990 ms，RX=10.388 ms，cancelled=true/exit0，DLL仍加载，全部 hash/SHM 相等 |
| 实际源→DirectShow | `run/wgc-chain-89491edd8c78/report.json` | 最终DLL：WGC私有640²窗口→硬件GPU crop416²→Raw Loopback/batch1→私有SHM→直接DLL COM。目标240 fps，TX/RX271/271；DShow704样本、193种hash，全部匹配接收帧，invalid=0；TX stop+destroy21.110 ms、RX10.450 ms、DShow Stop/Disconnect0.181 ms。实际源会话平均79.71 fps，接收平均76.34 fps，不是240吞吐、物理/双机、现场下游或DDA通过。较早一轮268/268与703样本保留在 `run/wgc-chain-68c2c6f1c866/` |
| Native GPUI | `run/rust-gui-smoke-9f94eb40d6/` | 真实选端/持久化/重启、接收工作台、更新无配对输入、尺寸与网卡/连接错误/正常关闭通过，截图非黑。未驱动新 Pending banner 的真实挂起状态，无流/注册/现场启动 |
| source self-check | `python -I rust/tests/source_dll_smoke.py --self-check` | 纯 helper 通过，包括最后返回0但超2秒必须失败；不加载 DLL，不替代实采 |

完整 Release 重建后的 `vc_stream.dll` SHA-256 为 `fdc6d8dc68354408b8fce7206165fada71cf8c0628f76284a8a695775c40574a`，WGC最终专项、271帧私有416→DShow链路、DDA最终失败与 REST/Raw DLL 使用该构建。较早268帧私有416→DShow验证使用前一构建；逐文件来源/哈希见各 report，不把不同构建合成同一份证据。

### DDA 明确失败，未改判

- `run/rust-source-dll-smoke-3e1d8be28f6b/failure.json`：可见/topmost 动态私有窗口下，首个 `dda-gpu-stretch` 在35秒内仍0帧；TX cleanup stop `[1,1,1,0]`、destroy `[0]`，最终 cancelled=true/exit0。没有复现远端多次 Pending 不退出。
- 增加直接取帧计数后的 `run/rust-source-dll-smoke-74fcaa940a97/dda-gpu-stretch-tx-stats.json`：35.033 s，`acquire_attempts=1793`、`acquire_timeouts=1793`、`acquired_frames=converted_frames=0`、`last_present_ticks=0`。卡点是第一次成功 Acquire 之前；GPU转换、回读和发送没有进入。此结果不能证明其他 Rust/停止缺陷不存在。
- 最终完整 Release 与增强fixture复跑 `run/rust-source-dll-smoke-c52456316daf/failure.json`：35.025 s、1796次Acquire全部超时、0帧；TX停止 `[1,1,1,0]` 后destroy0，cancelled=true/exit0。使用与最终WGC/Raw门相同的 `fdc6d8dc…` DLL，仍为FAIL。日志和SHA-256清单保全于 `run/stream-repair-20261126-final/report.json`。
- 独立 `D3D11CreateDevice(HARDWARE, BGRA)`→adapter0/output0→`DuplicateOutput`→`AcquireNextFrame(20)` 探针 `run/dda-native-8296506d9c53.json`：3.020 s、149次 `DXGI_ERROR_WAIT_TIMEOUT`、0帧。不经 Rust、GPU转换、Npcap、MMCSS 或接收线程，同样失败。另一个仅诊断的 `DuplicateOutput1(B8G8R8A8_UNORM)` 在创建期返回 `0x887A0004`（`DXGI_ERROR_UNSUPPORTED`，不是 INVALID_CALL），见 `run/dda-native-0cf435f925f6.json`；未改产品API或回退。
- `run/dxgi-outputs-a426aa642e20.json`：adapter0 RTX4070 的 output0 为 `DISPLAY1`，attached、2560×1440、monitor65537；其余同名 adapter 无输出。`run/fixture-motion-1795632626514355400.json` 的屏幕DC只采样私有fixture区域，两次64点均有效且hash变化，52/54种颜色。早期窗口DC采样因子窗口覆盖全为无效，不作为黑屏/关屏证据。
- `run/display-state-261c3bd1bcff.json`：console/session显示电源状态均1；`run/session-state-d83f7d1eaa36.json`：当前进程实际session1，`WTSSessionInfoEx SessionFlags=1`（UNLOCK），输入桌面 `Default`。不能将本次0帧归因锁屏、关屏或窗口遮挡；具体OS/驱动/显示交付原因仍未知。
- DDA 有帧时的停止/回读、物理NIC/双机、现场Pending和现场黑屏均仍未闭环。**DDA零帧失败不能因为WGC正常或静态单测通过而改成PASS/BLOCKED**；本轮无驱动重置、重启、替换源、降低参数或部署授权。

### 保留的中间失败

`run/wgc-chain-eea193903f3b/report.json` 的首轮fixture高392小于416 crop，按契约明确拒绝，未改成缩放；扩大私有fixture到640²后得到上表268帧证据。`run/wgcdiag-9aef5edf29df/report.json` 的crop/resize已收90帧，但最终居中区域只有7种Y值，仍按原门FAIL；仅补fixture灰阶覆盖后原门通过，原失败未删除。

### 继续排查：初始化差异的反证

目标检查要求继续实际修复，本轮进一步只在私有本机探针比较，不改产品源或显示设置：

- 历史 C++ 的 `D3D11CreateDevice` flags=0、`AcquireNextFrame(500)`：`run/dda-native-8387d37639fa.json` 在3.005秒内6次超时、0帧。较长等待和旧标志没有恢复取帧，不将此作为产品参数更改。
- 精确适配器身份：`run/dxgi-outputs-e3d9c9729812.json` 的 attached output0 属于 LUID `[72491,0]`；默认硬件设备探针 `run/dda-native-f709ec1afdd3.json` 使用相同LUID，在3.006秒内149次超时、0帧。没有证据支持设备名称相同导致误选GPU。
- 显式 `CreateDXGIFactory1`→adapter0→`D3D11CreateDevice(adapter, UNKNOWN)`：`run/dda-native-ece0ff5678e7.json` 确认同LUID，在3.012秒内149次超时、0帧；显式绑定同一设备未改善，不改产品选卡策略。
- `DuplicateOutput1` 显式允许 BGRA8/RGB10A2/RGBA16F 的诊断仍创建失败 `DXGI_ERROR_UNSUPPORTED`，见 `run/dda-native-15a6b3f92031.json`。未实施运行时API回退或格式变更。

这些反证缩小本机假设，但不提供现场远端停止卡点。继续定位需要发送端真实 stats/log_tail 等证据；未有单独授权前不直接访问对端、不部署、不重启驱动或终止现用进程。

### 只读现场接口快照（用户授权）

用户授权只对 `192.168.2.59:5143` 读取 `GET /health`、`GET /version` 与 `GET /sender/stats`，不包含任何控制动作。证据原件与 SHA-256 清单：`run/peer-readonly-20261126-074120-a7585fad/`。

- `/health`、`/version` 均为HTTP 200：Rust capture角色、版本0.2.1、build `20261126-002714-cbcb20`、PID 11440、Managed ROOT；session health 仍running。
- `/sender/stats` 精确匹配原现场session `s-26bc9b1be54e2f58`，`running=true`、`stopping=true`、`exit_code=null`；source DDA/RTX5070 Ti/2560×1440、416×416@240 Raw、MTU1500、每帧179片。第一次读取显示累计349帧/62471包、`fps_actual≈12.65`。随后5次GET、间隔3秒、观察约12秒，所有快照仍为349帧/62471包、同一session且没有终结错误字段。这证明当时控制状态报告 `stopping=true`、统计计数在观察窗口不变；**不单独证明 native worker 此刻仍在执行**：`NativeSlot` 在 `cleanup_pending` 时会强制对外报告 `running=true`，状态可来自停止前缓存，底层 worker 也可能稍后结束而尚未通过重试 join/reap。它不能定位 GPU/Npcap/teardown，也不能推断黑屏像素。历史 stop 返回 Pending 仍说明当次未确认回收。
- 用户另授权只读采样 PID 11440 线程等待状态及相关 Windows/Npcap/DXGI 记录。本机会话不是该远端进程所在主机；5985/5986 PowerShell Remoting 不可连接。随后通过获准的 DCOM/WMI 与远程事件查看尝试，目标拒绝访问，结果保留于 `run/peer-thread-diagnostics-20261126-075427.json`。没有尝试绕过权限、没有新控制请求、没有终止进程或重启。
- 2026-11-26 12:20:39–12:20:43（UTC+08:00）在原只读授权范围内复查：`GET /health` 与 `GET /version` 均为 WinError 10061 connection refused；紧邻的 `GET /sender/stats` 重试也被拒绝，但该次精确时间未被记录。完整复查记录：`run/peer-recheck-20261126-122039-92dcf7/report.json`。这只能说明当时 TCP 连接被拒，不能判断服务是否退出、端口/主机是否变化，也不能认定已正常停止。
- 已生成只读现场脚本 `run/peer-local-diagnostics-readonly.ps1`。首次不带确认参数运行会写出当前 PID/映像路径/创建时间、筛选后的24小时事件元数据及有限深度诊断文件清单；**只有操作人员核对当前显示的完整路径和创建时间，并显式传入两项确认后**才采该 PID 的线程等待原因，采样前后还会复核 PID/路径/创建时间，变化则丢弃线程快照。脚本不读取完整 dump、不采函数级调用栈、不暂停/停止进程，也不收集命令行。得到的是线程状态/等待原因和模块诊断线索，不等价于完整 native stack；函数级栈或内存 dump 若确有必要，仍须单独授权。mock-only 自测覆盖未确认、PID缺失、身份不符、确认值不符、采样期间变化及查询拒绝等分支，Windows PowerShell 5.1 下 10/10 通过，未触碰现场 CIM/事件日志。

### 当前收口边界

已实现并验证本机可覆盖的实际代码修正，但目标尚未达成：本机DDA原生交付仍失败；远端5143最近复查拒绝连接，当前服务/进程身份、真实停止回收情况与现场像素均未确认，也没有运行修正版现场二进制。没有新包、提交、部署或强制卸载；Rust-R3b保持IN PROGRESS。后续现场线程采样需操作人员核对并显式确认进程路径/创建时间；本机不能冒充实际双机恢复。

参考：[AcquireNextFrame 的超时定义](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutputduplication-acquirenextframe)、[DuplicateOutput](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput)、[DXGI 错误码](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-error)。文档定义不证明本次具体OS/驱动根因。
