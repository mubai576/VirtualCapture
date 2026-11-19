# Rust 控制面延迟差异审计（2026-11-14）

owner 反馈切换到 Rust 客户端后实测延迟约增加 10ms。本记录只做静态差异审计与可复现实验设计，
**没有独立复现该 10ms**，也不据此修改数据面、限速或网卡参数。审计期间未访问或停止 PC2，
未注册 DShow。

## 结论先行

Rust 控制面**没有替换数据面**：`vc-sender.exe`、`vc-receiver.exe`、`vc-vcam.dll` 仍是原 C++
二进制，28B 包头、分包、发送调度、SHM v3、DShow 交付代码都未改。当前工作区 `build/Release`
与已交付绿色包中的这三个文件 SHA-256 完全一致：

| 文件 | SHA-256（构建与包内一致） |
|---|---|
| `vc-sender.exe` | `52c5b3b27fb20e3c7f0f71e15e000b9f53828ebf5ef5ac2d225c6e3b5da012e9` |
| `vc-receiver.exe` | `7c56810fdae8217e1b5de5823b7c0f1c06edae7ba3c102e8226b25a1c2522805` |
| `vc-vcam.dll` | `a76bd2d8ca5f72eee469bd883d105ebc851a42823ed4210477cd4932fc862496` |

用户实测的约10ms增加作为待定位回归保留。当前已核对的静态数据路径与默认参数没有直接解释该差异，
仍需检查实际设备、会话参数与同机分段；GUI负载、进程生命周期和驱动调度也不能仅凭源码一致排除。
以下比对仅覆盖默认命令行，不代表两机现场配置或运行负载已相同。

## 控制面下发的参数比对

Rust `vc-core::process::sender_args` 与旧 `agent/vc-agent.py` 的 `launch_product` 对照：

| 项目 | 旧 Python 后台 | Rust 控制面 | 是否等价 |
|---|---|---|---|
| 发送端模式/图案 | `mode`、`pattern`（默认 dda/motion） | 同 | 是 |
| fps / frames | `fps=240`、`frames=0` | 同 | 是 |
| MTU / EtherType | 9000 / `0x88B5` | 同 | 是 |
| `--pacing-us` | 默认 0 | 固定 0 | 是 |
| `--batch` | 默认 69 | 默认 69 | 是 |
| `--dst-mac` | 配置值 | 由主机按接收网卡 MAC 生成 | 语义一致，值可能不同 |
| `--gpu-convert` | `gpu_convert=true` 时追加 | 同 | 是 |
| 输出显示器 | 0 时省略 | 0 时省略 | 是 |
| 几何/变换 | 有会话描述符时下发 width/height/transform | 每次显式下发 | 是 |
| `--source-mac` | 由会话描述符下发 | 每次随机本地管理 MAC | 是 |
| 进程创建 | `CREATE_NEW_PROCESS_GROUP`，cwd=ROOT，stdout 写日志 | 同 | 是 |
| 接收端参数 | adapter/ethertype/mtu/session/stats/hashlog/timeout-s/shm/width/height/fps/source-mac | 同 | 是 |

未发现 Rust 侧新增的每帧处理：GUI 350ms 快照刷新、后台 1s 统计轮询只读 stats 文件/HTTP，
不进入收发线程；启动前的网卡枚举（原生 10s + PowerShell 15s）发生在 `prepare/start` 之前。
这些静态事实不能排除同机GUI负载、进程调度或驱动资源竞争造成的间接影响。

## 仍需核实、可能造成 +10ms 的候选

1. **接收网卡不是同一条 2.5G 链路**：Rust 默认按控制连接自动选卡；若 Wi-Fi/其它网口也能路由到
   被采集机，可能选到 WLAN。请确认两端实际 `adapter` 与速率一致，必要时手动固定接收网卡。
2. **对比基准不是 Raw**：D25 记录 PC2 一度切到 PyroWave。若“旧版”测的是另一套编码/尺寸/帧率，
   与 Rust 当前 Raw 640×640@240 不可直接比较。
3. **测量口径不同**：Rust GUI 展示接收端 `inter_arrival_p99_ms`；历史结论中的 9.83ms 是
   `rseg`/全程分段口径。两者不是同一个量，不能混用。
4. **网卡调优未复现**：D14 的 FlowControl/ITR/EEE/InterruptModeration 是逐机逐卡设置；换机、换端口、
   驱动更新后需按 `docs/runbooks/pc2-nic-tuning.md` 重新确认。
5. **配置差异**：MTU、batch、gpu_convert、输出显示器、变换或 SHM 名称不同都会改变延迟或导致下游异常；
   Rust 使用独立 `vc-rust-config.json`，不会继承旧 `vc-config.json`。
6. **二进制来源未知**：包内原生文件与本工作区构建一致，但未与当前部署机上的原生文件逐字节比对；
   若部署的是更早构建，行为差异需单独核对。

## 建议的受控 A/B

同一对机器、同一网线/网口、同一 `motion` 源：

1. 固定两端 `adapter` 为 2.5G 物理网卡，记录 `--list-adapters` 的速率。
2. 配置统一：640×640、240fps、batch 69、MTU 9000、Raw、crop、gpu_convert=true。
3. 旧 Python 后台与 Rust 控制面各跑 60s，各自保留 stats/hashlog。
4. 只比较同机分段：发送端 `pc1_seg_p99`、接收端 `rseg_p99`、`vseg_p99`、`inter_arrival_p99`；
   不比较跨机 QPC 差值。
5. 判据沿用 `references/test-matrix.md`，不因本次反馈放宽；差异 >1ms 且可重复再定位到具体分段。

本轮记录为“用户反馈切换后约增加10ms，尚未独立复现/定位”。缺少现场同配置A/B证据时，
不宣称已修复或排除该回归，不为得到更好数字而静默调整发送调度、帧率或限速。
