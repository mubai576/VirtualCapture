# Rust-R3a 双 EXE 便携包（D34，2026-11-21）

## 成品与来源

| 项目 | 结果 |
|---|---|
| 版本 / build ID | `0.2.1` Windows x64 / `20261121-163453-82bb3e` |
| ZIP / 校验文件 | `dist/VirtualCapture-Rust-0.2.1-windows-x64-20261121-163453-82bb3e.zip` / 同名 `.zip.sha256` |
| 载荷 | 16 个文件，其中仅 `VirtualCapture.exe` 与 `vc-updater.exe` 两个 EXE；无启动脚本、独立诊断 EXE、许可正文、配置或凭据 |
| ZIP 字节数 | `14393011`（D33 为 `15583699` 字节、23 个文件） |
| ZIP SHA-256 | `d647416326ad46542fefeac75f70ed4c382b9192a700469cdebccb86c050343d` |
| 构建报告及日志 | `build/d34-b81397/build/rust-package/20261121-163453-82bb3e/{report.json,package.log}` |
| 独立复核报告 | `build/entrypoint-validation-d34/verified-20261121-163453-82bb3e.json` |

来源是 `ae4aeb7ec600169140fcb77797920365867a7330` 加已审阅的 D34 未提交改动，不是该提交的原样构建。独立副本 `build/d34-b81397/` 不含工作区的 `.vscode/settings.json`；清单如实记录 `source.dirty=true`，354 个源码文件的 SHA-256 逐一复核。许可仅在构建目录审计：524 个第三方 crate、1329 条文件记录、0 个未解决例外、24 个警告。审计材料不进入 ZIP。

同一快照使用 `scripts/package.py --allow-dirty --extended-smokes`。首次 Release 中 `rustc` 编译 `ureq` 时退出 `0xc0000005`，该次未生成包；保持源码、Release profile、锁文件和门限不变，设 `CARGO_BUILD_JOBS=4` 后全门通过。已有 `vc_stream.pdb` 同名输出警告，PDB 未分发。独立校验 ZIP 成员、CRC、逐文件大小和哈希、整个 ZIP 及 sidecar 后，复制到主工作区唯一 `dist/` 路径并再核哈希；未覆盖 D33 产物。本记录与 PLAN 的完成状态在打包后补写。

## 本机完整门禁

- `fmt`、workspace 316 项测试（0 失败、0 忽略）、Clippy `-D warnings` 与 Release 通过。原生依赖、PE 导入和许可审计通过。包报告的 10 项完整/扩展检查均为 `passed`。
- 解压运行验证了带中文与空格路径、扁平/Managed 主机和采集、实际 PID/ROOT/版本、同进程 DLL、实际虚拟卡能力导出、app-local MSVC、坏指针拒绝和 Ctrl+Break 正常退出。最终包的前台更新器入口在私有控制台收到 Ctrl+C，采集正常退出，等待进程返回 0，见 `build/entrypoint-validation-d34/ctrl-c-gy7lf84d/ctrl-c-report.json`。
- 网卡在完整诊断中有 14 张，产品列出全部 2 张已连接物理非虚拟网卡。已实际查看 `build/d34-b81397/run/rust-gui-smoke-7c9a5a2b21/physical-adapters.png`，界面非空白，显示“以太网 3 · 2.50 Gbps”和“以太网 2 · 10 Mbps”。真实刷新、预设、配对/重新配对、隔离连接错误与正常关闭通过，未启动桌面采集。
- 扩展 Raw 本机功能：416×416@60、`motion`、`batch=1`、`NPF_Loopback`，发送/接收各 120 帧，逐帧哈希一致。证据 `build/d34-b81397/run/rust-smoke-07100684d7/report.json`，不代表物理链路性能。
- 采集 OTA 原样暂存候选 ZIP：独立 helper 与两个 CRT 的字节一致；校验失败、显式回滚、启动失败恢复、回滚失败 code 6 和过期入口拒绝通过。主机 OTA 通过 HostController 使用带原清单的实际候选载荷，观察新 GPUI 首帧回执、实际新 PID、从固定 ROOT 重开以及正常关闭。报告分别为 `build/d34-b81397/run/rust-ota-smoke-66414db0f0/report.json` 与 `build/d34-b81397/run/rust-host-update-smoke-c6705b7214/report.json`。

## 真实旧包兼容与验收边界

使用实际 D33 ZIP `20261120-005019-063265` 与本记录的最终 ZIP，在新私有 ROOT 测试：D33 扁平采集 → 安装同一 D33 以保留回滚目标 → D34 统一入口 → 显式回滚 D33。核对了三次激活日志与实际 EXE、PID、ROOT/build 和原 `127.0.0.1:53458` 监听；固定 ROOT 主程序替换并能在升级/回滚后转发诊断，原 ROOT 更新器未改，回滚 worker 使用 D34 自身更新器和匹配的 CRT。证据 `build/entrypoint-validation-d34/D34 旧版迁移 hvhrs62r/compatibility.json`。这是旧版**采集端**的跨版结果，不是实际旧版主机 OTA 证明。

D34 完成不代表 Rust-R3 完整验收。物理网卡整帧传输、双机 240fps、长稳、画质和完整 DirectShow 仍未在本轮验证。不提交、不推送、不部署或切稳定版；未操作 PC2、修改网卡参数、注册实际 DirectShow，未动既有令牌及 `staging/`、`prev/`。
