# P5 功能清单 + GUI 整合提案（2026-10-28，待 owner 批 D16）

## 1. 现有功能清单（按 a987b00 盘点）

拓扑（当前实际）：采集机 = PC2 192.168.2.59（Realtek 2.5G，跑 sender）→ 接收机 = PC1 192.168.2.100（I226-V，跑 receiver + vcam）。
两台都跑 `vc-agent`（REST :18080），角色只是显示标签。

### A. 数据面（C++，已冻结 CLI，PLAN §1.3）
| 编号 | 功能 | 组件 | 入口 / 关键参数 |
|---|---|---|---|
| A1 | 测试源：counter/checker/rings/noise/motion（整数、可复现） | vc-sender | `--mode pattern --pattern <p> --fps` |
| A2 | 文件源：NV12 原始帧文件，可循环 | vc-sender | `--mode file --file --loop` |
| A3 | 桌面采集：DXGI DDA 居中裁 640，CPU 或 GPU compute 转 NV12 | vc-sender | `--mode dda [--gpu-convert]` |
| A4 | L2 发送：0x88B5，MTU 9000，69 包/帧，sendqueue 批发 | vc-sender | `--adapter --dst-mac --batch --pacing-us` |
| A5 | 重组 + CRC + 整帧 FNV 校验 + 测试源比对 | vc-receiver | `--adapter --expect-frames --timeout-s` |
| A6 | 发布到共享内存（shm v2，12 槽 + 事件） | vc-receiver | `--shm Local\VirtualCaptureVcamFrame` |
| A7 | DShow 虚拟采集卡 `VirtualCapture`（NV12 640x640@240，D12 无信号不出帧） | vc-vcam.dll | regsvr32；HKCU `FrameMap` / `Motion` |
| A8 | 统计：stats.json（500ms 原子覆写）+ hashlog（逐帧） | sender/receiver | 字段见 agent-api.md |
| A9 | 分段延迟：pc1_seg / rseg / vseg / cap_lag / gpu_dispatch | sender/receiver/vcam | 同机 QPC，不跨机比 |
| A10 | 自检：`--selftest`（图案、分片、CRC、重组、DDA、GPU 逐位一致） | sender/receiver | |

### B. 控制面（vc-agent.py 单文件，REST）
| 编号 | 功能 | 端点 |
|---|---|---|
| B1 | 健康 / 版本 / 网卡列表 | `/health` `/version` `/adapters` |
| B2 | 起停发端、收端，读实时统计 | `/sender/*` `/receiver/*` `/receiver/report` |
| B3 | 白名单远程执行（脚本、诊断命令） | `/exec` `/exec/log` |
| B4 | 远端文件分页读取（hashlog 拉回） | `/files` |
| B5 | OTA：stage → activate → 回滚 | `/update` `/activate` |
| B6 | 抓包 | `/pcap/start` |

### C. 编排 / 验收（Python + ps1）
| 编号 | 功能 | 入口 |
|---|---|---|
| C1 | 双机一键测试 + 判分（T0–T5，test-matrix.md） | `ctrl/vc-ctrl.py`、`ctrl/bringup.py` |
| C2 | 收发 hashlog 逐帧对账 | `ctrl/reconcile.py` |
| C3 | 远控 / OTA 推包 | `ctrl/run_remote.py`、`scripts/update.ps1` |
| C4 | vcam 验收 J1/J2/J3 | `vc-vcam-verify.exe`、`scripts/p4_4_*_verify.py` |
| C5 | 本地总闸（build + selftest + pytest） | `scripts/verify.py` |
| C6 | 装机 / 网卡调优 / watchdog | `install-pc2.ps1`、`set_nic.ps1`、`tune_pc1_nic.ps1`、`watchdog.ps1` |
| C7 | vcam 注册（HKCU） | `scripts/vcam_register.ps1` |

### D. 观测
| 编号 | 功能 | 组件 |
|---|---|---|
| D1 | NV12 实时预览 + OSD 指标（读 shm） | `vc-monitor.exe`（D3D11） |
| D2 | 终端双机仪表盘（rich） | `ctrl/vc-monitor-tui.py`（依赖 rich，pc1/pc2 默认地址与实际拓扑相反） |
| D3 | 240Hz 动画测试窗口（给 dda 用） | `vc-testwin.exe` |

