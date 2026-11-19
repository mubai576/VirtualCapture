# PyroWave：GPU 开发后端

默认仍为Raw；PyroWave须显式选择 `codec=pyrowave`、`codec_experimental=true` 与双端 `pyrowave:vulkan`，GOP固定1。它是Vulkan计算着色器帧内小波编码，不是固定功能视频引擎；`hardware=false`是该类别标记，不允许CPU编码。

## 硬件与运行

- 两端实际选中的设备必须可查询为集成/独立GPU、Vulkan API≥1.3；CPU、虚拟、未知设备或属性查询失败均拒绝，不自动换设备/后端。
- 当前仍经CPU缓冲上传、同步回读和UV交错，没有D3D11/Vulkan外部内存互操作，不是零拷贝。桌面输入仍强制硬件D3D11转换。
- 没有质量旋钮。单帧预算为 `min(max_bitrate_bps/8/fps, max_au_bytes−48, NV12帧字节数)`，在会话建立时固定并回显；它不是实际码率或画质保证。目标 `bitrate_mbps` 不参与PyroWave编码。
- 单帧须输出一个完整PyroWave packet，套48B VCAU后再L2分片；完整性、队列、限流与三种hash见skill `references/encoded-stream.md`。
- 普通断档只重建解码器对象并保留Vulkan设备；错误不静默换设备。创建/预热/编码/解码/重置/销毁各有5秒粘性看门狗；超时不退出宿主、不强制展开挂起调用、不卸载DLL，待清理状态锁配置/OTA/虚拟卡。

## 构建

产品由Cargo构建，原生依赖只走 `rust/packaging/native`，命令见 `build.md`。固定上游提交、SHA256与离线缓存位置以 `rust/packaging/native/Pyrowave.cmake` 为准，不再复制另一份版本表。

该入口只构建第三方 `pyrowave-shared`，不构建旧C++产品；不关闭TLS校验，缺源码/摘要不符直接失败。构建使用随源码固定的Vulkan-Headers，不需系统Vulkan SDK；运行必须由显卡驱动提供Vulkan1.3。

将本次构建的 `libpyrowave-shared-1.dll` 放在Rust模块同目录；完整包自动安装它与app-local MSVC。许可文本仅由打包流程在独立 `LicenseAudit` 组件/构建验证目录中收集核对，不进入便携包。依赖源码/二进制缓存不进Git，不改 `thirdparty/`。

## 本机验证（不注册、不部署）

```powershell
python -I rust/tests/dll_stream_smoke.py --package rust/target/release --codec pyrowave
```

该测试使用实际DLL、motion、私有SHM与显式Npcap Loopback/batch1，核对压缩传输hash及解码出口，不要求有损source与decoded相等。必须记录尺寸/fps/帧数/所选真实设备，不能把Loopback当作物理链路或双机性能。

`cargo test --manifest-path rust/Cargo.toml --workspace --locked`另含GPU设备拒绝用例、看门狗和原生编解码测试；缺运行前提不能记为实帧通过。历史证据 `docs/bringup/bringup-20261113-pyrowave.md` 仅适用于当时实现，旧子进程退出124语义已被进程内粘性超时替代。

物理网卡、双机240fps、长稳、PSNR/色彩与完整产品编排仍须独立验收，不能从预算、能力枚举或历史短测推断。
