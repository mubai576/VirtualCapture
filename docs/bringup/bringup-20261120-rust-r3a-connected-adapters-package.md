# Rust-R3a 精简网卡修正版提交与打包（2026-11-20）

## 授权与源码来源

按 owner 本轮“提交 打包”要求，提交 D31 便携包精简与 D32 已连接物理网卡枚举修正，重新执行完整及扩展打包门禁。不升版本、不 push、不部署；不操作 PC2、不修改物理网卡参数、不做真实 DirectShow 注册。

- 源码提交：`d6eb33db7ae0c675266cd98e0816ba35a47168b4`，`Rust-R3a/D31-D32: 精简便携包并修正物理网卡枚举`，22 个文件。
- 主工作区另有未跟踪的 `.vscode/settings.json`，只读确认后原样保留，未提交、未收录到来源或载荷，也未改变忽略规则。
- 从精确源码提交建立独立干净构建副本 `build/p-839983/`。打包前确认 HEAD 与上述提交一致、Git 状态为空；不使用 `--allow-dirty`。
- manifest 如实记录 `source.dirty=false`、空 `source.status`；独立复核全部 353 个源码文件 SHA-256，与该干净副本逐一一致，不含 `.vscode/`。包中的来源提交仍是 `d6eb33d`，本记录与 PLAN 的后续纯证据提交不在该包内。

从主工作区调用的完整打包命令：

```powershell
python -I -X utf8 .\build\p-839983\scripts\package.py --extended-smokes
```

唯一打包器仍为 `rust/packaging/package.py`；根入口只转发参数。独立构建副本用于隔离无关编辑器配置，并未改成另一套打包实现。

## 成品

| 项目 | 结果 |
|---|---|
| 版本 | `0.2.1`，Windows x64 Release |
| build id | `20261120-005019-063265` |
| 交付目录 | `dist/VirtualCapture-Rust-0.2.1-windows-x64-20261120-005019-063265/` |
| ZIP | `dist/VirtualCapture-Rust-0.2.1-windows-x64-20261120-005019-063265.zip` |
| SHA sidecar | 同名 `.zip.sha256` |
| 文件数 | 23，含 `package-manifest.json` |
| ZIP 字节数 | `15583699` |
| ZIP SHA-256 | `fc4dec95d6ffbc5af61b64eeaf0cac03ee23e76115afea323c140f71c7dfdfae` |
| 原始构建报告 | `build/p-839983/build/rust-package/20261120-005019-063265/report.json` |
| 构建日志 | 同目录 `package.log` |
| 独立复核报告 | `build/rust-precommit-20261120-5638635e/independent-verification.json` |

原始成品保留在独立副本的 `build/p-839983/dist/`。来源、载荷、ZIP 与 GUI 证据核对后，无覆盖复制目录、ZIP 和 SHA sidecar 到主工作区上述唯一 `dist/` 路径；复制后再次比对 ZIP SHA-256、sidecar 和全部目录文件哈希。旧包、失败候选、构建日志及运行证据均保留。

## 本次门禁

完整打包命令退出码为 0；fmt、workspace 314 项测试、Clippy `-D warnings`、Release 均成功。报告中的十项检查全部实际执行并为 `passed`：

| 检查键 | 本次结果与范围 |
|---|---|
| `rust_tests` | PASS，314 passed、0 failed、0 ignored |
| `fmt` | PASS |
| `clippy` | PASS，`-D warnings` |
| `portable_smoke` | PASS，从含中文和空格的解压目录运行；载荷、PE 导入、运行库与产品能力检查 |
| `dll_stream_smoke` | PASS，真实同进程 DLL 本机隔离 smoke，不是仅自检 |
| `physical_adapters` | PASS，完整诊断列表 14 张，产品列表恰为全部 2 张 Up 物理非虚拟网卡；默认 Loopback prepare 拒绝，显式诊断才可见 |
| `gui_smoke` | PASS，真实原生 GPUI 窗口、交互与非空白截图，正常退出 0 |
| `capture_ota` | PASS，独立根与本机隔离服务，不更新已用部署 |
| `raw_loopback_120_frames` | PASS，416×416@60、motion、batch=1、NPF_Loopback，发送/接收均120帧、hash_equal=true；仅本机功能检查，不是物理链路性能验收 |
| `host_controller_ota` | PASS，独立根与本机隔离 host 更新检查，不更新已用部署 |