### 已知缺口（整合时顺带暴露给用户）
- 没有"产品模式"一键启动：日常使用要手动起 receiver（带 `--shm`）+ 远端起 dda sender + 注册 vcam，共 3 步。
- 跨机端到端延迟仍缺（确认包 flags bit1 待 owner 终审，P4-2 步骤 2）。
- `vc-ctrl.py` 不透传 `shm`；TUI 默认地址写反；多处脚本硬编码 IP。
- 运行配置（IP、网卡子串、目标 MAC）散在命令行里，没有统一配置文件。

## 2. GUI 目标
一个窗口完成日常使用和排障：
1. **连接**：两台机器的地址、在线状态、版本，版本不一致时提示。
2. **一键开播 / 停播**（产品模式）：接收机起 receiver(+shm) → 采集机起 dda sender → 确认 vcam 已注册。
3. **实时仪表**：fps、pps、带宽、丢帧/哈希错、各段延迟 p50/p99/max、CPU，带 60s 曲线。
4. **测试**：选 T0–T5 一键跑，出判分报告，hashlog 自动拉回并对账（硬规则 8）。
5. **运维**：OTA 推包/回滚、vcam 注册/注销、网卡检查、看 agent exec 日志。
6. **预览**：启动 vc-monitor（不重写 D3D 预览）。

非目标：不改数据面（proto、CLI、shm 格式不动）；不做推理端（D8）；GUI 不直接调 Npcap，只调 agent。

## 3. 架构选项
GUI 只是 agent REST 的前端，所有动作都走 B1–B6，不新增数据面逻辑。

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| **A. agent 内置 Web UI（推荐）** | vc-agent 多 serve 一个静态页 `/ui`（单个 HTML+JS，随 agent 打包）；新增 `/peer/*` 反代到另一台 agent，避免跨域 | 零安装，任一台浏览器打开即用；随现有 OTA 更新；保持 agent 零依赖；两台机器都能当控制台 | 原生感弱；需给 agent 加反代（只转发到配置的对端 IP） |
| B. 桌面程序 Python + tkinter | 单独 `vc-gui.exe`（PyInstaller），调两台 agent | 标准库、原生窗口；无需改 agent | tkinter 画实时曲线与表格较吃力；多一个 exe 要发 |
| C. 桌面程序 PySide6 (Qt) | 同 B，用 Qt | 界面与曲线最好 | 引入 ~60MB 依赖，打包体积大，破坏"零依赖"惯例 |
| D. 并入 vc-monitor（C++ Win32/ImGui） | 在预览窗口里加控制面板 | 一个 exe，预览+控制合一 | C++ 写 HTTP/JSON/表单工作量最大，迭代慢 |

推荐 A：改动集中在 agent（契约变更：新增 `/ui`、`/peer/*`、`/config`，按硬规则 6 同步 agent-api.md 与 PLAN §1），
预览仍由 vc-monitor 负责。若 owner 要"双击打开的独立软件"，可在 A 之上加一个 WebView2 外壳（Windows 自带运行时），不改前端。

## 4. 分阶段
| 阶段 | 内容 | 验收 |
|---|---|---|
| G0 | 统一配置 `vc-config.json`（两台地址、角色、网卡子串、目标 MAC、shm 名）；agent `/config` 读写；修 TUI/vc-ctrl 默认值 | verify.py 绿；pytest 增配置用例 |
| G1 | 只读仪表：连接状态 + 双端 stats + 延迟曲线 | 双机 motion 跑 60s，页面数值与 stats.json 一致 |
| G2 | 一键开播/停播 + vcam 注册状态 | 从停机状态一键到 ffmpeg 能开 `VirtualCapture` 收流 |
| G3 | 测试页：T0–T5 + 对账 + 报告（复用 vc-ctrl/bringup 逻辑，改为库调用） | 与命令行同参数判分一致 |
| G4 | 运维页：OTA、回滚、exec 日志、网卡检查 | OTA 往返 pytest（test_ota）仍绿 |

## 5. 需要 owner 决定（D16）
1. 技术方案：A（推荐）/ B / C / D；是否要 WebView2 独立窗口外壳。
2. 首批范围：建议 G0+G1+G2（日常使用闭环），G3/G4 后续。
3. 控制台在哪台机器打开：两台都可（A 方案天然支持）还是只在接收机 PC1。
4. 访问控制：agent 现在无鉴权，`/ui` 和 `/peer` 是否只绑定直连网段 / 加简单口令。
