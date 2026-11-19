# Rust-R1 控制面开发与本机验证（2026-11-14）

## 结论与范围

D26 首期实现位于独立 `rust` 分支/worktree，从 `bb7a6ff` 开始。新增 Rust 控制面与 GPUI 客户端，
未修改原 C++/Python 源码，未提交/推送/部署，未访问 PC2 或接管现用虚拟卡。
后续owner要求绿色运行压缩包，Rust-P1单独制作Release便携交付；记录见本页对应章节。

- 被采集机 `vc-capture`：无界面 CLI + REST，管理既有 C++ sender。
- 主机 `vc-host`：原生 GPUI 客户端，内嵌接收/虚拟卡管理，没有本机 REST 服务。
- `vc-core`：不依赖 GPUI 的配置、预算、HTTP、网卡、会话、原生进程、虚拟卡与主机编排。
- R1 仅 Raw 可运行；编码草稿能保存，但 MJPEG/PyroWave 启动明确拒绝。
- 为未来软件集成留出核心边界，尚未实现 C ABI SDK DLL；现有 DShow DLL 与 SDK 是不同用途。

运行入口见 [Rust runbook](../runbooks/rust.md)，接口以
[Rust 控制面契约](../../.agents/skills/virtualcapture/references/rust-control.md) 为准。

## 实现保护

配置独立使用 `vc-rust-config.json`；已有损坏文件或未知版本报错，不写默认覆盖。
保存同目录临时文件、刷盘、原子替换。部分更新失败保留原草稿。

HTTP 校验 Rust implementation/API/role，拒绝旧 Python 后台；保留非2xx错误体。
普通请求5s，网卡枚举/prepare/start为40s，以容纳有界探测。阻塞工作不在GPUI渲染线程执行。

主机先 prepare，确认完整 capture/session/source MAC 与精确网卡，启动时固定该网卡。
预算含 L2 开销和预留，超过已知物理线速拒绝、速率缺失显示 unknown；不静默选首张网卡。
两端实际 exe 要求 stream revision 2 / variable resolution / SHM v3；自动注册前还隔离探测 DLL。
已有虚拟卡注册拒绝接管，本轮没有执行注册/注销。

活动 sender 拒绝覆盖，停止必须带 session_id；启动超时仍保留按本会话清理的责任。
原生进程提前退出会回传日志，正常停止 CTRL_BREAK、2s后强杀；正常释放控制器会清理并join。
重复session_id已有stats/hashlog/log时拒绝，避免截断已完成证据。

## 构建与静态/单元验证

环境：Windows 11 x64、Rust/Cargo 1.97.1、MSVC 14.44、Npcap。
GPUI：`gpui-kit 0.9.1`（`gpui-pre 0.3.12` snapshot + component），依赖锁在 `rust/Cargo.lock`。
初期R1构建与实际运行为debug；后续Rust-P1另行验证Release优化构建与便携交付，不以初期结果代替。

| 检查 | 结果 |
|---|---|
| `cargo fmt --manifest-path rust/Cargo.toml --all --check` | PASS |
| `cargo test --manifest-path rust/Cargo.toml --workspace --locked` | PASS：vc-core 49 + vc-capture 2；vc-host/doc-tests无测试 |
| `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo build --manifest-path rust/Cargo.toml --workspace --locked` | PASS：vc-host.exe、vc-capture.exe |
| `python -I scripts/verify.py` | ALL PASS：303 passed / 6 skipped；含原生构建、自检、pattern_ref与pytest |
| `python -I scripts/verify.py --no-build` | ALL PASS：303 passed / 6 skipped |
| 实际 vc-vcam DLL 隔离 `--capabilities` | stream_revision=2 / variable_resolution=true / shm_version=3；无注册 |

早期曾有一项 Raw拒绝错误文案断言过时，以及Clippy嵌套条件告警；修正后上述最终门禁全部通过。
总闸日志保留在 `run-rust-legacy-verify.log`、`run-rust-legacy-final.log`，Rust门禁输出在 `run/rust-final-cargo.log`（全部gitignore）。
6项skip不算通过双机/live验收。

## 实际 Raw 控制链路

