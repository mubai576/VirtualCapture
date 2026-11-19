# Rust 便携包契约

唯一实现 `rust/packaging/package.py`；根 `scripts/package.py` 仅原样转发参数。版本来自 `rust/Cargo.toml`，载荷与许可的唯一清单为 `rust/packaging/inventory.py`。

## 构建与发布门禁

Windows x64：Rust MSVC/Windows SDK、Python（pefile/Pillow）、Git、CMake/MSVC。所有产品及隔离验证器由 Cargo 构建；`rust/packaging/native/` 只构建固定 PyroWave 第三方库并收集 app-local MSVC。

流程：源码来源快照 → fmt/tests/Clippy/Release → 原生依赖 build/install → 载荷/许可与 PE 导入审计 → 完整候选清单/ZIP解压 → portable/DLL/adapter/真实GUI/capture OTA smoke → 来源不变复核 → 唯一 dist 目录/ZIP/SHA-256。
`--extended-smokes` 追加 control 与 host_update。本地隔离 OTA 不等于真实双机部署。

GUI 截图必须非空白；黑屏就是失败，不删断言、不以窗口存在代替。失败只保留候选和日志，不发布“已验收”最终包。日志位于 `build/rust-package/<build_id>/package.log`，未执行门禁保持 pending。

## 载荷与运行前提

- 图形入口 `VirtualCapture.exe`；Rust `vc-host/vc-capture/vc-updater.exe` 与运行时隔离verifier `vc-vcam-verify.exe`。独立 `vc-stream.exe`/`vc-transport.exe` 只由构建树诊断使用，不随包分发。
- `vc_stream.dll` C ABI v1、Rust `vc_vcam.dll`、`libpyrowave-shared-1.dll`、MSVC runtime。
- 启动脚本、使用说明、生成的 `version.json`、`package-manifest.json`。许可证仍在构建期收集和审计，不随便携包分发。
- codecs 精确为 raw/pyrowave；sources 为 pattern/dda/wgc。旧 C++ 产品、`turbojpeg.dll` 与开发证据禁止进入包。
- 目标机无需 Rust/Python/VS；实际收发需自行安装 Npcap。桌面转换需要硬件 D3D11；PyroWave 需真实 GPU 的 Vulkan1.3 驱动。驱动不随包复制。

## 来源保护与输出

默认 dirty 工作区拒绝；`--allow-dirty` 只在确认收录范围后使用，不代表允许夹带并发未审阅改动。
来源记录 Git revision/dirty、受跟踪与未忽略源码 SHA-256；构建中源码改变则失败。凭据、配置、run/build/dist 等不得作为来源或载荷收录；尤其不得读取现有更新令牌。

每次新建 `dist/VirtualCapture-Rust-<version>-windows-x64-<time>-<id>/` 及同名 ZIP/.sha256；不覆盖旧 `dist/VirtualCapture` 或运行配置。没有旧 `--output` 覆盖目录语义。

同一个完整 Rust ZIP 可用于 Rust OTA stage，必须先完整校验、再在停止且非 pending 状态激活；不兼容旧 Python `/update`。SHA-256 只证完整性，不是发布者签名。详细运行布局、令牌配对、就绪与回滚见 `rust-control.md`。
