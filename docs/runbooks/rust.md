# Rust + GPUI 开发版（D26–D28）

被采集机运行 `vc-capture.exe` CLI 后台，主机运行 `vc-host.exe` GPUI 工作台；编排、配置、预算与 OTA
在 UI 无关的 `vc-core` 中。Rust 产品数据面是模块化的：`vc-core` 进程内加载版本化 `vc_stream.dll`
（C ABI v1）运行采集/接收；采集源、编解码与传输编在该 DLL 内，`vc_vcam.dll` 是 Rust COM DirectShow
虚拟摄像头。独立 `vc-stream.exe` 诊断 CLI 与 DLL 复用同一 crate。产品启动不再以
`vc-sender.exe`/`vc-receiver.exe` CLI 拼装本机数据面；旧 C++ 产品与协议契约另存，不进入 Rust 包。
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

输出 `rust/target/debug/{vc-capture,vc-host,vc-launcher,vc-updater,vc-transport,vc-stream,vc-vcam-diag}.exe`
与 `{vc_stream.dll,vc_vcam.dll}`；加 `--release` 输出到 `rust/target/release/`。`vc-stream.exe` 是独立
Npcap Raw 诊断工具，也与 `vc_stream.dll` 共用实现；`vc_vcam.dll` 为 Rust DShow COM 虚拟摄像头。
`vc-launcher.exe` 在包内命名为 `VirtualCapture.exe`，作为带图标的主机图形启动入口。
原 `scripts/package.py` 与根 CMake 仍只构建/打包原产品；Rust 便携包、Rust 原生运行库与 OTA 使用
`rust/packaging/package.py`，载荷清单以 `rust/packaging/inventory.py` 为准（原生运行库单独由
`rust/packaging/native/CMakeLists.txt` 构建，不构建旧 C++ 产品）。

## 绿色运行压缩包

```powershell
python -I rust/packaging/package.py --allow-dirty
```

`--allow-dirty` 表示已检查当前 Rust worktree 改动；默认有未提交文件就拒绝。构建机另需 Python pefile、
Pillow；目标机不需要 Python/Rust/VS。流程为 Rust fmt/tests/Clippy/Release build → 构建
`rust/packaging/native`（仅 `vc-vcam-verify.exe`、固定 TurboJPEG/PyroWave 共享库与许可）→
按 `inventory.py` 收集 Rust EXE/DLL、app-local MSVC runtime、第三方许可 → 拒绝旧 C++ 产品与开发数据
→ ZIP 解压验证 → `portable_smoke`、同进程 `dll_stream_smoke`、adapter/gui/ota smoke → SHA-256 清单与最终 ZIP。
旧 `scripts/verify.py` 仍是独立旧产品门禁，不由 Rust 打包流程执行；默认门禁也不含
control/host_update 专项，`--extended-smokes` 额外执行 120 帧 Raw Loopback `control_smoke`
（Rust REST 控制面 + 进程内 `vc_stream.dll` 接收端）与 `host_update_smoke`（HostController
更新与真实 capture helper 重开）。

载荷以 `inventory.py` 为准：`VirtualCapture.exe`、`vc-host`/`vc-capture`/`vc-updater`/`vc-stream`/
`vc-transport` EXE，`vc_stream.dll`、`vc_vcam.dll`，`vc-vcam-verify.exe`，`turbojpeg.dll`、
`libpyrowave-shared-1.dll` 与 MSVC runtime；旧 `vc-sender.exe`/`vc-receiver.exe`/`vc-vcam.dll` 出现即失败。

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
`--native-dir` 必须直接包含 `vc_stream.dll`（主机还需 `vc_vcam.dll`），显式目录优先且不搜索回退；
未指定时按 Managed 当前 release → build → ROOT → exe 目录查找。示例指向 debug 目标目录，
Release 构建改用 `rust/target/release`。

