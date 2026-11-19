# VirtualCapture Rust 绿色开发版

Windows x64，GPUI主机 + CLI采集后台。产品全部为Rust：`vc_stream.dll`进程内采集/传输，`vc_vcam.dll`提供DirectShow；旧C++/Python产品已移除，不自动回退。

## 使用前提

将完整目录解压到本地可写位置。无需Rust/Python/VS，已带MSVC运行库；实际收发前两端需安装Npcap。桌面采集必须硬件D3D11 GPU转换；显卡驱动需正常。Raw默认；显式开发PyroWave另需两端真实GPU的Vulkan1.3，仍有CPU上传/回读，不是零拷贝。CPU MJPEG不再支持，旧配置会明确拒绝。

1. 被采集机双击 `Start-Capture.cmd`，无参数监听 `0.0.0.0:15143`；保留控制台，Ctrl+C正常退出。
2. 主机双击带图标的 `VirtualCapture.exe`，输入被采集机 `IPv4:15143`；省略端口默认5143，不能混淆。
3. 确认双端网卡、尺寸/fps/源。仅枚举 Windows 状态为 Up 的物理非虚拟网卡，不按厂商或型号偏置；本机按控制连接路由自动选择，对端多候选需明确选择，失效网卡需重选。MTU默认读取双端网卡有效值，可手动设置但不超过已报告限制，不自动修改系统Jumbo。
4. 获准使用真实虚拟卡后开始采集，下游选择VirtualCapture。正常停止/关窗回收本次管理的会话与注册；强杀/断电不保证清理。深蓝画幅为尺寸比例示意，不是实时预览。

只在受控网络使用；首次可能出现防火墙提示。不自动替换原稳定部署，也不接管其他运行根/存活进程的虚拟卡。

## 配置与目录

默认DDA/GPU/640×640@240/Raw/crop；可变尺寸不代表所有组合已达到目标。PyroWave必须明确 `codec_experimental=true`、GOP1与双端 `pyrowave:vulkan`；CPU转换或未协商组合失败，不静默换编码/尺寸/fps。

`VirtualCapture.exe --root <独立目录>` 分离主机配置。Rust使用 `vc-rust-config.json`，不要复制旧Python `vc-config.json`。产品只显示 Windows 状态为 Up 的物理非虚拟网卡；Loopback只供显式诊断，不进普通GUI。

更新后固定ROOT布局：

```text
app/current.json / app/releases/<build_id>/
data/vc-rust-config.json / data/run/
updates/staged/ worker/ incoming/ history.jsonl
```

首次激活仅复制旧扁平配置/配对凭据，不移动旧run或覆盖证据。运行时不要移动程序目录。

## 更新

主机“应用更新”面板用于配对和上传完整Rust ZIP；配对凭据只保存在独立令牌文件，不进入普通配置。stage校验不切版本；采集/待清理时禁止激活。

应用更新后helper等旧进程退出、复核载荷、切current并确认新版本就绪；失败尝试恢复旧版本，结果记updates/history.jsonl。HTTP202不等于成功。仅保留且验证有效的build可回滚；不兼容旧Python更新端点。不要把令牌、配置或run证据放进包。

## 载荷与校验

图形入口、Rust host/capture/updater、运行时隔离verifier，`vc_stream.dll`、`vc_vcam.dll`、固定 `libpyrowave-shared-1.dll`、MSVC DLL和启动脚本。独立 `vc-stream.exe`/`vc-transport.exe` 仅在构建树用于诊断，不进入便携包；许可证只在构建期审计，也不随包分发。Npcap/Vulkan驱动不随包复制；不要从旧包补sender/receiver/旧vc-vcam或turbojpeg。

`package-manifest.json`记录版本、来源和逐文件SHA256；ZIP旁.sha256校验整个包，均不是发布者签名。图形入口自动按app/current选择版本，无需CMD/PowerShell；采集启动器保留CLI。

## 验收边界

这是开发产品，Rust-R3未完整验收。Loopback、私有WGC窗口与隔离COM验证不等于物理网卡/双机240fps/长稳/画质/完整DShow或OTA部署验收。GUI非空白截图是硬门，历史黑屏失败不能略过。未通过完整打包门的候选不是正式验收包。

`vc-vcam-verify.exe --dll <路径> --capabilities`仅隔离只读探测；没有注册表枚举回退。归属恢复只接受本根精确证明与全局owner一致，未知/跨根注册拒绝接管。