脚本 `python -I rust/tests/control_smoke.py` 使用随机回环REST端口、独立ROOT、私有SHM，
实际运行Rust后台与C++ sender/receiver；不碰5143、不注册DShow。

最终构建核验目录：`run/rust-smoke-19ff8dbea8/`；先前通过的独立轮次仍保留。
参数完整记录：

| 项目 | 值 |
|---|---|
| source / pattern | pattern / motion |
| 尺寸 / fps上限 | 416×416 / 60 |
| batch / MTU | 1 / 9000 |
| adapter | `\Device\NPF_Loopback` |
| frames | 120 |
| 结果 | 发送120、接收120，逐frame_id FNV全量一致 |
| receiver | dropped=0 / hash_mismatch=0 |

真实REST负例通过：非法配置400且原稿不变、prepare不启进程、完整参数回显、错误会话stop409、
运行中修改配置409、运行中重复start409；完成后复用相同session_id返回409且原hashlog字节不变。
Rust后台CTRL_BREAK正常退出0。hashlog、stats、stdout与report.json都保留在对应run目录。

这是本机功能证据，没有报告物理网卡吞吐、240fps、双机延迟或长稳结论。

## 实际 GPUI 窗口

脚本 `python -I rust/tests/gui_smoke.py` 打开真实GPUI窗口，使用目标HWND的原生鼠标/字符消息
填写本机随机地址并点击连接；本机HTTP错误桩必须收到 `/health`，返回503。
截图目视确认输入值、明确错误原因、事件记录和不可启动状态；WM_CLOSE退出码0。

最终构建清晰截图目录：`run/rust-gui-smoke-3a9d777a00/`，随机地址 `127.0.0.1:59520`。
窗口显示 `/health 返回 HTTP 503: GUI smoke: capture unavailable (503).`；无实时帧时表格为空。
640×640@240默认预算展示线上开销约1.19Gbps，网卡未知不伪装可用容量。

验证过程中修正了冒烟脚本：原屏幕绝对坐标没有点击到控件，新增真实HTTP请求断言；
后台运行者不能可靠获得前台焦点，改为仅向目标窗口发消息。
PrintWindow对GPU窗口返回黑图，最终截图短暂置顶本窗口并只截不透明客户端区域，随后取消置顶，
防止把其他应用窗口内容带入证据。失败/黑图的历史目录保留，不作为通过依据。

此项只验真实窗口、地址输入、连接错误展示与正常退出，没有通过GUI启动采集或注册DShow。

## Rust-UI1 界面重做（2026-11-14）

按owner“更精美的ui”要求，Rust客户端改为浅色采集工作台：独立设备侧栏、深蓝输出画幅示意、
主区采集设置、可折叠诊断表、单处错误提示。画幅按配置比例绘制并明确不是实时预览，
主操作采用蓝色按钮，预设改用原生Button，支持选中/禁用/键盘焦点。
设计与色彩/布局取舍见 [UI提案](../proposals/rust-ui-20261114.md)。

真实窗口证据 `run/rust-gui-smoke-0fd79457a9/`：

- `initial.png`：默认1280×900工作台；
- `presets-720p.png`：真实点击720p与120fps，断言持久配置为1280×720@120，画幅同步为16:9；
- `details.png`：真实展开诊断表，未运行时显示空指标；
- `compact.png`：1080×740客户端布局，设置无横向溢出，纵向内容可滚动；
- `connection-error.png`：输入随机回环端口，错误桩收到health并返回503，单处提示显示具体原因；
- 正常关闭退出0，无采集或DShow注册操作。

本轮fmt/Clippy -D warnings/build通过，workspace测试51项通过；完整原总闸303 passed / 6 skipped。
辅助文字在雾灰底的对比度经计算约4.86:1，深色画幅说明约8.34:1；未加入数据曲线或虚假实时值。
原C++/Python源码及控制契约未改，未提交/部署。日志：`run/rust-ui-build.log`、
`run/rust-ui-legacy-verify.log`（完整总闸）、`run/rust-ui-final-verify.log`（最终no-build总闸）。

## Rust-UI2 主区调整与物理接收网卡（2026-11-14）

