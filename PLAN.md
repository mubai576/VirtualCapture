# PLAN：VirtualCapture Rust

入口/仓库地图/硬规则见 AGENTS.md；长证据只放docs/bringup或proposals。历史PLAN与旧实现查Git；docs/history为只读档案。

## 0. 当前状态（2026-11-22）

- 开发全面转Rust，产品宿主进程内加载vc_stream.dll；旧C++/Python产品树和旧运维脚本从工作树移除，只保留Git历史。独立Python测试参考与打包协调不是产品后台，继续保留。
- 当前编码Raw/显式PyroWave；DDA/WGC仅硬件GPU转换，CPU MJPEG移除。必要CPU上传/回读/Npcap/hash/SHM仍存在，不宣称零CPU或零拷贝。
- 正常枚举已连接物理网卡，不按厂商或型号偏置；MTU可配置并默认读取双端网卡。本次仅只读查询与产品逻辑，不修改物理网卡、不操作PC2、不注册真实虚拟卡。
- Rust-R3保持IN PROGRESS。既有 Raw/PyroWave DLL Loopback、GPU DDA/WGC、隔离 DirectShow 等本机证据见[原打包记录](docs/bringup/bringup-20261119-rust-r3a-package.md)。D31/D32源码已提交`d6eb33d`，从该干净提交生成0.2.1精简网卡修正版包；314项测试与10项完整/扩展包检查通过，真实GUI非黑屏，见[交付记录](docs/bringup/bringup-20261120-rust-r3a-connected-adapters-package.md)。历史三编码与GUI黑屏仍只代表当时版本，不回填通过。
- 物理网卡/双机/长稳/画质/完整DShow编排仍待专项；本机便携包与隔离OTA通过不等于硬件或部署验收。旧双机自动T1–T5工具已移除，Rust等价编排/完整取证工具尚缺，门限不变。
- 已用部署不因本重构改变。角色按实际运行状态，不从旧PC编号推断；历史封存见 docs/bringup/bringup-20261102.md，后续部署观测见 docs/bringup/bringup-20261113-release-0.5.0.md。

## 1. 当前契约

| 范围 | 契约与真相源 |
|---|---|
| 桌面角色 | 双击首次选择采集/接收；确认后独立 `vc-startup.json` 原子持久化，下次进入该角色；空闲角色页或 `--select-role` 可重新选择，Managed ROOT 保留重选及自定义监听参数；回滚保留已有 data 偏好，OTA 按 manifest 能力恢复实际图形/后台模式并兼容旧入口；见 references/rust-control.md |
| Wire | proto/vc_wire.h冻结28B、0x88B5；payload=mtu−42，Raw帧B=w×h×3/2、N=ceil(B/payload)，不占保留位；skill references/wire-protocol.md |
| 产品拓扑 | `VirtualCapture.exe` 首次选端并保存偏好，启动已选 GPUI 角色；接收端无本机 REST，采集端内嵌 REST v2（默认 5143），`capture` 子命令仍运行控制台 REST 后台；`vc-updater.exe` 独立更新/回滚。角色进程内加载 C ABI v1，EXE/DLL 共用数据面，不拉起旧 sender/receiver；references/rust-control.md |
| 网卡/MTU | 仅列 Windows 状态为Up的物理非虚拟网卡，不按型号偏置；本机按控制连接路由精确匹配，对端多候选须明确选择。显式选择保持、失效后重选。新配置自动MTU，旧缺字段保留显式值；双端只读NlMtu取最小、上限9000，不改系统参数 |
| GPU/编码 | DDA/WGC强制硬件GPU；pattern独立诊断。Raw不压缩；PyroWave显式开发/精确backend、GOP1、真实GPU Vulkan1.3；MJPEG明确拒绝，旧codec ID不复用；references/encoded-stream.md |
| 批提交 | Raw物理卡整帧一个sendqueue，编码按实际AU整帧一次提交，零/短发送失败。显式Loopback固定batch1例外，不冒充产品性能 |
| 生命周期 | stop/destroy仅0证明释放；pending保留worker/句柄/DLL，未知destroy不盲重试。PyroWave每操作5s粘性看门狗不退出宿主；待清理锁配置/OTA/虚拟卡 |
| SHM/DShow | docs/spec/shm-v3.md；动态NV12、12槽、只读seqlock、事件对；D12从无好帧不输出、停流只重复最后好帧。隔离verifier由Cargo构建，无注册表枚举回退 |
| 归属/OTA | 全局owner+本根schema2精确证明/互斥/刷盘pending；未知拒绝接管。固定app/data/updates，配对令牌独立，stage不等于activate成功；references/rust-control.md |
| 构建/包 | Cargo workspace/locked 为唯一产品入口；CMake 仅处理 PyroWave 第三方库与运行库。便携包顶层仅 `VirtualCapture.exe` 和 `vc-updater.exe`，无启动脚本、独立诊断 EXE 或许可材料；构建树保留完整 COM graph 验证。`unified-v1` 清单标记区分旧拆分版；唯一 dist 不覆盖旧成品，GUI 黑屏硬失败；references/packaging.md |
| 判据 | test-matrix.md冻结T0–T5/D1；Raw整帧FNV，性能motion且注明batch/adapter，跨机不减QPC；有损transport/decoded出口分开核对 |

