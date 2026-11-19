# Rust + GPUI 开发版（D26 / D27）

被采集机运行 `vc-capture.exe` CLI 后台，主机运行 `vc-host.exe` GPUI 工作台；编排、配置、预算与 OTA
在 UI 无关的 `vc-core` 中。产品数据面仍是 C++：Npcap L2、DDA/WGC、SHM 与 DShow 继续使用原二进制，
Rust 产品控制器仅能运行 Raw。`vc-transport` 是冻结 wire、分片、校验与有界重组的迁移库；独立 `vc-stream`
另提供 Windows Npcap Raw 双向诊断与可选 SHM v3 writer，尚未接入产品控制面、绿色包或 OTA。
`vc-updater` 是版本切换助手，由 OTA 从 `updates/worker/` 启动。

设计与契约：[重构提案](../proposals/rust-rewrite-20261114.md)、
[OTA 目录与事务](../proposals/rust-ota-layout-20261114.md)、
[Rust 控制面契约](../../.agents/skills/virtualcapture/references/rust-control.md)。

## 构建

Windows x64、MSVC/Windows SDK、Rust MSVC 工具链（本轮 Rust/Cargo 1.97.1）。
GPUI 使用 `gpui-kit 0.9.1`，需要图形桌面。`vc-host/build.rs` 从 `assets/app-icon.svg` 生成 ICO，
再调用 Windows SDK 的 `rc.exe` 写入 exe 图标；找不到时可用 `VC_RC` 指向 `rc.exe`，否则构建失败。

```powershell
cmake -S . -B build
cmake --build build --config Release
cargo build --manifest-path rust/Cargo.toml --workspace --locked
```

输出 `rust/target/debug/{vc-capture,vc-host,vc-launcher,vc-updater,vc-transport,vc-stream}.exe`；加 `--release` 输出到 `rust/target/release/`。其中 `vc-stream.exe` 是独立 Npcap Raw 诊断工具，不纳入当前绿色包或 OTA，运行方法见下文。
`vc-launcher.exe` 在包内命名为 `VirtualCapture.exe`，作为带图标的主机图形启动入口。
原 `scripts/package.py` 仍只打包原产品；Rust 便携包与 OTA 使用 `rust/packaging/package.py`。

## 绿色运行压缩包

```powershell
python -I rust/packaging/package.py --allow-dirty
```

`--allow-dirty` 表示已检查当前 Rust worktree 改动；默认有未提交文件就拒绝。构建机另需 Python pefile、
Pillow；目标机不需要 Python/Rust/VS。流程为 Rust fmt/tests/Clippy/Release build → 原项目 `verify.py`
→ CMake Runtime install → 收集 Rust 五个 exe、原生 7 目标、PyroWave DLL、app-local MSVC runtime、
第三方许可 → ZIP 解压验证 → SHA-256 清单与最终 ZIP。

输出 `dist/VirtualCapture-Rust-<版本>-windows-x64-<时间>-<唯一号>/` 与同名 `.zip` / `.zip.sha256`。
每次新目录，不替换原 `dist/VirtualCapture`；日志与解压运行目录留在 `build/rust-package/<构建号>/`。
任一门禁失败会终止发布最终包；候选包的清单中未跑门禁保持pending，实际结果以 `build/rust-package/<构建号>/package.log` 为准。

包内入口按 `app/current.json` 选择当前 release：

- **主机双击 `VirtualCapture.exe`**：Windows图形子系统、原生图标；直接解析current并启动GPUI，不调用CMD/PowerShell。
  日志及启动回执写`<data>/run/host-*`，失败弹出原生错误对话框；支持`--root`/`--native-dir`，默认根为入口所在目录。
  OTA从0.2.0升到0.2.1时，新host在首帧就绪前补齐ROOT入口；已有入口保留，始终按current选择版本。
  原`Start-Host.cmd`/`scripts/start-host.ps1`只保留兼容诊断用途，不是日常入口。
- `Start-Capture.cmd` → `scripts/resolve-capture.ps1`：只返回当前 `build_id`，CMD 用原始 Unicode 根
  直接运行 CLI；无参数默认 `0.0.0.0:15143`，exe 本身仍默认 5143。