```powershell
.\rust\target\debug\vc-capture.exe --bind 0.0.0.0 --port 15143 --root D:/VirtualCaptureRust/capture --native-dir D:/Project/REA/VirtualCapture-rust/rust/target/debug
.\rust\target\debug\vc-host.exe --root D:/VirtualCaptureRust/host --native-dir D:/Project/REA/VirtualCapture-rust/rust/target/debug
```

- 两端列表只保留 Windows 明确 `hardware_interface=true` 且 `virtual=false` 的物理网卡，排除回环与
  元数据未知设备；展示、自动选卡、手动选择、prepare/start 共用该列表。主机枚举失败清空旧列表，
  失效选择清除后须手动重选。
- 被采集机默认禁止回环；仅显式 `--allow-loopback-diagnostics` 且控制器本身是回环地址时允许
  `\Device\NPF_Loopback`，用于本机诊断，不冒充物理容量。`--list-adapters` 仍是完整诊断枚举。
- 默认 640×640@240、Raw、crop、DDA、GPU 转换、MTU9000；Raw batch 由尺寸/MTU 自动计算为整帧
  （无用户可调 batch），Npcap Loopback 诊断固定 batch1；可变尺寸要求偶数。
  MJPEG/PyroWave 必须显式 `codec_experimental=true` 且后端精确匹配，否则 Rust 启动明确拒绝，不静默降级。
- 产品启动不拉起 sender/receiver 子进程；`vc_stream.dll` 缺失、ABI/能力不符时明确失败，不回退旧程序。
- 虚拟卡归属由 HKCU CLSID `VirtualCaptureOwner` 与本根精确匹配的 schema 2 证明确定（DLL、运行根、PID、进程创建时间）。current/pending 固定在 `ROOT/data/run/vcam-owned.json`、`ROOT/data/run/vcam-owned.pending.json`，兼容读取旧 `ROOT/run/vcam-owned.json`，不随 Legacy/Managed/OTA 回滚改变位置，也不移动旧 run 证据。Windows 等价根路径先规范化；共享 `--native-dir` 不允许不同运行根互相接管，缺少全局 owner 值的旧注册不自动恢复。
- owner 更新先保全旧全局证明，再刷盘新 pending、提交注册表、提升 current；注册表写失败或提交后进程中断仍保留一份精确证明，重试不放宽记录一致性要求。该机制不是断电时跨文件系统/注册表的原子事务；未知状态仍拒绝自动接管/注销，详细顺序见 [Rust 控制契约](../../.agents/skills/virtualcapture/references/rust-control.md)。
- 数据面模块必须报告 stream revision 2、可变尺寸、SHM v3、sendqueue=frame 与 codecs/sources；
  虚拟卡注册前用 `vc-vcam-verify.exe --dll ... --capabilities` 隔离探测；已有注册必须证明属于本根本进程或确认已退出属主，不能接管其他根、其他存活属主或未知注册。
- PyroWave 在 ready 前显式编解码预热，使用同几何/后端/预算的合成 NV12，输出丢弃；冷启动耗时另记，
  不计入正式帧号/hashlog/SHM/稳态帧率。创建/预热/编码/解码/重置/销毁受原生 5 秒看门狗覆盖；
  超时只报告会话粘性超时，不终止宿主；挂起时保留 worker/句柄/DLL，
  pending 期间不能改配置/激活 OTA/注册虚拟卡，不能安全停止时不自动卸载，交由进程退出回收。
- 正常停止/关窗按会话号停远端 sender、停本机 receiver，只回收本控制器注册的 DLL；
  停止未确认显示待清理，先清理再改配置。断电/强杀不保证清理。

## 独立 Raw Npcap 互操作诊断

