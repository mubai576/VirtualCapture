# PLAN：VirtualCapture（P5 稳定版封存 → P6 可配置采集与传输）

> 入口与规则见根目录 `AGENTS.md`。本文件只放：现状、冻结契约、待决事项、当前任务卡。
> 完成一项在卡头标 DONE + 一行证据；长证据写 `docs/bringup/` 或 `docs/proposals/`，这里只放链接。
> 档案（只读）：旧 PLAN 全文与 "hist S11/S23/…" 编号 → `docs/history/plan-history.md`；
> 已关闭的 P1~P3 卡原文 → `docs/history/phase3-closed-cards.md`。

## 0. 现状快照（2026-11-02；P6 进度更新 2026-11-03）

P4 历史测试结论保留；2026-11-02 已只读封存当前部署，不新增性能结论。owner 已反馈约三天实际使用满意，优先保住已用版本，不追加压力测试或扩展 G3/G4。详见 `docs/bringup/bringup-20261102.md`。

封存后 owner 已批准继续开发（D19）：分辨率预设 `416×416 / 640×640 / 720p / 1080p / 1440p` + 自定义，
裁切/缩放与双端链路预算。P6-1 与 P6-2 开发版已接通，原生 Raw/四种变换/发送限速/动态 SHM 与虚拟卡完成本机隔离验证。
**2026-11-13 owner 决定（D23）移除 H.264/H.265**（含 Media Foundation 与原生 NVENC）及其测试结果和
`vc-codec-probe`/`vc-codec-roundtrip`；当前编码只保留 D21 批准的 MJPEG。P6-3 MJPEG 开发版已接通
L2/有界解码/每帧恢复/SHM/DShow 与 GUI，默认不启用，须明确开发授权。双机/长稳/画质仍待验收，不替换现有部署。
**同日 owner 批准（D24）接入 PyroWave**（Vulkan 计算小波 intra-only）：开发版后端 `pyrowave:vulkan`，
阶段一 CPU 上传/回读，两端需 Vulkan 1.3；本机 Loopback 功能验证通过，未双机/长稳/画质验收。

| 维度 | 状态 |
|---|---|
| 传输正确性 | ✅ 双机 T0/T1 五档/T2 10min 零丢零错；T2 144000 帧 PC2 hashlog 分页全量对账零错（docs/bringup/bringup-20261022.md） |
| 240fps | ✅ DDA 实测 235fps（testwin D3D11 动画源），batch=69，gpu_convert=on |
| 延迟 | ✅ D14 网卡调优后全程 p99=9.83ms（batch=69）；rseg_max 1015ms→35ms（-97%）；Npcap 用户态实用极限约 10ms |
| P4-1 GPU 采集 | ✅ gpu_dispatch_ms=0.028ms（D3D11 timestamp，235fps 实测）；gpu_convert 代码落地，默认 CPU 路径待 owner 切换 |
| P4-2 延迟测量 | ✅ 分段计时落地（pc1_seg + rseg）；双机实测门限候选 <10ms；确认包（flags bit1）待 owner 终审 |
| P4-3 虚拟采集卡 | ✅ vcam DShow filter 落地，本机回环 4455 帧逐字节零错，240.0fps，D12 无信号行为实现；PC2 接线验收待 P4-4 |
| P4-4 虚拟卡验收 | ✅ DONE 2026-10-23：三判据全通（J1 枚举/J2 NV12 640x640@240/J3 35069帧零错+SHM hash 27/27一致）；tools/vcam_verify/ + scripts/p4_4_shm_verify.py；docs/bringup/bringup-20261023-p4-4-vcam-acceptance.md |
| 产品链路 P4 | ✅ P4-1/2/3/4 全部完成 |
| 网卡调优 | ✅ PC2 I226-V: FlowControl=0, ITR=0（D14，docs/runbooks/pc2-nic-tuning.md）；PC1 Realtek 已同等设置 |
| P5 实际使用 | owner 于 2026-11-02 确认约三天实际效果测试、目前满意；不等同连续 72h 或全量零丢帧验收 |
| 当前部署 | PC1 192.168.2.100 = host（I226-V，receiver + GUI + vcam）；PC2 192.168.2.59 = capture（Realtek 2.5G，sender）；两端 :5143 在线，封存时均停止采集 |
| 版本与来源 | 两端 /health 均报 0.4.1 / product_revision=2；成品清单逐字节一致；主机 87 项载荷哈希通过，封存前 299 项源码与成品记录一致；远端实际二进制尚未独立核验 |
| 已用版本封存 | `archive/baseline-20261102-230617/`：主机成品、双端配置/当前完整 hashlog、源码工作区和 Git bundle；未编译、重启、提交或 OTA |

