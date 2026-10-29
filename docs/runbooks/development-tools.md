# 开发工具与脚本入口

目录职责以根 `AGENTS.md` Repo map 为准。本页按操作找入口；命令均在仓库根目录执行。
双机测试与部署会操作真实设备，参数和前置条件按各自 runbook 核对。

## 日常开发

| 操作 | 入口 |
|---|---|
| 一键编译验证并打包完整目录 | `python scripts/package.py`；确认包含未提交改动时加 `--allow-dirty` |
| 本地完整验证 | `python scripts/verify.py` |
| 本机回环 live | `python scripts/verify.py --no-build --live Loopback` |
| 查看构建产物时间 | `python scripts/check_bins.py` |
| Python 测试源自检 | `python ctrl/pattern_ref.py` |
| 双机编排与判分 | `ctrl/vc-ctrl.py`；参数见 `AGENTS.md` 常用命令 |
| 生成联调报告 / 对账 | `ctrl/bringup.py` / `ctrl/reconcile.py` |
| 文本仪表 | `ctrl/vc-monitor-tui.py` |

## 原生辅助工具

统一使用根 CMake 构建；完整命令和二进制路径见 `build.md`。

| 用途 | 源码 | 构建目标 |
|---|---|---|
| SHM NV12 预览、OSD | `tools/monitor/monitor.cpp` | `vc-monitor` |
| D3D11 诊断 | `tools/monitor/diag.cpp` | `vc-monitor-diag` |
| DDA 高帧率动画源 | `tools/testwin/testwin.cpp` | `vc-testwin` |
| DShow 枚举 / NV12 格式验收 | `tools/vcam_verify/vcam_verify.cpp` | `vc-vcam-verify` |

这三个源码目录于 2026-10-29 从根目录迁入 `tools/`；历史报告中的旧源码路径保留原文。
工具名称、Release 二进制路径均保留，现有启动脚本继续适用。

## 设备运维与专项验收

| 用途 | 入口 |
|---|---|
| 安装 / OTA / watchdog | `scripts/install-pc2.ps1`、`scripts/update.ps1`、`scripts/update_sender.ps1`、`scripts/watchdog.ps1` |
| 网卡检查 / 调优 | `scripts/netinfo.ps1`、`scripts/check_mtu.ps1`、`scripts/set_nic.ps1`、`scripts/tune_pc1_nic.ps1` |
| 发端 / DDA 联调 | `scripts/run_T1.ps1`、`scripts/run_dda_sender.ps1`、`scripts/kill_sender.ps1` |
| batch 探针 | `scripts/brn.py`、`scripts/batch_test.ps1` |
| 动画窗口辅助 | `scripts/start_testwin.ps1`、`scripts/fetch_testwin.ps1`、`scripts/test_anim.py` |
| 虚拟卡注册 / 出口验收 | `scripts/vcam_register.ps1`、`scripts/p4_4_shm_verify.py`、`scripts/p4_4_vcam_verify.py`；见 `pc2-vcam-acceptance.md` |
| SHM v2 Python 镜像 | `scripts/vc_shm.py`，布局真相源仍是 `vcam/vcam_shm.h` |
| 状态 / 历史自检结果 | `scripts/read_pc1_stats.py`、`scripts/list_run.ps1`、`scripts/check_selftest3.py` |
| 历史取证与报告分析 | `scripts/analysis/` |

部分专项脚本绑定了特定机器或历史 session，运行前先看脚本参数；日常验收从 `scripts/verify.py` 与 `ctrl/vc-ctrl.py` 进入。
新建一次性分析脚本放 `scripts/analysis/`，运行证据放 `run/`，文档按 `AGENTS.md` 分类。