- 图形入口与采集启动器都支持空格/中文路径；`VirtualCapture.exe --root <目录>`可分离主机配置。
  图形入口放在`app/releases/<build_id>/`时也会识别固定ROOT，避免把配置另存到版本目录。

## 目录布局与 OTA（D27）

固定运行根 `--root`。存在 `app/current.json` 即进入 Managed 布局：

```text
ROOT/app/releases/<build_id>/   当前/历史版本程序
ROOT/app/current.json           原子切换的当前指针（含 build_id/version/generation）
ROOT/data/vc-rust-config.json   配置
ROOT/data/run/                  stats/hashlog/日志
ROOT/data/update-token.json     采集端更新令牌（仅 --print-update-token 打印）
ROOT/data/update-peer-token.json 主机配对令牌
ROOT/updates/staged/<build_id>/ 已校验待激活
ROOT/updates/worker/            helper、job 与日志
ROOT/updates/incoming/          上传临时区
ROOT/updates/history.jsonl      激活/回滚事件
```

无 `current.json` 的扁平目录保持 `ROOT/vc-rust-config.json` 与 `ROOT/run/`。**首次激活**只把旧配置与令牌
复制到 `data/`，不移动 `run/` 或旧目录；首次升级失败时移除 `current.json` 即可回到扁平布局。
已进入 Managed 后程序与配置分目录，版本切换不移动正在使用的配置与证据。

流程：

1. 采集端运行 `vc-capture.exe --print-update-token --root <ROOT>`，把令牌填入主机“应用更新”面板并配对；
   令牌不进入 health、日志或普通配置，错误令牌返回 401。
2. 主机选择同一便携 ZIP：可暂存本机、或上传采集机；上传带 `X-VC-SHA256`，超时 180s。
   暂存只校验并解包到 `updates/staged/`，不切 `current`；运行中允许暂存，采集或待清理时不可激活。
3. “应用更新”提交 `{build_id}`，服务返回 202 并由 `vc-updater` 等待旧 PID 退出、复核载荷、
   原子切换 `current.json`、启动新程序。采集端确认 Rust 身份/ROOT/build/新PID；主机以
   `--update-ready <path>` 首帧后回执。
4. 失败恢复旧指针并重启旧版本，历史写入 `history.jsonl`；不可指定回滚到未暂存/未保留的 build。
   ZIP 只接受本产品 `package-manifest.json`，拒绝路径穿越、重复、符号链接、越界与哈希不符；
   SHA-256 不是发布者签名。

旧 0.1.0 程序没有更新端点，**首次启用 OTA 需要一次手工 bootstrap**（用支持 OTA 的版本替换一次），
之后才可全程从固定 ROOT 更新。不要把 Rust ZIP 上传到旧 Python agent 的 `/update`。

## 独立运行

两个入口都支持 `--root`、`--native-dir`；优先级 CLI > `VC_ROOT`/`VC_NATIVE_DIR` > 自动探测。
Managed 布局下原生 exe 默认取当前 release，显式 `--native-dir` 优先。

```powershell
.\rust\target\debug\vc-capture.exe --bind 0.0.0.0 --port 15143 --root D:/VirtualCaptureRust/capture --native-dir D:/Project/REA/VirtualCapture-rust/build/Release
.\rust\target\debug\vc-host.exe --root D:/VirtualCaptureRust/host --native-dir D:/Project/REA/VirtualCapture-rust/build/Release
```

- 两端列表只保留 Windows 明确 `hardware_interface=true` 且 `virtual=false` 的物理网卡，排除回环与
  元数据未知设备；展示、自动选卡、手动选择、prepare/start 共用该列表。主机枚举失败清空旧列表，
  失效选择清除后须手动重选。
- 被采集机默认禁止回环；仅显式 `--allow-loopback-diagnostics` 且控制器本身是回环地址时允许
  `\Device\NPF_Loopback`，用于本机诊断，不冒充物理容量。`--list-adapters` 仍是完整诊断枚举。
- 默认 640×640@240、Raw、crop、DDA、GPU 转换、batch69、MTU9000；可变尺寸要求偶数。
  MJPEG/PyroWave 草稿可保存，Rust 启动会明确拒绝，不静默降级。