环境：MSVC 19.44（cmake VS 生成器直连，cl 不在 PATH）、python 3.14、Npcap 运行时、
`thirdparty/npcap-sdk` + `thirdparty/ninja` 已 vendor。当前产品接线：PC2=192.168.2.59 被采集，
PC1=192.168.2.100 接收；历史报告中的 PC1 sender / PC2 receiver 不代表当前角色。主机 ROOT 为 `dist/VirtualCapture`，
采集后台 ROOT 为 `C:\Users\s1973\Documents\VirtualCapture`，端口 5143；没有变更 IP、网卡或运行配置。

## 1. 冻结契约（改即全线返工，须 owner 批准）

### 1.1 包头与链路
- `proto/vc_wire.h`：`vc_pkt_hdr_t` 28B，EtherType `0x88B5`，`payload = mtu - 42`，
  `total = ceil(B/payload)`，`B=width*height*3/2`；缺省仍为614400。P6-2 控制面显式协商几何/MTU/源MAC，不占用包头/flags。proto/ 归 owner 锁定。

### 1.2 构建
- 根 `CMakeLists.txt` 编排 `common/`、`sender/`、`receiver/`、`vcam/` 与 `tools/`，MSVC 优先；统一规则在 `cmake/`。默认 `-B build` 下发收端入口仍为 `build/vc-sender.exe`、`build/vc-receiver.exe`；各目标路径见 `docs/runbooks/build.md`。
- 一键便携包：`python scripts/package.py [--allow-dirty] [--output <目录>]`；默认 `dist/VirtualCapture/`，包含 7 个原生目标、冻结 agent（含 GUI）、MSVC 运行库、脚本和文档。生成文件 SHA-256 与源码来源清单；成功才替换，旧成品整目录归档。契约见 skill `references/packaging.md`。

### 1.3 CLI（动词级冻结；dda/batch 为已落地的追加，本次正式纳入）
```text
vc-sender.exe --adapter <子串> --mode pattern|file|dda|wgc --pattern motion --fps 240
  --frames 0 --ethertype 0x88B5 --mtu 9000 --pacing-us 0 --batch 69 --dst-mac ff:ff:ff:ff:ff:ff
  --session <id> --stats <path> --hashlog <path> [--file <nv12> --loop] [--list-adapters] [--selftest] [--gpu-convert]
  [--output 0 --wgc-window <标题子串> --wgc-no-cursor]
  [--width 640 --height 640 --transform crop|fit|fill|stretch --source-mac <MAC>] [--capabilities]
vc-receiver.exe --adapter <子串> --ethertype 0x88B5 --mtu 9000
  --session <id> --stats <path> --hashlog <path> --timeout-s 0 [--expect-frames N] [--shm <mapname>] [--selftest]
  [--width 640 --height 640 --fps 240 --source-mac <MAC>] [--capabilities]
```

P6-3 MJPEG共同CLI追加：`--codec mjpeg --codec-backend turbojpeg:cpu --codec-hardware 0 --gop 1 --bitrate <bps> --max-bitrate <bps> --decode-in-flight N --max-au-bytes N --jpeg-quality 1..100`；另有`--codec-backends/--codec-check/--au-selftest/--mjpeg-selftest`。压缩载荷/API/日志契约见skill `references/encoded-stream.md`，28B包头不变；Raw CLI和判据不变。`--codec h264|hevc` 在打开网卡前以退出码2拒绝。