按owner进一步反馈，将原来的大幅深色画幅与右侧零散设置，改为连续的白色设置面板：
分辨率、帧率、画面适配、采集来源四行对齐；底部紧凑深色输出栏显示尺寸/fps/格式，
按比例的小画幅明确标注非实时预览，开始/停止按钮紧邻本次规格。保留自动保存、会话锁定与原生按钮焦点。

本机接收网卡由 `vc-core` 统一限制：Windows `hardware_interface=true` 且 `virtual=false`，
显式排除 Npcap Loopback；元数据未知不入列表。展示、自动路由、手动选择、启动检查共享合格列表。
扫描失败清空旧列表；已保存虚拟/失效设备选择清除后要求手动重选，不自动换到另一张卡。
被采集机CLI全网卡枚举和回环诊断保留；契约、PLAN §1与runbook同步更新。

真实窗口证据 `run/rust-gui-smoke-8bf140993c/`（已目视核验）：

- `presets-720p.png`：真实点击并持久化1280×720@120，随后恢复640×640@240；
- `details.png`：展开真实诊断表；`compact.png`：1080×740无横向溢出；
- `physical-adapters.png`：实际枚举14项，仅保留3张物理设备（以太网2、WLAN、以太网3），11项虚拟/回环隐藏；
  事先保存的Loopback选择被清除，配置断言通过；完整CLI枚举和物理子集保存在 `adapters.json`；
- `connection-error.png`：本机随机HTTP桩收到 `/health`，503具体原因集中显示；正常关闭退出0。

Rust workspace测试56项通过（core54 + capture2），Clippy `-D warnings`、fmt与workspace构建通过。
完整 `python -I scripts/verify.py`：ALL PASS，303 passed / 6 skipped；skip不代表双机/live验收。
CLI回环回归 `run/rust-smoke-cb1a64d8aa/`：motion / 416×416@60 / batch1 / NPF_Loopback，
120发送/120接收，FNV全量一致；无物理吞吐或双机性能结论。原源码边界diff检查通过，未提交/部署。
日志：`run/rust-ui2-tests.log`、`run/rust-ui2-gui.log`、`run/rust-ui2-control.log`、`run/rust-ui2-legacy-verify.log`。

## Rust-P1 绿色运行包（2026-11-14）

owner要求“打包 绿色执行环境压缩包”。新增独立`rust/packaging/package.py`，从Rust Release与CMake Runtime install收集
主机/后台、7个原生目标、PyroWave DLL、app-local MSVC运行库、第三方许可、双击启动器及中文使用说明。
新建唯一dist目录/ZIP，不替换旧产品，不带本机配置、历史run或PDB，不执行驱动安装、注册DShow、OTA或远端部署。
原生/协议源码未改；Rust Release主机添加Windows图形子系统，避免隐藏控制台时把GPUI主窗口一起隐藏。

预检已通过：Rust56项、fmt/Clippy/Release build、原总闸303 passed / 6 skipped（`run/rust-package-legacy-verify.log`）。
`build/rust-preflight-console-evidence/report.json`验证实际两个CMD入口、随机端口health、干净PATH、包内运行库实际加载和正常退出。
`run/rust-smoke-c8ca1a5354/`与`run/rust-gui-smoke-3e3aa0472c/`分别为Release回环与真实窗口预检；最终包另从ZIP解压验证。
最终独立包：`dist/VirtualCapture-Rust-0.1.0-windows-x64-20261114-124010-20e160.zip`，16,538,084 B，
SHA-256 `7c47189c51b896532444b2589418a4723209b57ea8a19ab01843d8d023fcbcb8`；旁附`.zip.sha256`，目录同名。
包内1342文件，含514个目标平台依赖的许可/归属记录，未解决许可文本0；OR替代许可/未分发上游文档等说明保留在inventory。
打包来源记录当前未提交工作区逐文件摘要，构建期间来源未变；生成后仅补本页与PLAN的交付记录，不改变包内程序。

