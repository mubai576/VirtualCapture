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

提交前复核修正便携 smoke 的 Host ROOT 创建顺序及实际 GPUI 窗口标题。OTA 清单新增显式 `role_launch_args` / `desktop_capture` 能力：D34 旧 unified 入口回滚不传新角色参数，图形采集目标缺能力时在停旧进程前拒绝。旧 staged metadata 缺少快照时从完整校验的 manifest 在内存补齐；已有快照不符、载荷哈希不符仍拒绝，不改写旧元数据。旧 job 保持可读，helper 复核目标后恢复能力快照。新增回归覆盖旧/新清单、旧元数据安装与复验、快照冲突及载荷篡改；capture OTA smoke 以 D34 格式元数据验证真实激活。

## 边界

D35 专项实现验收仅覆盖本机构建及隔离/回环测试；专项验证当时未重新打包、提交或部署，后续交付结果在下文追加。没有使用 PC2、没有物理网卡发包、没有注册 DirectShow，也没有读取或操作现存更新令牌、`staging/` 或 `prev/`。不构成物理链路、双机、240fps、长稳、画质或完整 Rust-R3 产品验收。