WGC开发追加（2026-11-13）：`--mode wgc` 使用 Windows Graphics Capture 采集 `--output` 指定显示器（默认0），`--wgc-window <标题子串>` 改为按窗口采集；窗口模式默认包含光标，`--wgc-no-cursor` 关闭。WGC 仍经 DWM 合成，不消除合成延迟；独占全屏不可采集；失败显式报错，不回退 DDA。DDA 同样支持 `--output`。不改变 28B 包头、raw 逐像素判据与 `--gpu-convert` 语义。

### 1.4 文件型 IPC
- stats ~500ms 原子覆写（tmp→rename）；hashlog 每帧 append 一行。
- `sender.hash.jsonl`：`{"frame_id":N,"hash":"<16hex>","qpc_ns":T,"send_qpc_ns":S}`；T为采集/生成开始，S为开始分包发送
- receiver hashlog D3 carries qpc_ns; reconcile compares frame_id and hash only.
- vcam DShow 设备契约（2026-10-25）：FriendlyName `VirtualCapture`，pin `Capture`（PIN_CATEGORY_CAPTURE），仿 GC573 枚举为 `(video)`；HKCU CLSID 键下可选 REG_SZ `FrameMap`、REG_DWORD `Motion`=1（D12 显式 bring-up）。
- receiver `--shm <name>` (P4-3): publish latest complete frame to named file mapping (layout vcam/vcam_shm.h, NOT proto/); stats adds `shm_published`; agent /receiver/start forwards `shm`.
- **shm v2（D15 历史；开发版由下述 P6-2 v3 替代）**：`vcam/vcam_shm.h` 为唯一布局（magic `VCA2`，12 槽 ring，数据区 4096 起）；receiver 原地重组进空闲槽，发布=翻槽 seq + 置命名事件 `<name>.evt0/.evt1`（manual-reset 交替），整帧哈希在发布后由哈希线程写入（`hash_fid==frame_id` 才有效）；vcam 事件驱动（新帧立即交付，静默 1.5 帧后才重复上一帧，D12 不变）。v1 读端遇 v2 映射判"无信号"，不崩。Python 镜像 `scripts/vc_shm.py`，monitor 使用公共布局。
- vcam→receiver 回写环 `<name>.vcam`（`vc_vstat_*`，仅测量）：receiver stats 新增 `vseg_avg_ms/vseg_p50_ms/vseg_p99_ms/vseg_max_ms`（receiver 发布→vcam 交付下游，同机 QPC，不含重复帧）、`vcam_delivered`、`vcam_repeats`、`hash_queue_max`；sender stats 新增 `cap_lag_avg_ms/cap_lag_p99_ms/cap_lag_max_ms`（dda：DXGI LastPresentTime→转换完成，同机 QPC）。
- receiver hashlog `qpc_ns` = 重组完成时刻（原为当次循环起点，差 <1 包）；sender PC1 段不再含整帧哈希（哈希移到末包之后）。历史 D15 的 DDA 不限速由 P6-2 替代：发送开始间隔至少 ceil(1e9/fps) ns、不补发追赶；timeout/鼠标更新/重复 LastPresentTime 不发送或递增frame_id。
- sender stats 新增 `gpu_dispatch_ms`（D3D11 timestamp 包住 CopyResource 的真 GPU 时间均值）+ `gpu_dispatch_samples`（有效样本数）；`gpu_ms` 仍为 capture() wall-clock，待 P4-1 实测分布后按 D10 决定是否切换口径。
- sender stats 新增 `gpu_convert`（0/1，采集走 CPU 还是 GPU compute shader；GPU 失败显式报错，不静默回退）。
- P4-2步骤1（免协议改动）：receiver stats 新增 `rseg_avg_ms/rseg_p50_ms/rseg_p99_ms/rseg_max_ms`（R(N)=首包到达→重组完成，同机QPC）；sender stats 新增 `pc1_seg_avg_ms/pc1_seg_p99_ms/pc1_seg_max_ms`（PC1段=采集/生成完成→末包发出，同机QPC）；确认包（flags bit1）仍待 owner 终审后做步骤2。
- P5 GUI（D16）：agent 新增 `/ui`、`/config`（ROOT/vc-config.json）、`/vcam/status|register`、`/product/{start,stop,status}`、`/product/local/{start,stop}`；`/sender/start` 可选 `gpu_convert`。详见 agent-api.md。
- P5-U2（D17）：仅主机（默认 `--role host`）提供 UI，收端角色固定；被采集机 `--role capture` 仅后台。产品控制端口 5143，UI 只填 IPv4，网卡自动枚举后下拉选择；主机下发采集参数，对端按控制路由匹配网卡。默认 SHM 固定隐藏，接收前注册 vcam，停止/失败/子进程退出时回收；开始/停止采集合为一个按钮。API 追加能力标记、网卡详情与生命周期语义，详见 agent-api.md；数据面与判据不变。
- P5-U2 后续精简：连接配置仅显示 IP、网卡和目标帧率，移除画面来源、像素转换、发送批大小及 MTU；隐藏参数沿用后台配置，保存时保留原值，底层 API 与默认值不变。
- P5-U3 后端整合：产品启动约 1 秒存活检查，提前退出失败并回滚；stats 追加进程诊断和实际采集方式，界面等待首帧。`/health.root` 支持启动器校验同目录/同角色复用；主机与采集后台各有便携启动入口。契约见 agent-api.md、packaging.md。
- P6-1（D19）：配置新增输出宽高、crop/fit/fill/stretch、raw/mjpeg、目标/最大码率与链路预留余量；旧配置仍为 640×640 / crop / raw。新增只读 `GET /stream/capabilities` 与草稿 `POST /stream/plan`，`/adapters` 追加数值型收/发链路速率，`/health.stream_revision=1`（历史，现已由P6-2的2替代）。该阶段的非640/缩放拒绝由P6-2解锁；编码模式仍在注册/启动前409拒绝；禁止低层接口静默忽略新流参数。预算使用 mtu-42 和完整 L2 线速开销，不是实测吞吐或帧率保证；速率未知不冒充可行。完整契约见 skill `references/stream-config.md`。
- P6-2（D19）：`stream_revision=2`、显式 descriptor 双端回显、实际原生能力探测与源 MAC 会话过滤；旧后台/二进制拒绝。可变尺寸 Raw、整数 crop/fit/fill/stretch、发送帧率上限已接通。SHM v3（VCA3，header offset32/36/40=width/height/fps，动态12槽步长）；新读端兼容固定v2，旧读端拒绝v3，writer拒绝已有同名映射。DShow 创建时固定一档真实会话格式，改尺寸先关下游再重新开始/打开；D12不变。详情与低层API见 skill `references/stream-config.md`。
- 哈希 FNV-1a64（初值 14695981039346656037，prime 1099511628211，小写 16hex），整帧 B 字节（旧默认614400B）。