最终构建与解压证据：`build/rust-package/20261114-124010-20e160/report.json`、`package.log`、`smoke/report.json`。
完整门禁Rust56项、fmt/Clippy/Release、原总闸303 passed / 6 skipped全部通过，输出日志`run/rust-package-final-utf8.log`。
解压目录包含中文与空格，限制PATH后两个CMD入口/health/正常退出及实际app-local runtime加载通过；DLL仅隔离能力探测，无注册。
解压出的Release链路`run/rust-smoke-48f9bd3a01/`：motion / 416×416@60 / batch1 / NPF_Loopback，120发送/120接收全量FNV一致。
真实窗口`run/rust-gui-smoke-5cf8b3caac/`：预设持久化/恢复、明细/1080宽、14项保留3物理隐藏11、失效选择清除、503原因展示与退出0通过。
最终ZIP的CRC、逐文件SHA-256及清单一致性全部通过；不带本机配置/历史run/PDB，不替换现用dist或部署。

## Rust-R2 OTA、布局与后端迁移起点（D27，2026-11-14）

owner要求OTA最高优先级，四个agent按文件分工处理更新事务、双端物理网卡与延迟审计、UI与图标、Rust数据面迁移。
新控制面版本定为0.2.0，区别于无OTA入口的0.1.0；旧包与已用部署保留，不自动访问PC2/注册现用虚拟卡。

实现：

- Rust OTA覆盖独立64hex令牌配对、ZIP上传/逐文件校验、暂存、独立helper切换与就绪确认、指定历史回滚。
  主机工作线程接通本机与被采集机操作；普通控制API仍用于受控直连网络，不因更新令牌而宣称全服务鉴权。
- 固定ROOT：无current保持扁平；首次应用更新后程序在app/releases，配置/令牌复制到data，运行证据不移动；
  updates保存incoming/staged/worker/history.jsonl。稳定启动器按current选版本，损坏指针明确失败。
- 复核中修复：旧PID退出后的复核/安装等错误必须重启旧版；回滚失败单独报code6；capture就绪绑定新PID；
  staged幂等与安装复验拒绝清单外DLL、符号链接/重解析点；临时上传先释放句柄后清理。
- 双端产品网卡筛选统一，采集端默认仅物理网卡，回环要求显式诊断开关与本地控制器；CLI保留完整枚举。
- 主区删大标题、重复引导和大黄色待清理横幅，阻塞原因留在操作栏；更新面板按需展开，令牌可重新配对。
  主机SVG图标在Windows构建生成ICO并嵌入PE资源1；指标区明确采集/发送/重组/虚拟卡交付口径。
- vc-transport新增std-only冻结wire解析、分片、CRC/FNV、有界重组与classic pcap inspect/replay诊断CLI；
  实际Npcap采集/SHM/DShow仍由C++承载，本轮不是生产数据面已全面切换。

已完成独立门禁：完整原项目`python -I -X utf8 scripts/verify.py` ALL PASS，303 passed / 6 skipped，
日志`run/rust-r2-legacy-verify.log`。C++真实头/CRC/FNV向量与Rust契约匹配，pcap回放7包恢复frame77、
FNV `0edf612810c4d174`，正确识别坏CRC与重复片并拒绝pcapng，日志`run/rust-r2-transport-vectors.log`。
本轮Rust workspace 116项测试通过（core73、capture6、transport库27/CLI4/contract6），fmt、Clippy `-D warnings`与Release构建通过；
`run/rust-r2-final-cargo.log`记录最终结果。Release PE确认GUI subsystem=2，嵌入RT_ICON/RT_GROUP_ICON资源。
真实原生GUI证据`run/rust-gui-smoke-244516a1b0/`：标题/大横幅移除、更新面板展开/收起、配对与更换令牌持久化、
预设保存/恢复、1080宽窗口、物理接收枚举14→3、503具体错误与正常关闭均通过，已目视截图。
被采集端默认14项中保留3物理、隐藏11、Loopback未授权prepare409且无启动；`run/rust-adapter-smoke-adb814c2cb/`。
Raw回环`run/rust-smoke-3a13fdcc7e/`：motion/416²@60/batch1/NPF_Loopback，120发送/120接收、全量FNV一致。
OTA私有root 17项通过：`run/rust-ota-smoke-f7169dfa0a/report.json`，含code4旧服务恢复、code5自动回滚、
code6指针恢复失败单独报错；此轮为合成载荷，最终Release完整包验证另记，不冒充PC2部署。
整合修复Windows `\\\\?\\`规范路径前缀造成同一ROOT被误判不同的问题，主机更新确认绑定ROOT/build/新PID。
最终0.2.0 Release独立绿色包已生成并交付：

