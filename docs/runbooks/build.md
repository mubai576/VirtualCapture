# Rust 构建与打包

## 工具链

Windows x64、Rust MSVC工具链、Visual Studio C++/Windows SDK、Python；完整打包另需 Git、CMake≥3.20、Python pefile/Pillow。MSVC不必在普通PATH中，原生依赖用VS生成器。Cargo依赖锁在 `rust/Cargo.lock`。

产品全部由 Cargo 构建，包括 Rust 隔离虚拟卡验证器；根 CMake 与旧产品树已移除。唯一 CMake 入口 `rust/packaging/native` 仅处理固定 PyroWave 第三方库与 app-local MSVC，不构建项目 C++ 产品。

## 编译与总闸

```powershell
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
```

输出 `rust/target/release/`：Rust EXE、`vc_stream.dll`、`vc_vcam.dll`；`vc-launcher.exe` 在包内命名 `VirtualCapture.exe`。

```powershell
python scripts/verify.py
```

依次运行 fmt检查、workspace测试、全target Clippy（warnings为错误）、Release、独立Python图案参考/冻结wire向量/DLL脚本self-check。默认不打开采集源或网卡，不注册DLL。`--no-build` 仅省略Release，仍跑tests/Clippy。

```powershell
python scripts/verify.py --no-build --live Loopback
```

显式增加同进程DLL的Raw本机Loopback功能诊断，不代表物理NIC/双机门通过。GPU/source/GUI/OTA/DShow专项入口见 `rust.md`。

## 原生依赖（只在需要 PyroWave/打包时）

```powershell
cmake -S rust/packaging/native -B build/rust-package-native -A x64
```

```powershell
cmake --build build/rust-package-native --config Release
```

```powershell
cmake --install build/rust-package-native --config Release --prefix build/rust-native-runtime --component Runtime
```

固定提交及SHA256在 `rust/packaging/native/Pyrowave.cmake`，下载档案进入该构建目录 `_deps/downloads/` 并校验，缺失/错误就失败；不改 `thirdparty/`。产物为 `libpyrowave-shared-1.dll`、MSVC DLL及原始许可。运行PyroWave还需显卡驱动提供 Vulkan1.3；不需构建机安装Vulkan SDK，不再依赖TurboJPEG/NASM。

## 完整便携包

```powershell
python scripts/package.py
```

工作区dirty默认拒绝；仅在审阅所有收录改动后：

```powershell
python scripts/package.py --allow-dirty
```

唯一实现 `rust/packaging/package.py`，精确载荷 `inventory.py`。自动fmt/tests/Clippy/Release/native install、许可/PE审计、解压运行、DLL/网卡/GUI/OTA隔离门；`--extended-smokes`追加control/host_update。GUI黑屏不可绕过，任一失败不发布最终包。

输出唯一 `dist/VirtualCapture-Rust-<版本>-windows-x64-<时间>-<id>/`、ZIP和SHA256，不覆盖已用 `dist/VirtualCapture`；日志/候选在 `build/rust-package/<build_id>/`。构建过程中源码改变即失败；来源/载荷不得收录现有凭据、配置、证据或未审阅并发改动。

目标机无需开发工具，但实际收发需Npcap、桌面转换需硬件D3D11、PyroWave需Vulkan1.3。完整包可用于Rust OTA，不能上传旧Python更新端点。构建/打包不构成部署授权。

## 证据保护

不清理 `run/build/dist/release/archive`，不手动改 `staging/prev`。PASS报告/receiver证据删除前先归档；双机结束立即保全双端完整hashlog。原工具链历史只从Git查阅，不恢复为构建依赖。