- P6-3开发编码（仅MJPEG）：codec_revision=1、mjpeg_revision=1；控制面显式`turbojpeg:cpu`/硬件类别false/GOP1/码率/队列/内存上限及开发授权。48B VCAU只在载荷内，压缩长度动态分片，CRC+整体压缩hash；独立有界CPU解码、每帧独立恢复、EOS/drain，解码NV12继续SHM v3。sender记source_hash/transport_hash，receiver另记transport与decoded_hash，不套用Raw reconcile。只读codec-check与GUI能力、状态及完整契约见skill `references/encoded-stream.md`。
- P6-3后续诊断支持显式自定义尺寸/fps/变换、CPU/GPU及后端类别、有界测试时长，区分发送fps/源hash/解码hash/DShow重复；decoder另请求CODECAPI低延迟属性，但HEVC单帧上限仍失败，不自动加深。完整参数与同机计时范围见skill `references/encoded-stream.md`。
- P6-3性能修正：严格截止限速、有界分型准备；追加最近10000帧timing_ms及逐帧encode_wait_ns，保持encoder1帧/decoder显式上限。编码sender沿用Raw发送线程调度设置（MMCSS Capture/TIME_CRITICAL、进程HIGH），不改系统全局/GPU优先级或编码参数。编码receiver首包改取Npcap返回后，修正旧decode_pipeline_ms含读取等待的偏差，不作为解码提速。指标/等待语义见skill `references/encoded-stream.md`；H.264/H.265相关实验报告已按D23删除。
- P6-3帧龄优化：准备预测改为请求IDR/非IDR各最近64次nearest-rank p95+0.1ms、最多一个周期，原帧率/码率闸及队列上限不变；stats追加preparation，逐帧日志追加prepare_lead_ns/prepare_work_ns，专项脚本重放验证预测，完整口径见skill encoded-stream。不把低帧龄冒充无代价吞吐改善。
- P6-3 MJPEG（D21）：显式turbojpeg:cpu CPU/SIMD、JPEG质量1..100（默认80）、GOP1、每帧独立；sender/receiver和descriptor追加mjpeg_revision=1与jpeg_quality，Raw会话字段不变。VCAU codec=3只扩展48B载荷语义，28B包头不变；原码率上限/队列与SHM/DShow不变。目标码率不应用于JPEG，不自动降质；精确能力/配置确认及帧21恢复规则见skill encoded-stream，构建依赖/许可证见docs/runbooks/mjpeg.md。
- MJPEG内存优化只调整本地缓冲生命周期：本帧借用JPEG输出、复用封装/分片/按需增容sendqueue、压缩hash不重复扫描，直接解码到未发布帧槽；错误时abandon，成功才publish。CRC/全部逐帧哈希、固定质量与原队列/限速/载荷/IPC均保留；audit_hash指标语义与故障门见skill encoded-stream。