浮点只允许已批准UNORM格式解码例外且逐字节匹配整数参考，不扩展为图案生成许可。实际配置/能力/精确回显优先，预算不当实测。

## 2. 决策记录与待决事项

D1–D28为历史决策原记录，涉及已移除实现的路径/命令只在Git历史中有效；后续决策优先，不把旧授权解释为本轮部署/提交授权。

| # | 事项 | 建议 | 解锁 |
|---|---|---|---|
| D1 | T-06 判据重定 | ✅批：节奏门=发端 cadence 4.166±0.3ms（PC1 本地 hashlog，不涉跨机时钟）；抖动门=收端 p99<8.3ms 保留；收端 avg 只判稳态窗口 | P2-2 |
| D2 | OTA 0.4.0 | ✅方案批（stage/activate 分离 + execv + 版本化 exe + GC + 全树 prev）；已落地，PC2 实推全绿（P3 DONE） | P3 |
| D3 | receiver hashlog 加 qpc_ns | ✅批：实际写点 vc_receiver.cpp:443，全文件唯一；reconcile 只比 frame_id+hash | P2-1 |
| D4 | dda selftest 无屏 SKIP | ✅批：有屏仍须 PASS；初始化失败但非无输出仍判 FAIL | P2-1 |
| D5 | `--batch` 默认值 | ✅批：改 69 | P2-1 |
| D6 | 0.3.7 re-stage | ✅批：作废，并入 0.3.8 | P2-1 |
| D7 | checker-transient | ✅保留并自动判：发端 hashlog 相邻帧间隔 >1s 由 gap_any.py 判（曾现 1713 帧停住 8.5s，删等于丢证据） | P2-3 |
| D8 | 范围与 P4-2 门限口径 | ✅批（2026-10-23）：本项目只管**采集与传输**，推理不在范围内。延迟口径 = PC1 采集完成 → PC2 重组完成，分段同机 QPC 测；**门限候选 <10ms（全程 p99）**：双机实测 9.83ms（batch=69 + D14 网卡调优），Npcap 用户态实用极限；确认包（flags bit1）待 owner 终审 | P4-2 |
| D11 | noise 种子 + 只读映射 + 撕裂回退 | ✅批（2026-10-24，三项全做）：①noise 种子 `fid<<32|C` 与 C 高 16 位 0x1234 置位 bit{2,4,5,9,12} 重叠，1500帧只有128独立帧/T1 14400帧512个/T2 144000帧4608个（约每32帧不同）；已改 XOR（C++/Python/small64 三处，VEC 已重核）；历史 noise 对账仍有效（逐帧比），防压缩/去重主张被削弱，motion 不受影响（XOR 种子）；②读端禁 LOCK 前缀（已改 plain volatile 读）；③撕裂读跳过该 tick（缺口由 discontinuity 标，motion 只保留给无映射 bring-up） | P4-3 |
| D12 | vcam 无信号默认行为 | ✅定（2026-10-23，owner 授权 AI 定；owner 可推翻）：receiver 未运行/从无有效帧时 vcam 不输出任何样本（下游看到停顿而非假运动画面）；motion 帧源改为显式 bring-up 开关（默认关）；有过有效帧后撕裂/停顿则重复最后一帧好数据。实现完成 7e7ce80（vcam.cpp：m_lastGoodFrame/m_hasLastGood/m_motionEnable，三态全改，verify.py ALL PASS） | vcam |
| D13 | UNORM 解码 float 例外 | ✅定（2026-10-23，owner 授权 AI 定；owner 可推翻）：硬规则第 2 条禁的是“生成像素用浮点”；B8G8R8A8_UNORM→float→floor(v*255+0.5) 属格式转换，允许用 float，但结果必须和 CPU 参考逐字节一致（dda_gpu 自测 5 档已证）。§1.5 已注，不许扩散 | — |
| D10 | P4-1 测量口径与 idle 语义 | ✅批（2026-10-24）：`gpu_ms` 切真 GPU 时间待 PC1 2K240 实测分布后再定，`gpu_dispatch_ms` 先作诊断；dda selftest 连续 5x E_TIMEOUT（2.5s 无新帧）判 SKIP（idle 与故障不可分，live 已容忍超时） | P4-1 |
| D14 | PC2 网卡调优（I226-V） | ✅执行（2026-10-23，AI 操作，owner 可回滚）：FlowControl=0、ITR=0、EEE=0、InterruptModeration=0；实测 rseg_max 1015ms→35ms（-97%），rseg_p99 8.3ms→6.7ms（-19%），全程 p99 12ms→9.8ms；步骤见 docs/runbooks/pc2-nic-tuning.md；回滚命令见同文件 | P4-2 |
| D15 | 端到端延迟优化 1+2 组（含 shm v2 IPC 变更） | ✅批（2026-10-28，owner"1 2 一起优化"）：见 §1.4 shm v2 条；本机回环 motion/240/batch1 实测 pc1_seg 1.03→0.37ms、rseg 2.34→0.39ms、vcam 出口 vseg avg 0.03ms（旧轮询理论 +2.1ms）；双机（PC2 发→PC1 收+vcam，motion/240/batch69）rseg p99 6.70→2.05ms、max 35→2.26ms，vseg p99 0.08ms，7200 帧对账零错；证据 docs/bringup/bringup-20261028-latency-opt.md。PC2 已推 0.4.2（发版号 AI 暂定，owner 可改）；v1/v2 不互通，下游须重开 | ✅已部署 |
| D16 | P5 GUI 整合方案 | ✅批 A（2026-10-29，owner）：agent 内置 Web UI；首批 G0/G1/G2 已落地。双端均有 UI 的初始默认已由 D17 替代；WebView2 外壳、访问控制仍未定 | P5 G3/G4 |
| D17 | 主机 UI 与采集后台分工 | ✅批（2026-10-29，owner 本轮要求）：取消角色选项，只有主机 UI；对端只填 IP，固定端口 5143；自动获取网卡后选择；SHM 隐藏固定；开始接收自动注册、结束自动回收；取消对端页面跳转；开始/停止采集共用按钮。实现与迁移见 docs/proposals/gui-host-20261029.md | P5-U2 |
| D9 | 本机主 agent 换代到 0.4.0 | ✅被后续部署覆盖（2026-11-02 只读核对）：当前两端均为 0.4.1 / product_revision=2，不再为旧 0.4.0 事项重启。2026-10-23 的 activate 无应答记录保留为历史，不据此声称该竞争已修复 | — |
| D18 | 实际使用反馈与本轮收口范围 | ✅owner（2026-11-02）：约三天实际使用、目前满意；按计划先封存已用版本、补准确状态、整理现有改动。使用反馈不代替 test-matrix 验收；不放宽门限、不改数据链路、不自动提交或发版。远端实际二进制封存仍需本地导出或可读共享 | P5-U4 |
| D19 | 可配置采集与编解码扩展 | ✅owner（2026-11-02）：允许新增 H.264/H.265 有损链路，保留 raw；允许可变尺寸会话/SHM/虚拟卡契约扩展；预设 416、640、720p、1080p、1440p + 自定义宽高/帧率。28B 包头不因本授权自动解冻，历史 raw 判据不变，稳定部署不自动替换。按阶段落地，不把预算或选项存在当成原生能力/实测验收。设计见 docs/proposals/configurable-stream-20261102.md | P6 |
| D20 | P6双机并存验证与正式切换 | 待owner确认：建议先批准PC2全新目录/备用诊断端口并存、后端实测和小规模物理链路，不停稳定5143；正式GUI/虚拟卡切换另定维护窗口。当前PC2仍是旧后台，不能核验新codec。门禁与回滚见 docs/runbooks/p6-cutover.md | P6-3/4双机验收 |
| D21 | 优先转向MJPEG | ✅owner（2026-11-03）：“先转为 mjpeg 吧”。新增可选MJPEG与质量配置、先做640本机验证；保留Raw/H.264/H.265（其中H.264/H.265后由D23移除），不自动替换稳定部署。每帧独立，不套用H.264 GOP恢复或有损像素相等门 | P6-3 MJPEG |
| D22 | PC1 Raw 数小时后卡顿排查 | ✅owner（2026-11-08）：批准独立采样、生命周期修复候选、分项阻塞计时和隔离回归；不自动换已用DLL/部署、不改网卡或下游业务逻辑。六小时现场关联与整机根因仍待实用复现 | P5-LR |
| D23 | 移除 H.264/H.265 | ✅owner（2026-11-13）：这种编解码方式不适合当前项目。移除 Media Foundation H.264/HEVC、原生 NVENC、`vc-codec-probe`/`vc-codec-roundtrip` 及全部相关测试结果/报告/开发包；保留 Raw 与 D21 的 MJPEG。不自动替换稳定部署；契约与 PLAN/AGENTS/skill 同批更新 | P6-3/P6-4 |
| D24 | 接入 PyroWave | ✅owner（2026-11-13）：“开始 PyroWave 接入”。新增显式开发编码 `pyrowave:vulkan`（MIT，Vulkan 1.3 计算小波，intra-only），保留 Raw/MJPEG；阶段一只做 CPU 上传/回读，不做 D3D11 互操作；每帧预算=min(max_bitrate÷8÷fps, max_au−48, NV12 帧字节)，无质量旋钮；不自动回退/替换部署；双机/长稳/画质未验收。构建与限制见 `docs/runbooks/pyrowave.md` | P6-3 |
| D25 | P6 review修正版提交/打包/部署 | ✅owner（2026-11-13）：“提交 打包 部署”。批准本轮修复提交、生成0.5.0便携包并切换双端；保留旧目录、配置与运行证据，编码仍须显式启用。0.5.0已提交/打包并在本机host启动；PC2预检为0.4.1 host/receive错误角色，按切换门禁未执行远端部署；最终发现本轮之外已将PC2更新为0.5.0 capture，并将双端改为活动PyroWave，停止进一步部署操作，不覆盖并发状态；远端包manifest未核验。证据见[发布记录](docs/bringup/bringup-20261113-release-0.5.0.md)；双机/长稳/画质仍未验收 | P6-R1发布 |
| D26 | Rust + GPUI 重构 | ✅owner（2026-11-14）：在新 `rust` 分支开发、现有实现先不动；首期Rust控制面复用C++二进制，主机GPUI内嵌逻辑、被采集机CLI后台，允许重新设计API。后续软件集成先保留UI无关核心；C ABI DLL后续实现。28B头不因控制面授权自动解冻，不自动部署。设计见 docs/proposals/rust-rewrite-20261114.md | Rust-R1 |
| D27 | Rust后端、OTA与运行布局 | ✅owner（2026-11-14）：OTA最高优先级，恢复自动更新通道；重新规划工作/更新目录，开始后端Rust迁移，并行处理双端物理网卡、UI冗余与应用图标。反馈切换后约增加10ms，按同配置/同机分段排查，不预判根因；28B头/Raw判据保留，旧部署与运行证据不自动覆盖。 | Rust-R2 |
| D28 | 全面转 Rust 产品路径 | ✅owner（2026-11-19）：目标全面转Rust；产品宿主通过版本化DLL模块在进程内连接数据面，不再以sender/receiver EXE CLI拼装本机数据面；模块可编译DLL与独立EXE并复用同一实现；仅NV12 Raw/MJPEG/PyroWave。Raw启动按尺寸/MTU计算一帧一个batch；编码按实际AU整帧单次queue；Loopback仅显式诊断且固定batch1例外。旧C++/Python契约与稳定部署不改、不自动替换；Rust-R3保持IN PROGRESS，实测验收前不标DONE。 | Rust-R3 |
| D29 | Rust-only、I226/可配置MTU、仅GPU与中文文档 | owner（2026-11-19）要求：“使用网卡 i226；可配置mtu 默认读取网卡参数；统一迁移到 rust；只做GPU 降级到CPU无法满足我的性能需求”，并要求移除非Rust工程产品代码、简化文档、Rust注释中文化。执行为旧产品只留Git、保留独立测试/构建工具、删除CPU MJPEG/转换降级；不自动部署/提交/push，不解冻wire或放宽验收。I226多卡拒绝与MTU双端最小是实现默认，不冒充额外owner批示。 | Rust-R3结构收拢 |
| D30 | Rust-R3a 提交与打包 | owner（2026-11-19）要求“提交打包”：提交已审阅的Rust-only改动，从干净提交生成0.2.1便携包并执行完整及扩展隔离本机门禁；未授权push、部署、PC2、物理网卡变更或真实DShow注册。源码提交a918e4f，原包产物与边界见[打包记录](docs/bringup/bringup-20261119-rust-r3a-package.md)。 | Rust-R3a包 |
| D31 | 精简 Rust 便携包 | owner（2026-11-19）要求移除无用包文件，并明确许可材料无需随包分发。许可收集/例外检查仍在 build 审计目录执行；便携包不含许可文件、许可元数据、`vc-stream.exe` 或 `vc-transport.exe`，保留运行时依赖 `vc-vcam-verify.exe`。精简包和验收证据见[记录](docs/bringup/bringup-20261119-rust-r3a-trimmed-package.md)。 | Rust-R3a 精简包 |
| D32 | 产品网卡枚举不偏置 I226 | owner（2026-11-20）澄清：“只是当时测试用的网卡是 i226”，要求正常枚举已连接物理网卡。产品候选限定 Windows `Status=Up`、明确硬件且非虚拟；不按厂商/型号偏置。本机自动选择按控制连接路由精确匹配，对端唯一候选才可预览，多候选明确选择；MTU只读契约不变。 | Rust-R3a 网卡策略修正 |
| D33 | 精简网卡修正版提交与打包 | owner（2026-11-20）要求“提交 打包”：提交D31/D32已审阅改动，从精确干净提交`d6eb33d`生成0.2.1新包，完整及扩展门禁通过；不升版本、不push、不部署，不授权PC2、物理网卡变更或真实DShow注册。23文件、来源/哈希与证据见[交付记录](docs/bringup/bringup-20261120-rust-r3a-connected-adapters-package.md)。 | Rust-R3a 修正版包 |
| D34 | 两 EXE 便携入口 | owner（2026-11-21）要求把包内五个 EXE 减到两个。合并主机、采集和隔离能力探测，独立保留更新器；新采集默认 5143，OTA 沿用运行实例原 bind/port。不改数据面/验收门限，不自动部署、注册或切稳定版。 | Rust-R3a 两 EXE 入口 |
| D35 | 启动选端、重选与持久化 | owner 要求启动选择采集/接收角色并独立持久化；首次选择、下次直启、本机角色空闲时重选，显式 capture 后台不受偏好影响，OTA 恢复实际运行模式。已完成实现和本机 GUI/OTA 专项验收；本次另行授权提交与重新打包，不部署。 | Rust-R3a 桌面角色 |