`vc-stream.exe` 是随包的 Windows 诊断 CLI，不属于 `vc-capture` 控制 API；它与 `vc_stream.dll` 共用 crate 实现，但不注册 DShow，也不切换产品数据面。当前 CLI 仍是 Raw pattern 诊断（DDA/WGC 与编码经同一 crate 的 DLL ABI 在进程内运行）。需要 Windows + Npcap。完整逐帧证据和范围见[bring-up 记录](../bringup/bringup-20261119-rust-data-plane.md)。

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
python -I rust/tests/dll_stream_smoke.py --package <解压目录>
python -I rust/tests/control_smoke.py
python -I rust/tests/gui_smoke.py
python -I rust/tests/ota_smoke.py
python -I rust/tests/host_update_smoke.py
python -I rust/tests/transport_vectors.py
```

- `adapter_smoke.py`：真实启动采集后台，确认 `--list-adapters` 仍为完整列表、`/adapters` 只含物理网卡、
  无诊断开关时回环 prepare 被 409 拒绝且未启动采集。
- `dll_stream_smoke.py`：同一进程 ctypes 加载真实 `vc_stream.dll`（C ABI v1），覆盖 ABI 布局/版本、
  start/status/stop/destroy、Raw 与按 `--codec mjpeg|pyrowave` 选择的发送/接收 hash、SHM v3 与
  pending stop；不启动任何 sender/receiver/vc-stream/vc-vcam-verify 子进程，默认只用 Npcap Loopback。
  `--package <解压目录>` 测发布包，`--performance` 才加本机 cadence/延迟门；它不证明物理网卡或双机。
- `control_smoke.py`：Rust `vc-capture` REST 控制面 + 同一进程内 `vc_stream.dll` 接收端，不启动
  sender/receiver EXE；随机回环控制端口、独立 ROOT、私有 SHM，以 `--allow-loopback-diagnostics`
  启动后台，120 帧 Raw motion 全量 FNV 对账，并检查错误会话停止、运行中改配置、复用会话不得覆盖
  证据及 DLL stop/destroy 生命周期。默认由 `--extended-smokes` 追加。
- `gui_smoke.py`：真实 GPUI 窗口的预设持久化、物理网卡刷新与筛选、1080 宽布局、HTTP 错误和正常退出。
- `ota_smoke.py`：私有ROOT/随机端口，真实capture/helper暂存、激活、回滚和失败恢复；`--package <解压目录>`使用Release。
- `host_update_smoke.py`：需要 `vc_stream.dll` 与 `vc_vcam.dll`；从 UI 无关 HostController 驱动
  对端更新与本机 helper 重开，确认新 GPUI 回执；支持 `--package`，默认由 `--extended-smokes` 追加。
- `source_dll_smoke.py`：通过真实 DLL ABI 对私有窗口做 WGC CPU/GPU 采集、缩放与关闭生命周期检查；DDA需要可用DXGI显示输出；`--allow-dda-unavailable`只将无输出明确记录为BLOCKED，不会改用其他源。证据留在 `run/rust-source-dll-smoke-*`。
- `vcam_dll_smoke.py`：通过隔离 `vc-vcam-verify.exe` 检查 `vc_vcam.dll` 的SHM v3、交付/重复和阻塞Stop/Disconnect生命周期，不注册DirectShow或操作注册表。证据留在 `run/rust-vcam-dll-smoke-*`。
- `transport_vectors.py`：真实C++黄金向量、独立Rust库测试与pcap CLI回放，不开网卡。

以上只证明本机功能。2026-11-19三编码Release DLL Loopback性能、WGC与vcam隔离验证及失败边界见
[本机 Rust-R3 记录](../bringup/bringup-20261119-rust-r3-local.md)：Raw/MJPEG/PyroWave各2400帧 motion/640×640@240
同机cadence和<10ms延迟门通过；不代表物理NIC或双机验收。该轮DDA因无DXGI输出BLOCKED，GUI smoke全黑截图触发硬门失败，故便携包完整验收未通过。Rust OTA 的双机升级/回滚、DShow完整编排、编码画质、双机240fps与长稳仍待专项验收；Rust-R3保持IN PROGRESS。性能结论必须注明pattern/batch/adapter，Loopback不冒充物理网卡/双机结论。