### 1.5 全局禁止
1. 不改包头尺寸/字段顺序。 2. 像素生成禁用浮点。 3. 原有 raw 模式禁止压缩；D21 批准的 MJPEG 为开发有损模式（H.264/H.265 已按 D23 移除），不得复用 raw 逐像素哈希门来声称有损正确性。
4. 性能宣称必须用 `motion` 并注明 pattern/batch/adapter。 5. 跨机不报 QPC 差值延迟，只报 inter-arrival。
6. 不许拿回环数据冒充双机结论。 7. 不许放宽 test-matrix 判据（改判据只能经 D1）。
注：UNORM 解码允许 float（B8G8R8A8_UNORM→float→floor(v*255+0.5) 属格式转换，非像素生成；结果必须和 CPU 参考逐字节一致，dda_gpu 自测已证）；不许拿它当先例去别处用 float。

### 1.6 目录
以 `AGENTS.md` Repo map 为准；产物与运行数据全部 gitignore；任务只碰自己的目录。共享 CRC32/FNV 统一在 `common/`，原生辅助工具源码归 `tools/`；`proto/vc_wire.h`、`sender/pattern.h`、`vcam/vcam_shm.h` 的契约位置保留。

### 1.7 PC1 Raw 长运行诊断（2026-11-08 增量）
独立 `scripts/longrun_diagnostics.py` 默认每秒/6小时，只 GET 双端 stats、观察系统/下游资源并留全量样本；`--mark/--stop` 只标记/结束记录器。候选 Raw receiver/vcam 通过进程环境 `VC_DIAG=1` 提供固定有界 `Local\VirtualCapture.Diag.<pid>` 旁路计时（stats准备/写盘、hash写盘、GetBuffer、Receive）；不改现有 wire、帧 SHM、rseg/vseg 语义或验收门限。契约见 skill `references/longrun-diagnostics.md`，操作见 `docs/runbooks/longrun-diagnostics.md`。

## 2. 决策记录与待决事项

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

## 3. 任务卡

当前优先：P6 分阶段开发；P5 已用版本封存保留，G3/G4 暂不推进。P4 卡保留历史完成证据；改代码必跑 `python scripts/verify.py`，不重建/替换正在使用的 dist 成品。