- 两端实际 exe 必须报告 stream revision 2、可变尺寸与 SHM v3；虚拟卡注册前用
  `vc-vcam-verify.exe --dll ... --capabilities` 隔离探测。已有 VirtualCapture 注册时拒绝接管。
- 正常停止/关窗按会话号停远端 sender、停本机 receiver，只回收本控制器注册的 DLL；
  停止未确认显示待清理，先清理再改配置。断电/强杀不保证清理。

## 独立 Raw Npcap 互操作诊断

`vc-stream.exe` 是单独的 Windows 诊断 CLI，不属于 `vc-capture` 控制 API，不由当前绿色包/OTA 收集；它不注册 DShow，也不切换产品数据面。需要 Windows + Npcap。完整逐帧证据和范围见[bring-up 记录](../bringup/bringup-20261119-rust-data-plane.md)。

```powershell
cmake --build build --config Release
cargo build --manifest-path rust/Cargo.toml -p vc-stream --release --locked
python -I rust/tests/raw_stream_smoke.py --self-check
python -I rust/tests/raw_stream_smoke.py
```

默认只用 `\\Device\\NPF_Loopback`、640×640@60、MTU9000、120 帧、motion、batch1，并同时运行 C++/Rust 双向 TX 与 C++/Rust 双接收端；六份完整 hashlog 必须逐帧一致，且全部与 Python pattern reference 对账。Rust receiver 默认发布唯一私有 SHM v3 mapping，冒烟会检查布局、最新帧 payload 和 hash 标记。该 smoke 只验证功能互操作，不证明物理网卡、吞吐、延迟、双机、长稳或 DShow 生命周期。无 `--batch` 参数覆盖时固定 batch1；显式 batch 原值使用，不自动 retry/fallback。运行证据留在 gitignored `run/rust-raw-stream-smoke-<id>/`。

同机 240 fps 探针使用独立 harness：

```powershell
python -I rust/tests/raw_stream_perf.py --self-check
python -I rust/tests/raw_stream_perf.py --batch 1
```

默认测 2400 帧 Raw 640×640@240、motion、MTU9000、Npcap Loopback、SHM v3；用同机 hashlog QPC 配对测发送准备完成至末片重组完成延迟，并独立报告 T0 CPU 门禁。当前本机样本 cadence 与 p99 <10ms 达标，但 sender CPU 门限未通过；见[性能证据和限制](../bringup/bringup-20261119-rust-data-plane.md#240-fps-同机性能探针)。这不替代物理网卡或双机 T1/T2。`--batch 69` 可显式进行对照，短发送即失败且不自动 fallback。结果保存在 gitignored `run/rust-raw-perf-<id>/`。

## 验证

```powershell
cargo fmt --manifest-path rust/Cargo.toml --all --check
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
python scripts/verify.py
python -I rust/tests/adapter_smoke.py
python -I rust/tests/control_smoke.py
python -I rust/tests/gui_smoke.py
python -I rust/tests/ota_smoke.py
python -I rust/tests/host_update_smoke.py
python -I rust/tests/transport_vectors.py
```

- `adapter_smoke.py`：真实启动采集后台，确认 `--list-adapters` 仍为完整列表、`/adapters` 只含物理网卡、
  无诊断开关时回环 prepare 被 409 拒绝且未启动采集。
- `control_smoke.py`：随机回环控制端口、独立 ROOT、私有 SHM，需以
  `--allow-loopback-diagnostics` 启动后台；Rust REST → C++ sender/receiver，120 帧全量 FNV 对账。
- `gui_smoke.py`：真实 GPUI 窗口的预设持久化、物理网卡刷新与筛选、1080 宽布局、HTTP 错误和正常退出。
- `ota_smoke.py`：私有ROOT/随机端口，真实capture/helper暂存、激活、回滚和失败恢复；`--package <解压目录>`使用Release。
- `host_update_smoke.py`：从UI无关HostController驱动对端更新与本机helper重开，确认新GPUI回执；支持`--package`。
- `transport_vectors.py`：真实C++黄金向量、独立Rust库测试与pcap CLI回放，不开网卡。

以上只证明本机功能；Rust OTA 的双机升级/回滚、DShow 完整编排、DDA/WGC 实采、双机 240fps 与长稳
仍待专项验收。性能结论必须用 `motion` 并注明 pattern/batch/adapter；回环数据不冒充双机结论。