- ZIP：`dist/VirtualCapture-Rust-0.2.0-windows-x64-20261114-140047-1a5966.zip`；18,515,362 B。
- SHA-256：`d46f6d2047b79336b11fec22139bbcd31dcbc3c21379595f63e42f55a2a4aec3`；旁附`.zip.sha256`。
- 同名便携目录共1366文件（清单声明1365个载荷文件，另含清单本身）。最终ZIP CRC、逐文件SHA-256及清单一致性通过；
  交付时再次核验ZIP摘要与校验文件，并检查无配置、更新令牌或配对令牌。
- 最终报告：`build/rust-package/20261114-140047-1a5966/report.json`；完整日志同目录`package.log`，
  汇总日志`run/rust-r2-package-final.log`。Rust116项、fmt、Clippy、Release与原总闸303 passed / 6 skipped通过，
  portable/GUI/物理网卡/Raw回环/采集端OTA/HostController OTA六项运行门禁全部通过。
- 打包来源为未提交工作区逐文件摘要；构建期间来源未变，之后只补本页与PLAN交付记录，不改变包内程序。
  原0.1.0包、现用部署和历史证据保留；未提交、未访问PC2或接管现用DShow。

以下均为本轮最终候选包解压出的Release独立证据，不沿用早期UI1/UI2结论：

| 验证 | 证据与结果 |
|---|---|
| 中文/空格路径与稳定启动器 | `build/rust-package/20261114-140047-1a5966/smoke/report.json`；限制PATH后两CMD、health、正常退出、实际app-local运行库加载通过；managed启动器读取data中120fps配置并从当前release加载运行库 |
| 双端物理网卡 | `run/rust-adapter-smoke-fe73acf991/`与下列GUI报告；14项保留3项物理、隐藏11项；未显式诊断时Loopback prepare返回409，未启动采集 |
| Raw功能回归 | `run/rust-smoke-5eba918691/`；motion / 416×416@60 / batch1 / NPF_Loopback，120发送/120接收、全量FNV一致，仅为本机功能证据 |
| 真实GPUI | `run/rust-gui-smoke-9213180171/`；图标、首帧回执、标题/横幅移除、更新面板、配对/更换令牌、配置持久化、物理网卡、503具体错误、窄窗口与正常关闭通过 |
| 采集端完整包OTA | `run/rust-ota-smoke-e838184837/`；实际candidate完整1365文件载荷暂存并激活为`20261114-140047-1a5966`，legacy→managed与配置/令牌保留通过；后续合成故障包验证额外DLL拒绝、旧服务恢复、历史回滚、code5自动回滚和code6恢复失败准确报告 |
| HostController更新 | `run/rust-host-update-smoke-cac047ece3/`；对端合成包经真实helper重启；本机使用最终完整候选载荷激活为`20261114-140047-1a5966`，新GPUI写入首帧回执、helper历史确认active、配置/配对令牌保留，窗口正常关闭且DShow注册未变 |

“选择ZIP”的原生Windows文件对话框另有真实交互证据`run/rust-ui-zip-probe-a477e306/report.json`：
通过对话框选择完整候选载荷并由GUI命令暂存成功；该项使用较早候选`20261114-135446-4d9173`且未激活，
最终候选的激活/回滚以表中Release OTA报告为准。旧0.1.0没有更新端点，首次启用需一次bootstrap；之后通过固定ROOT更新。

延迟：owner反馈切换后增加约10ms，尚未独立复现/定位。默认参数和原生二进制一致性核对不排除GUI负载、
驱动或现场配置差异；[审计记录](../proposals/rust-latency-audit-20261114.md)明确同配置、同机分段A/B边界。
[OTA布局](../proposals/rust-ota-layout-20261114.md)与[数据面迁移](../proposals/rust-data-plane-20261114.md)记录具体契约。

## Rust-UI3 主机图形启动入口（2026-11-14）