## 3. 当前任务卡

### Rust-R3 模块化全Rust产品 — IN PROGRESS

已实现版本化进程内DLL、Rust采集/编解码/Npcap/SHM/COM、OTA及精确owner恢复。历史本机证据：[Rust-R3](docs/bringup/bringup-20261119-rust-r3-local.md)、[Raw诊断](docs/bringup/bringup-20261119-rust-data-plane.md)。Rust-R3a 收拢与 GPU/MTU 子卡已通过本机总门，证据见[本轮收拢记录](docs/bringup/bringup-20261119-rust-r3a-restructure.md)。本机便携包与隔离 OTA 门禁已补齐，见[打包记录](docs/bringup/bringup-20261119-rust-r3a-package.md)；随后按 D31 精简至 23 个包文件，见[精简包记录](docs/bringup/bringup-20261119-rust-r3a-trimmed-package.md)。D31/D32现已提交并从干净来源重新打包，见[D33交付记录](docs/bringup/bringup-20261120-rust-r3a-connected-adapters-package.md)。Rust-R3 整体仍需 Rust-R3b 的双机矩阵工具及物理链路、长稳、画质、目标桌面与完整 DShow 编排验收；不以历史292 tests、本机Loopback或包门禁代替。

### Rust-R3a 仓库收拢与GPU/MTU — DONE

