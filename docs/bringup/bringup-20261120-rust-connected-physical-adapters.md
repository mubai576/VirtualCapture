# Rust 已连接物理网卡枚举修正 — 2026-11-20

## 范围与实现

按 D32 澄清，I226 仅为历史测试设备，不是产品筛选或排序偏好。

- `vc-core` 统一产品候选：明确 `hardware_interface=true`、`virtual=false`、Windows `Status=Up`，排除 Npcap Loopback；缺失元数据或状态不视为合格。不按厂商、型号或枚举顺序自动挑选。
- 本机自动选择按控制连接出站 IPv4 路由精确匹配；对端只有唯一合格候选时才可自动预览，多候选明确要求选择。显式选择失效后不得静默换卡。
- `vc-capture` REST 产品列表、`vc-host` 网卡列表与 MTU 预览、prepare 回显检查共用已连接物理网卡规则。完整 CLI 诊断列表保留，Loopback 仍需显式诊断许可。
- MTU 仅只读查询双端网卡报告值，不修改系统参数；wire、REST schema、DLL ABI 与 GPU-only 约束不变。

## 本轮验证

源码状态为 `rust` 分支、基于 `1a6cd14` 的未提交工作区；既有便携包精简修改保留。本记录不表示这些改动已经提交或重新打包。

| 验证 | 结果与证据 |
|---|---|
| Rust 总门 `python -I -X utf8 scripts/verify.py` | PASS，退出码 0；fmt、workspace 314 项测试、Clippy `-D warnings`、Release、独立整数图案参考、wire vectors、DLL self-check。DLL self-check 未加载真实 DLL，不替代 live smoke |
| Release 网卡 `python -I -X utf8 rust/tests/adapter_smoke.py --package rust/target/release` | PASS；完整诊断 14 张，产品列表为全部 2 张已连接物理网卡（以太网 3、以太网 2），隐藏 12 张；无厂商过滤；未授权 Loopback prepare 返回 409 且未启动采集；显式诊断可见 Loopback。证据 `run/rust-adapter-smoke-2921d540d5/report.json` |
| 原生 GPUI `python -I -X utf8 rust/tests/gui_smoke.py --package rust/target/release` | PASS，正常退出 0；真实窗口非黑屏，实际刷新显示两张物理网卡、清除保存的虚拟网卡选择；预设持久化、更新面板、隔离 HTTP 503 错误与正常关闭均通过。证据 `run/rust-gui-smoke-25ff575e22/report.json` |
| GUI 人工图像复核 | 已查看 `run/rust-gui-smoke-25ff575e22/physical-adapters.png`：两张网卡分别显示 2.50 Gbps / Up、10 Mbps / Up；虚拟和未连接设备不在产品列表。低速已连接物理卡仍正常列出，链路预算与启动门禁另行判断 |
| 补丁空白检查 | `git diff --check` 通过；仅 Git LF/CRLF 提示，无空白错误 |

Release 构建有 Cargo `vc_stream.pdb` 文件名冲突警告，退出码仍为 0；未隐藏警告。早先两项 smoke 默认使用 debug 路径时未完成验证，以上为构建完成后使用 Release 产物的成功重跑，不回填旧失败日志。

## 未覆盖与权限边界

- 本轮只运行本机隔离服务和原生 GUI，没有采集、物理发包、双机、长稳或画质验证，也没有真实 DirectShow 注册。
- 没有操作 PC2、改变物理网卡参数、部署、提交、push 或重新打包；既有精简便携包不包含本次网卡修正。
- Rust-R3 整体仍为 IN PROGRESS；本次网卡子卡完成不替代完整产品验收，也不改变历史失败或 BLOCKED 结论。