### 已关闭（原文见 docs/history/phase3-closed-cards.md）

| 卡 | 结论 | 证据 |
|---|---|---|
| P1-1 止血 | DONE | f128e03 / 67e8ce5；live 用例 VC_ADAPTER=Loopback 通过 |
| P1-2 构建+git+治理 | DONE | docs/runbooks/build.md；run/ 顶层 <60 |
| P1-3 文档同步 | DONE | e0483fb；2026-10-23 再整理为 AGENTS.md + docs/ 分层 |
| P2 0.3.8 + T-06 | DONE | 810f11d；docs/bringup/bringup-20261022.md（备注：T1×5 离线对账仅覆盖头部 ~27%，判定依据 PC2 in-run 全量校验；T2 已分页补全） |
| P3 OTA 0.4.0 | DONE | e01af52 / ac0e2b4 / ae44cbc；PC2 推→回滚→再推全绿 |

### 遗留观察 / 小改动（不阻塞 P4）

- **P5-LR 实现/隔离验证 DONE（2026-11-08）**：PC1 Raw 独立每秒/6小时采样、可选旁路阻塞计时、候选生命周期/Stop/Disconnect 修正；最终 verify 含 Loopback 408 passed / 2 skipped，六组 motion 各 720 帧零缺失/错哈希。已用成品87项哈希未变；现场6小时记录已启动，实际长时卡顿根因**仍待复现关联**。证据：`docs/bringup/bringup-20261108-longrun.md`。

- stop 记来源 IP：agent 侧已提交（d15287d，写 run/exec.log），随下个版本上 PC2。
- /activate 应答与 execv 竞争导致 RST，调用方靠轮询 /version；可改为先回应答再 execv。
- T2 重跑盯：发端 203.5ms 间隔（batch69 后首次 >20ms）、外部 stop 是否复发。
- PC2 当前已为 capture 后台（2026-11-02 /health）；计划任务/watchdog 是否覆盖 Documents 部署仍未核实，本轮不调整启动方式。
- 直连线：owner 决定短期不考虑；线到后补 广播 vs 单播 T2。

### S1 项目结构整理 — DONE（2026-10-29）

`common/` 合并重复校验头、`tools/` 收纳原生辅助工具、`cmake/` 统一规则；verify ALL PASS（28 passed / 3 skipped）+ 独立目录干净构建通过。证据：`docs/proposals/project-structure-20261029.md`。

### S2 一键编译打包 — DONE（2026-10-29）

`python scripts/package.py --allow-dirty` 实际生成 `dist/VirtualCapture/`（含当前 GUI）；verify ALL PASS（42 passed / 3 skipped）+ 成品隔离冒烟/PowerShell 5.1 检查/79 项载荷哈希通过。证据：`docs/proposals/packaging-20261029.md`。

### P5-U1 GUI 优化 — DONE（2026-10-29）

总览/连接配置改版，修复编辑覆盖、离线/桩进程指标与会话帧率基线；verify ALL PASS（46 passed / 4 skipped），桌面/窄屏及隔离双端交互通过。证据：`docs/proposals/gui-20261029.md`；当时未打包，后续已并入 U3 成品，当前来源核对见 `docs/bringup/bringup-20261102.md`。

### P5-U2 主机 UI 与采集后台 — DONE（2026-10-29）

落实 D17：主机专属 UI、固定 5143/纯 IP、网卡下拉选择、自动虚拟卡生命周期及统一开始/停止按钮；配置精简为 IP/网卡/目标帧率。verify ALL PASS（58 passed / 4 skipped）+ PowerShell 5.1 解析与隔离浏览器保存验证通过。证据：`docs/proposals/gui-host-20261029.md`；该轮仅隔离验证，后续打包与使用状态见 U3/U4，不回填未经专项验证的注册表结论。

### P5-U3 后端与完整项目整合 — DONE（本机实现 2026-10-29；实际使用确认 2026-11-02）

