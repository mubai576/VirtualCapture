# 开发工具与脚本入口

目录职责以根 `AGENTS.md` Repo map 为准。本页按操作找入口；命令均在仓库根目录执行。
双机测试与部署会操作真实设备，参数和前置条件按各自 runbook 核对。

## 日常开发

| 操作 | 入口 |
|---|---|
| 一键编译验证并打包完整目录 | `python scripts/package.py`；确认包含未提交改动时加 `--allow-dirty` |
| 本地完整验证 | `python scripts/verify.py` |
| GUI 前端状态逻辑 | `python -m pytest ctrl/tests/test_gui_client.py -q`；需要开发机有 Node.js，无 npm 依赖，agent 运行不需要 Node.js |
| MJPEG缓冲生命周期与故障清理 | `python -m pytest ctrl/tests/test_mjpeg_buffers.py -q`；MSVC编译实际sender/receiver，注入内存Npcap替身，不操作网卡/注册表；需先有默认MJPEG原生构建 |
| 主机/后台与虚拟卡生命周期 | `python -m pytest ctrl/tests/test_gui_host.py -q`；使用进程与注册替身，不操作真实注册表 |
| P6 Raw尺寸/发送上限/真实DShow | `python scripts/p6_raw_verify.py`；`--dda`需变化桌面，私有映射，不注册 |
| P6 MJPEG传输/解码/真实DShow | `python scripts/p6_codec_verify.py --codec mjpeg --encoder turbojpeg:cpu --decoder turbojpeg:cpu`；`--matrix`尺寸矩阵、`--loss`丢包恢复；自定义尺寸/fps/变换/DDA/CPU及较长测试见下文；本机隔离，不注册 |
| P6 产品控制端到原生链路 | `python scripts/p6_product_verify.py`；编码加显式 `--codec/--encoder/--decoder`，隔离ROOT/随机HTTP端口，注册替身，其余实际执行 |
| 本机回环 live | `python scripts/verify.py --no-build --live Loopback` |
| 查看构建产物时间 | `python scripts/check_bins.py` |
| Python 测试源自检 | `python ctrl/pattern_ref.py` |
| 双机编排与判分 | `ctrl/vc-ctrl.py`；参数见 `AGENTS.md` 常用命令 |
| 生成联调报告 / 对账 | `ctrl/bringup.py` / `ctrl/reconcile.py` |
| 文本仪表 | `ctrl/vc-monitor-tui.py` |

### P6 编码专项参数

H.264/H.265 及 MF/NVENC 参数已于 2026-11-13 按 D23 移除；脚本只接受 MJPEG：

```powershell
python scripts/p6_codec_verify.py --codec mjpeg `
  --encoder turbojpeg:cpu --decoder turbojpeg:cpu `
  --width 640 --height 640 --fps 60 --frames 1800 --test-seconds 45
```

- `--width/--height`必须成对，不能与`--matrix`混用；尺寸、fps、帧数、测试时长和编码参数先校验，再创建目录/进程。
- `--transform crop|fit|fill|stretch`；DDA加`--dda`，CPU转换须显式`--cpu-convert`，没有自动回退。
- WGC 冒烟：`vc-sender.exe --wgc-selftest`（显示器采集，无窗口）；`--mode wgc [--wgc-window <标题子串>] [--output N] [--wgc-no-cursor]`。仍经 DWM 合成，窗口模式含光标。
- MJPEG两端`--encoder-hardware/--decoder-hardware`固定0；其他编码名在启动前拒绝。
- `--test-seconds`范围3..122，DShow比receiver少观察两秒；frames/fps须严格小于剩余时长，实际较慢仍可能失败。
- 记录源/解码/虚拟卡不同hash数、实际发送fps和receiver首包至解码的同机段；重复末帧不算新画面。
- 性能排查另看发送间隔分布、真实解码fps、sender采集开始至发送开始及最近10000帧分段timing_ms；encode_wait_ns包含调度，不是纯GPU时长。
  编码receiver计时修正为Npcap返回后，旧报告的decode_pipeline_ms含读取阻塞，不能直接对比为解码收益；详见skill encoded-stream。