旧产品/根CMake/退休工具仅从工作树移除并保留 Git 历史；Cargo提供隔离vcam verifier，独立Python图案/向量参考迁入rust/tests/reference。当前文档统一Rust入口；中文注释、GPU-only、MTU实现通过总门（fmt、22组/312项测试、Clippy、Release、独立参考与DLL自检）。该卡完成时的I226选择实现为历史状态，当前规则按D32执行。详细专项证据与未验收边界见[本轮bring-up](docs/bringup/bringup-20261119-rust-r3a-restructure.md)。已提交a918e4f并生成0.2.1便携包，10项包门禁全通过，来源/哈希见[打包记录](docs/bringup/bringup-20261119-rust-r3a-package.md)。后按D31移除包内许可材料和独立诊断EXE，最终批次`20261119-230840-bedfe1`为23个文件，312项测试与10项包检查通过，ZIP/逐文件哈希独立复核一致；精简批次当时未提交的状态保留在[精简包记录](docs/bringup/bringup-20261119-rust-r3a-trimmed-package.md)；D31/D32现已提交`d6eb33d`并按D33重新打包，未部署，见[最新交付记录](docs/bringup/bringup-20261120-rust-r3a-connected-adapters-package.md)。

### Rust-R3a 网卡策略修正（D32） — DONE