主机/后台真实编排、启动失败回传、实际会话状态与两个启动入口已接通；2026-10-29 本机 verify 含 Loopback live：65 passed / 2 skipped。motion 26957 帧、DDA 7431 帧收发全量对账零缺零错，两轮 SHM 各 20/20 一致，DShow 实际取帧通过。证据：`docs/proposals/gui-integration-20261029.md`。截至 2026-11-02 两端已上线，owner 约三天实际使用满意；专项自动化双机验收不据此补判通过。

### P5-U4 已用版本封存 — 主机 DONE / 远端二进制待独立导出（2026-11-02）

主机成品 87/87 哈希通过，封存前源码 299/299 与成品记录一致；双端清单/配置/最新完整 hashlog、源码及 Git bundle 已保全，原文件不动。证据：`docs/bringup/bringup-20261102.md`；远端仅文本接口可读，16 项 PE 文件只核对存在与大小，不冒充实际字节备份。历史 dropped=1 保留观察，不阻塞日常使用，也不改为零丢帧通过。

### P6-1 GUI 配置与链路预算 — DONE（2026-11-03）

五档预设 + 自定义、四种裁切/缩放草案、raw/MJPEG 规划、两端方向速率及 L2 线速预算；verify ALL PASS（122 passed / 4 skipped）+ Edge 隔离桌面/手机宽度 13 项检查通过。原生/包头/SHM/部署未修改，未实现组合拒绝启动；稳定成品 87/87 与封存 424/424 完整性复核通过。证据：`docs/proposals/configurable-stream-20261102.md`。

### P6-2 可变尺寸 raw 全链路 — DONE（开发实现与本机验证，2026-11-03）

显式会话/真实原生能力、可变尺寸Raw、四种整数变换、发送上限、SHM v3及动态DShow已打通；motion回环七组1665帧、DDA/GPU三组300帧全量对账零错，产品HTTP→真实原生→DShow 121/121一致。双机与交付留P6-4。证据：`docs/bringup/bringup-20261103-p6-raw.md`（含总闸与生命周期负例）。

### P6-3 编码能力与后端（MJPEG / PyroWave） — IN PROGRESS（2026-11-03；2026-11-13 更新）

H.264/H.265（Media Foundation 与原生 NVENC）于 2026-11-13 按 D23 从源码、构建、契约与测试中移除，
相关报告、提案与开发包一并删除；不再提供 `vc-codec-probe`/`vc-codec-roundtrip`。当前编码为 D21 的 MJPEG
与 D24 的 PyroWave（均为开发版、每帧独立）。

MJPEG开发实现与本机专项 — DONE（D21）：libjpeg-turbo CPU/SIMD、独立帧恢复、质量配置及GUI/产品贯通；总闸含Loopback401 passed / 2 skipped。640² motion/batch1/Loopback两轮7200帧，239.86～239.97fps、生成至DShow首次交付avg1.67～1.73ms/P99约2.0～2.2ms，Q80约141Mbps（显式800Mbps上限）；真实本机DDA仅34.5fps，未证明PC2/DDA240或双机延迟。完整证据与独立测试包入口见`docs/bringup/bringup-20261103-mjpeg.md`；不自动替换部署，P6-3总体仍IN PROGRESS。

MJPEG缓冲优化 — DONE（本机实现/验证，2026-11-03）：去掉发送临时JPEG复制与接收整帧复制、复用缓冲及压缩hash；403 passed / 2 skipped含Loopback。交叉31200帧全部收发/首次交付且新旧压缩输出与解码像素一致；240档avg1.67～1.69→1.64ms、60档1.80→1.73～1.74ms，P99区间重叠、总CPU收益未证实，不宣称大幅提速。11项专项及真实sender/receiver的无网卡故障注入通过；证据/独立包结果见`docs/bringup/bringup-20261103-mjpeg-opt.md`，不切换部署。