- 脚本不创建/移动动画窗口，DDA测试须提供变化桌面，且crop区域确实包含变化内容；这不是画质/长稳或双机验收。
- MJPEG：`--codec mjpeg --encoder turbojpeg:cpu --decoder turbojpeg:cpu --encoder-hardware 0`，
  `--jpeg-quality 1..100`默认80，GOP1、解码上限1。
  `--loss`验证丢帧20后从21恢复，不等待GOP。固定质量而非CBR；`--max-bitrate`仍是发送限流，默认80Mbps可能压低实际fps。
  640@240诊断可显式设`--max-bitrate 800`，但不代表所有内容/链路均可承载，也不自动修改产品默认值。
  具体命令、SIMD依赖/许可证及本机合成源与DDA结果见`mjpeg.md`和`../bringup/bringup-20261103-mjpeg.md`。

双机并存与切换门禁见`p6-cutover.md`，须owner确认，不直接套用历史18080割接脚本。

## 原生辅助工具

统一使用根 CMake 构建；完整命令和二进制路径见 `build.md`。

| 用途 | 源码 | 构建目标 |
|---|---|---|
| SHM NV12 预览、OSD | `tools/monitor/monitor.cpp` | `vc-monitor` |
| D3D11 诊断 | `tools/monitor/diag.cpp` | `vc-monitor-diag` |
| DDA 高帧率动画源 | `tools/testwin/testwin.cpp` | `vc-testwin` |
| DShow 枚举 / NV12 格式验收 | `tools/vcam_verify/vcam_verify.cpp` | `vc-vcam-verify` |
| 虚拟卡对象/真实连接图生命周期（直接加载 DLL，不注册） | `tools/vcam_verify/vcam_lifecycle.cpp` | `vc-vcam-lifecycle` |

这三个源码目录于 2026-10-29 从根目录迁入 `tools/`；历史报告中的旧源码路径保留原文。
工具名称、Release 二进制路径均保留，现有启动脚本继续适用。

## 设备运维与专项验收

| 用途 | 入口 |
|---|---|
| 安装 / OTA / watchdog | `scripts/install-pc2.ps1`、`scripts/update.ps1`、`scripts/update_sender.ps1`、`scripts/watchdog.ps1` |
| 被采集机后台启动 | `scripts/start-capture-worker.ps1`；当前登录用户会话隐藏启动，控制端口 5143；主机 UI 用法见 `gui.md` |
| 主机完整项目启动 | `scripts/start-host.ps1`；共用 `scripts/start-agent.ps1` 校验角色/目录并复用服务，支持 `-NoBrowser` |
| 网卡检查 / 调优 | `scripts/netinfo.ps1`、`scripts/check_mtu.ps1`、`scripts/set_nic.ps1`、`scripts/tune_pc1_nic.ps1` |
| 发端 / DDA 联调 | `scripts/run_T1.ps1`、`scripts/run_dda_sender.ps1`、`scripts/kill_sender.ps1` |
| batch 探针 | `scripts/brn.py`、`scripts/batch_test.ps1` |
| 动画窗口辅助 | `scripts/start_testwin.ps1`、`scripts/fetch_testwin.ps1`、`scripts/test_anim.py` |
| 虚拟卡注册 / 出口验收 | `scripts/vcam_register.ps1`、`scripts/p4_4_shm_verify.py`、`scripts/p4_4_vcam_verify.py`；见 `pc2-vcam-acceptance.md` |
| SHM v3 Python 镜像（读端兼容固定v2） | `scripts/vc_shm.py`，布局真相源仍是 `vcam/vcam_shm.h` |
| 状态 / 历史自检结果 | `scripts/read_pc1_stats.py`、`scripts/list_run.ps1`、`scripts/check_selftest3.py` |
| PC1 长运行资源/阻塞现场与隔离回归 | `scripts/longrun_diagnostics.py`、`scripts/longrun_windows.py`、`scripts/longrun_verify.py`；见 `longrun-diagnostics.md` |
| 历史取证与报告分析 | `scripts/analysis/` |

部分专项脚本绑定了特定机器或历史 session，运行前先看脚本参数；日常验收从 `scripts/verify.py` 与 `ctrl/vc-ctrl.py` 进入。
新建一次性分析脚本放 `scripts/analysis/`，运行证据放 `run/`，文档按 `AGENTS.md` 分类。