已移除 I226 偏好，产品统一枚举已连接物理网卡，路由精确自动选择、多候选明确选择；Rust 总门314项测试及 Release 网卡/真实GPUI smoke通过，证据见[网卡修正记录](docs/bringup/bringup-20261120-rust-connected-physical-adapters.md)。已随D31提交`d6eb33d`并按D33重新打包；未部署，不代表物理收发或双机验收。

### Rust-R3a 精简网卡修正版提交与打包（D33） — DONE

源码提交`d6eb33d`；干净来源0.2.1包`20261120-005019-063265`为23个文件，314项测试、10项完整/扩展包检查、真实GUI截图与来源/ZIP/逐文件哈希独立复核通过，见[交付记录](docs/bringup/bringup-20261120-rust-r3a-connected-adapters-package.md)。交付后仅补本记录与PLAN证据，不改包内来源；无push或部署。

### Rust-R3a 两 EXE 便携入口（D34） — DONE

`0.2.1` 包 `20261121-163453-82bb3e` 共 16 个文件、仅两个 EXE；原 D34 验收证据见[记录](docs/bringup/bringup-20261121-rust-r3a-two-exe-package.md)。本机后续源码增补及 D35 验收见[桌面角色记录](docs/bringup/bringup-20261122-desktop-role-selection.md)：完整 Rust 门禁331项测试、Clippy、Release、独立参考与 DLL 自检通过；Release GUI 首选/重选/重启真实点击通过；Managed ROOT 显式重选写入 data 偏好；图形采集 OTA 恢复窗口及原 bind/port，隔离 Host OTA 通过。用户要求提交代码并重新打包；交付结果待补。本机与 Loopback 证据不代表物理网卡、双机、长稳或画质通过。

### Rust-R3b 双机矩阵替代与产品验收 — TODO

补Rust控制面的双机编排/完整hashlog保全工具，再在单独授权下按冻结T1/T2等执行；物理NIC整帧batch、双机240fps/长稳、画质、目标桌面DDA与完整DShow分别验收。本机便携包/隔离OTA已通过，不替代上述专项；历史失败/BLOCKED/未跑保持原结论。

## 历史证据入口

- P1–P3与旧PLAN：docs/history/（只读）。
- P4/P5及封存：docs/bringup/bringup-20261022.md、bringup-20261023-p4-4-vcam-acceptance.md、bringup-20261102.md。
- P6与Rust既有实现/发布：带日期docs/bringup与docs/proposals。旧MJPEG报告保留为历史，不代表当前后端存在。
- 未关闭现场Raw数小时卡顿关联、物理双机与维护窗口仍需独立安排；不以结构清理关闭历史观察。