本次运行证据均位于独立副本的 `build/p-839983/run/`：

- DLL：`rust-dll-stream-smoke-04178bb9b4b8/`。
- 网卡：`rust-adapter-smoke-ca84203282/report.json`。
- GUI：`rust-gui-smoke-8fe87a4898/report.json`；已实际查看该目录 `physical-adapters.png`，不是黑屏，显示“以太网 3：2.50 Gbps · Up”和“以太网 2：10 Mbps · Up”。低速但已连接物理网卡仍正常列出；链路预算与启动门禁另行判断。GUI 报告中的网卡名称来自诊断枚举推导，实际窗口显示由截图复核补充确认。
- GUI 还通过刷新、清除已保存虚拟卡选择、预设持久化、更新配对/重新配对、隔离 HTTP 503 错误处理与正常关闭；仅访问本机 fixture 的 `/health`，没有开始真实采集。
- Capture OTA：`rust-ota-smoke-280003317d/`。
- Raw 控制面：`rust-smoke-04f48000c5/`。
- Host OTA：`rust-host-update-smoke-9206872d74/`。

提交前最终 `scripts/verify.py` 也以退出码 0 完成：314 项测试及 fmt/Clippy/Release、独立整数图案参考、wire vectors、DLL self-check 均 PASS，日志为 `build/rust-precommit-20261120-5638635e/precommit-verify.log`。其中 self-check 不加载真实 DLL；本次包另有上述 live DLL smoke，不混为一项。

## 载荷、许可审计与独立复核

23 个包文件中保留图形入口、Rust host/capture/updater、`vc_stream.dll`、`vc_vcam.dll`、运行时能力探测依赖 `vc-vcam-verify.exe`、固定 PyroWave 与 MSVC 运行库、启动脚本、使用说明和版本/来源清单。不分发 `vc-stream.exe`、`vc-transport.exe`、开发证据、配置、凭据或 `licenses/`。

构建期仍完整收集并审计许可材料：目标 `x86_64-pc-windows-msvc`，524 个第三方 crate、1329 条文件记录、0 个未解决例外、24 个警告；审计材料保留在本次验证目录的 `license-audit/`，不进入 Git 或便携载荷。不随包分发许可正文、许可审计索引或逐依赖副本，不表示第三方许可义务被免除。`package-manifest.json` 的 `source.files` 仍记录源码路径及 SHA-256，路径可含 license 相关名称，这不是捆绑许可文件。

独立复核确认：来源 revision/clean 状态与全部源码哈希一致；十项检查齐全；`gpu_only=true`、codecs 精确为 raw/pyrowave；实际目录文件集与 manifest 一致；ZIP 无重复成员、CRC 正常、内嵌 manifest 与目录一致，所有逐文件大小/SHA-256、ZIP SHA-256 和 sidecar 均一致；主工作区交付副本与原始成品一致。

Release 仍有 Cargo `vc_stream.pdb` 同名输出警告，构建退出码为 0，PDB 不在载荷中；未隐藏警告或扩大修改范围。此前只读复核 Workflow 在文档 verifier 完成前被停止，不宣称所有 Workflow verifier 已完成；实际源码复核、文档局部修正后的最终总门及本次完整打包结果分别以各自证据为准。

## 验收边界

Rust-R3 仍为 **IN PROGRESS**。本次网卡枚举、真实 GUI、本机 Loopback、隔离 OTA 与便携包门禁不等于物理网卡整帧传输、双机 240fps、长稳、画质、目标桌面或完整 DirectShow 验收。旧双机编排/完整取证的 Rust 替代工具仍待补齐，冻结 wire 和验收门限不变，历史失败/BLOCKED/未跑结论不回填。

本轮仅提交源码和本记录/PLAN 交付证据、生成并校验新包；没有 push、部署、PC2 操作、物理网卡参数变更或真实 DirectShow 注册。既有更新令牌、`staging/`、`prev/` 和已用版本均未改动。
