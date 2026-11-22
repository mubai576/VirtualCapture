# Rust-R3a 桌面角色选择与持久化（D35）

日期：2026-11-22

## 范围

两台电脑运行相同的 `VirtualCapture.exe` 时，各自首次选择本机为采集端或接收端。选择仅在用户确认后写入与采集配置分离的 `vc-startup.json`；下次启动直接进入保存角色。接收工作台、空闲采集页及 `--select-role` 可打开选择界面。采集端忙碌、会话待清理或 OTA 中时，不允许切换或直接卸载后台。显式 `VirtualCapture.exe capture` 仍是控制台 REST v2 采集后台。

扁平 ROOT 首次选择写 `ROOT/vc-startup.json`，Managed 写 `ROOT/data/vc-startup.json`。读取优先使用已有 `data/` 偏好，以便 Managed 回滚后继续采用最新角色。偏好 JSON 为 schema 1，角色 `capture` 或 `receive`；损坏或未知 schema 不静默默认为某一端。通过 ROOT 使用 `--select-role` 时仍转发 `--capture-bind` / `--capture-port`。

## 实现

- `rust/crates/vc-core/src/startup.rs`：角色枚举、严格读取及同目录临时文件原子保存；与 `vc-rust-config.json` 完全分离。
- `rust/crates/vc-host/src/app_ui.rs`：选择页、采集状态页、接收工作台重选；采集服务先确认空闲、关闭并 join 后才进入选择页。
- `rust/crates/vc-host/src/main.rs`：默认启动按偏好选角色；公开重选入口、Managed ROOT 转发与采集监听参数保留；OTA 内部恢复角色独立处理。
- `rust/crates/vc-core/src/update.rs`：更新任务创建和读取均校验角色、桌面采集模式的组合，拒绝会令 helper 启动错误角色或无法回执的 job。
- `rust/tests/gui_smoke.py`：真实 GPUI 首次选择、落盘、退出、重启、采集/接收两端切换、再重启及接收工作台原有交互。
- `rust/tests/host_update_smoke.py`：私有 Managed ROOT/OTA 测试真实点选 ROOT 转发的角色选择器；另有 `--capture-desktop` 场景验证图形采集 OTA 后恢复原监听。

## 验证证据

本机完整门禁命令 `python scripts/verify.py`：通过；331 项 workspace Rust 测试、fmt、全 workspace Clippy、Release、独立 pattern 参考、冻结 wire vectors、DLL self-check 均通过。

真实 Release GUI：`python -I rust/tests/gui_smoke.py --package rust/target/release` 通过，窗口正常退出，截图包含采集端、接收端及两端重选页。私有证据目录 `run/rust-gui-smoke-e9b3b36a7a/`，最终偏好为 `{"schema":1,"role":"receive"}`，控制配置除既有无效 adapter 清理行为外保持一致。

真实 Managed Host OTA：`python -I rust/tests/host_update_smoke.py --bin rust/target/release --native rust/target/release` 通过。覆盖更新后接收页、ROOT 保存的采集角色及自定义端口、显式 `--select-role` 转发后真实点击采集端、`data/vc-startup.json` 更新、监听关闭、peer 配置保持及 DShow 注册未变化。私有报告 `run/rust-host-update-smoke-29181c6216/report.json`。

真实图形采集 OTA：`python -I rust/tests/host_update_smoke.py --bin rust/target/release --native rust/target/release --capture-desktop` 通过。更新 job 保持 `capture_desktop=true`、`start_role=null` 及原 loopback bind/port；更新前后均显示采集窗口，关闭后进程退出 0、监听释放。私有报告 `run/rust-host-update-smoke-6dce058b39/report.json`。

GPUI 角色切换曾出现标题/HTTP 已变化但画面保留旧视图的问题。修复在 `DesktopApp::show` 将实体通知延迟到当前事件结束后，并使窗口下一帧失效；上述 Release GUI 证据来自这一修复后的真实窗口。

## 提交前兼容修正

提交前复核修正便携 smoke 的 Host ROOT 创建顺序及实际 GPUI 窗口标题。OTA 清单新增显式 `role_launch_args` / `desktop_capture` 能力：D34 旧 unified 入口回滚不传新角色参数，图形采集目标缺能力时在停旧进程前拒绝。旧 staged metadata 缺少快照时从完整校验的 manifest 在内存补齐；已有快照不符、载荷哈希不符仍拒绝，不改写旧元数据。旧 job 保持可读，helper 复核目标后恢复能力快照。新增回归覆盖旧/新清单、旧元数据安装与复验、快照冲突及载荷篡改；capture OTA smoke 以 D34 格式元数据验证真实激活。首轮完整包 smoke 的 GUI 截图碰到 Windows 前台焦点限制，增加线程输入附加辅助后独立重跑全 GUI smoke 通过；仍要求前台 HWND 精确匹配并保留非空白截图硬门。

## 提交与便携包交付

| 项目 | 结果 |
|---|---|
| 源码提交 | `a661fea`（D35角色选择、持久化与OTA能力兼容）；`df2f0ad`（GUI前台证据激活辅助及此交付记录） |
| 最终来源 | `df2f0adaa4d0678a8fe6aba12b39f445abbd56bb`，manifest 记录 `dirty=false` |
| 版本 / build ID | `0.2.1` Windows x64 / `20261122-133707-9f6564` |
| 便携目录 / ZIP | `dist/VirtualCapture-Rust-0.2.1-windows-x64-20261122-133707-9f6564/` 与同名 `.zip` |
| ZIP 大小 / SHA-256 | `14,474,955` 字节 / `31038befdbdb2186ac44522e77e04c71461135d2ea8aa664281fd552bd92a389` |
| 载荷 | 共 16 个归档成员（含 manifest），仅 `VirtualCapture.exe` 与 `vc-updater.exe` 两个 EXE；独立核对 ZIP CRC、SHA-256 sidecar 和清单均一致 |
| 构建记录 | `build/rust-package/20261122-133707-9f6564/{report.json,package.log}` |

从干净源码提交运行 `RUST_TEST_THREADS=1 python scripts/package.py --extended-smokes`，完整和扩展十项检查全部通过。workspace Rust 测试 332 passed、0 failed、0 ignored；fmt、Clippy `-D warnings`、Release、原生依赖/PE/许可审计通过。许可审计覆盖 524 个 crate、1329 条文件记录、0 个未解决例外、24 个警告；许可材料不进入便携包。

解压运行的 portable、同进程 DLL Raw、网卡可见性、真实 GPUI GUI、采集 OTA、HostController OTA 与 Loopback 控制 smoke 全通过。GUI 真实截图覆盖首次选端、保存后重启、采集/接收两侧重选、Host 工作台；截图窗口必须在前台且内容非空。采集 OTA 将 D34 格式的旧 staged metadata（无能力快照）用于新包真实激活，原元数据保持未改写；清单和哈希校验、回滚及错误路径测试通过。Loopback 控制为 `motion`、416×416@60、120帧、batch 1，收发哈希一致；这不是物理链路性能证据。

## 边界

本轮未使用 PC2、未在物理网卡发包、未注册真实 DirectShow，也未读取或操作现存更新令牌、`staging/` 或 `prev/`。便携包、GUI 和 Loopback 门禁不构成物理链路、双机、240fps、长稳、画质或完整 Rust-R3 产品验收；不部署、不 push、不切稳定版。
