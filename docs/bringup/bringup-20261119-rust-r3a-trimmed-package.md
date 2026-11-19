# Rust-R3a 精简便携包（2026-11-19）

## 范围与产物

按 owner 要求缩减便携包，不分发第三方许可正文/索引或独立数据面诊断程序。旧的 0.2.1 包及报告保持不变；本次使用新的 build id 生成独立产物，不覆盖已有包。

| 项目 | 结果 |
|---|---|
| 版本 | `0.2.1`，Windows x64 Release |
| build id | `20261119-230840-bedfe1` |
| 目录 | `dist/VirtualCapture-Rust-0.2.1-windows-x64-20261119-230840-bedfe1/` |
| ZIP | `dist/VirtualCapture-Rust-0.2.1-windows-x64-20261119-230840-bedfe1.zip` |
| 文件数 | 23，含 `package-manifest.json` |
| ZIP 字节数 | `15582496` |
| SHA-256 | `cce91999639b5813ca4317fc1899fe79460f5df61ec8658adfa466ff7131771b` |
| 构建报告 | `build/rust-package/20261119-230840-bedfe1/report.json` |
| 独立复核 | `build/rust-package/20261119-230840-bedfe1/independent-verification.json` |

相较上一包 1363 个文件、19,717,132 字节，减少 1340 个文件及 4,134,636 字节。保留 `vc-vcam-verify.exe`，因为主机运行时用它探测 DirectShow DLL；移除没有产品运行时依赖的 `vc-stream.exe` 与 `vc-transport.exe`。独立诊断仍从构建输出运行。

首个精简批次 `20261119-230045-92d76f` 保留不动；本次重新执行完整及扩展门禁，使最终包的来源快照涵盖修正后的 build/PyroWave runbook。来源基于 `1a6cd149335e365634fe8bb1b49f8b884e637091` 加已审阅的未提交精简改动，manifest 如实记录 `dirty=true`。

## 许可审计与分发边界

Rust crate 和原生依赖许可收集仍作为打包门禁，在唯一 build 验证目录的 `license-audit/` 下运行；便携目录与 ZIP 不含 `licenses/`、Rust 许可正文、汇总清单、许可元数据或逐依赖副本。审计覆盖 Windows MSVC 目标的 524 个第三方 crate、1329 条文件记录，0 个未解决例外、24 个警告。许可材料仍保留在被忽略的 build 验证目录中，不进入 Git。

本包按 owner 指示未附许可文本；这不表示第三方许可义务被免除。原生 PE 导入审计、依赖许可收集和“许可目录不得进入载荷”的负向检查仍启用。

## 验证

完整命令 `python -I -X utf8 scripts/package.py --allow-dirty --extended-smokes` 以退出码 0 完成。312 项 workspace Rust tests、fmt、Clippy `-D warnings`、Release 均成功。最终报告的 10 项检查全部为 `passed`：`rust_tests`、`fmt`、`clippy`、`portable_smoke`、`dll_stream_smoke`、`physical_adapters`、`gui_smoke`、`capture_ota`、`raw_loopback_120_frames`、`host_controller_ota`。

独立复核确认实际目录与 manifest 文件集一致，ZIP CRC、全部逐文件大小/SHA-256、ZIP 与目录内容及 `.zip.sha256` 均一致；23 个文件中无许可证目录，也无两个移除的诊断 EXE。证据回填前，来源快照的 352 个文件全部与工作区匹配，包括两份最终 runbook。

来源快照对应构建时状态；打包后仅回填本记录和 PLAN 的最终证据，不修改产品/打包实现或 runbook，不重写已生成的 manifest、ZIP 或 SHA sidecar。Release 仍有 Cargo 的 `vc_stream.pdb` 同名输出警告；构建成功，PDB 不在载荷中。

GUI smoke 启动解压包内应用并正常退出。网卡检查仅证明可见性，Raw motion/120 帧 Loopback 仅为本机功能证据；不表示物理网卡传输、双机、长稳、画质或完整 DirectShow 验收。Rust-R3 整体仍为 IN PROGRESS。

本轮精简改动尚未提交；没有 push、部署、PC2 操作、物理网卡参数变更或真实 DirectShow 注册。