owner指出已有GUI仍要求使用CMD不合理。0.2.1新增`VirtualCapture.exe`（`vc-host`包内`vc-launcher` bin），
使用Windows图形子系统及原生图标，直接选择current版本并拉起GPUI，不执行CMD/PowerShell。
根目录/原生目录支持显式参数，启动日志与PID回执保留在run，失败显示原生消息框。
从0.2.0首次OTA进入本版时，新host在首帧就绪前将版本目录中的入口复制到固定ROOT，之后可一直双击该入口。
已有入口保持稳定，版本选择随current变化；被采集机保留原CLI后台模式。
旧0.1.0/0.2.0成品不替换。0.2.1独立绿色包已交付：
`dist/VirtualCapture-Rust-0.2.1-windows-x64-20261114-144748-460022.zip`，18,915,081 B、1367文件，
SHA-256 `75db17c2153efd2adf111ae6c907e141e6208ea4494ef586c82268653052e157`，旁附`.zip.sha256`。
最终报告`build/rust-package/20261114-144748-460022/report.json`，完整日志同目录`package.log`与
`run/rust-ui3-package.log`；Rust116项、fmt/Clippy/Release、原总闸303 passed / 6 skipped及全部运行门禁通过。

- `build/rust-package/20261114-144748-460022/smoke/report.json`：直接EXE启动、中文/空格路径、
  独立ROOT、managed current解析、data配置与实际release运行库加载、正常关闭；PE校验入口/GUI均为subsystem2且有图标资源。
- `run/rust-host-update-smoke-f9676ab7a8/report.json`：完整本机候选经helper激活，新host首帧回执和active历史确认；
  ROOT自动生成的图形入口与载荷一致，直接打开当前release、正常关闭，配置/配对令牌保留，DShow注册未变。
- `run/rust-gui-smoke-16fbe94a2d/`：真实GUI预设、物理网卡、更新配对/更换、错误展示与正常退出回归通过。
- `run/rust-smoke-223750b640/`：motion / 416×416@60 / batch1 / NPF_Loopback，120发送/120接收全量FNV一致；仅本机功能验证。
- `run/rust-adapter-smoke-079c30a02c/`与`run/rust-ota-smoke-c953bc3126/`分别通过物理筛选与完整候选OTA/故障恢复门禁。

最终ZIP再次核验摘要、CRC、图形入口资源及无配置/令牌。构建期间源码快照不变，之后仅补交付文档；
原生产源码未修改，未提交/部署PC2。

旧0.2.0真实窗口兼容性追加验证通过：`run/rust-ui3-upgrade-9933ba2d7d/report.json`，
日志`run/rust-ui3-upgrade-retry.log`。从旧交付包复制到独立中文/空格ROOT，通过真实Windows文件对话框选择
最终0.2.1 ZIP、点击应用，旧GUI退出0，helper切换并确认新host首帧；ROOT图形入口与release载荷一致，
120fps配置保留。关闭新GUI后直接运行ROOT/VirtualCapture.exe，再次打开当前release并正常关闭，
截图`exe-reopened.png`已目视确认；未采集网卡、未改DShow注册、未访问PC2。
初次探针误传旧包外层目录，启动前未找到exe；改用实际程序目录后通过，未因此修改产品或重新打包。

## 尚未验收/未实现

- Rust主机完整编排中的实际DShow注册/注销、下游取帧，以及运行中关闭窗口后的完整双端回收尚未专项验证。
  当前桌面的已有注册/部署未接管；不能把只读能力探测写成虚拟卡验收。
- DDA/WGC实际桌面采集未通过本Rust编排专项运行；本轮使用motion测试源。
- 未进行Rust双机、240fps、长稳或软件集成SDK验收；OTA只按R2独立本机证据记录，不代替双机切换。
- Rust数据面已开始wire/校验/有界重组与pcap诊断迁移；生产Npcap/采集/SHM、Rust DShow、编码控制与C ABI DLL尚未完成。
- 正常关闭可能等待正在进行的有界HTTP/网卡探测后才完成清理；强杀/断电不保证回收。

这些边界不改变历史产品验收或Raw判据，也不触发稳定部署切换。