PyroWave（D24）— 开发实现与本机功能验证 DONE（2026-11-13）：固定提交构建 `libpyrowave-shared-1.dll`，
capabilities/codec-check/descriptor、48B VCAU（codec=4）、有界解码与 GUI 后端选择接通；`verify.py` ALL PASS。
640² motion/batch1/Loopback 120fps 120/120、30fps 60/60 解码，零丢包/零传输错/零解码错，压缩 hash 与 sender 全量一致；
`p6_codec_verify.py --codec pyrowave` 含 DShow 全对账与 `--loss` 帧21恢复均 PASS；
`p6_product_verify.py --codec pyrowave` 隔离HTTP→真实原生→DShow（418×282）117/117 通过（同时修复该脚本 raw 分支被误拒）。
期间发现并修复：流断档不再重建 Vulkan 设备（只重建解码器对象），否则设备创建慢于帧周期会触发队列丢弃死循环。
240fps 同机双端因发送线程抢占出现解码队列丢弃（114），不据此判定编解码吞吐，也不放宽队列。
未做 PC2 部署/双机/长稳/画质；证据与探针数字见`docs/bringup/bringup-20261113-pyrowave.md`，
构建/限制见`docs/runbooks/pyrowave.md`。

### P6-4 组合验收与交付 — TODO

raw / 编码分开验收，motion、尺寸/帧率/模式/网卡/batch 全记录；双机证据与完整 hashlog 保全。未专项验收的组合不标已达标，owner 确认后再安排发布/切换，稳定版可回退。

### P4 产品链路（按 docs/spec/requirements.md；提案与批示见 docs/proposals/）

```text
P4-1 采集性能：DONE（代码）
     gpu_dispatch_ms=0.028ms（D3D11 timestamp，双机 DDA 235fps 实测，2026-10-23）。
     GPU compute shader（居中裁+整数BT.601）落地，--gpu-convert 显式开启，失败报错不回退。
     待 owner 决策：①gpu_convert 设为默认开（D10）；②T5 门限写入 test-matrix。
P4-2 端到端延迟测量：DONE（分段计时代码）
     双机实测：pc1_seg_p99=3.14ms，rseg_p99=6.70ms，全程 p99=9.83ms（batch=69，D14 网卡调优后）。
     batch 对比（优化后）：batch=69 最优（全程 9.27ms，rseg_max=9.9ms 最稳定）。
     待 owner 决策：①全程 p99 门限拍板（建议 <10ms）；②确认包（flags bit1）是否实现。
P4-3 PC2 虚拟采集卡：DONE（代码+本机验证）
     vcam DShow filter 落地，本机回环 4455 帧逐字节零错，240.0fps QPC 起搏。
     D12 无信号默认行为实现（7e7ce80）：无映射→stall，撕裂+有历史帧→repeat-last，正常→输出。
     OTA 白名单已放行 vc-vcam.dll；验收手册：docs/runbooks/pc2-vcam-acceptance.md。
P4-4 虚拟卡出口验收（不含推理）：DONE 2026-10-23
     J1 PASS：vc-vcam-verify.exe（tools/vcam_verify/ C++）ICreateDevEnum 枚举到 VirtualCapture 640x640 NV12（2026-10-25 改名为 `VirtualCapture`，见 docs/runbooks/vcam.md）。
     J2 PASS：IAMStreamConfig GetFormat = NV12 640x640 AvgTimePerFrame=41666 (240.0fps)。
     J3 PASS：35069帧 shm_published==complete, dropped=0, hash_mismatch=0；SHM FNV-1a64 采样 27/27 与 receiver hashlog 一致。
     验收工具：build/vcam_verify/Release/vc-vcam-verify.exe + scripts/p4_4_shm_verify.py。
     证据：docs/bringup/bringup-20261023-p4-4-vcam-acceptance.md。
P4 原范围外：推理端及其联调（D8）、独占全屏 Hook。原“超 2.5G 后编解码另立项”（hist S24）已由 D19 解锁为 P6，非 P4 历史验收的一部分。
owner 待决：①全程延迟门限 <10ms；②gpu_convert 默认开；③确认包是否实现；④T5 门限数字。
```
