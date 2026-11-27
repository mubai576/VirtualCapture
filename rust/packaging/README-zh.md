# VirtualCapture Rust 绿色开发版

Windows x64，GPUI主机 + CLI采集后台。产品全部为Rust：`vc_stream.dll`进程内采集/传输，`vc_vcam.dll`提供DirectShow；旧C++/Python产品已移除，不自动回退。

## 使用前提

将完整目录解压到本地可写位置。无需Rust/Python/VS，已带MSVC运行库；实际收发前两端需安装Npcap。桌面采集必须硬件D3D11 GPU转换；显卡驱动需正常。Raw默认；显式开发PyroWave另需两端真实GPU的Vulkan1.3，仍有CPU上传/回读，不是零拷贝。CPU MJPEG不再支持，旧配置会明确拒绝。

1. 两台电脑都双击带图标的 `VirtualCapture.exe`。首次分别选择角色：采集画面的电脑选“采集端”，播放画面的电脑选“接收端”；选择会自动保存，下次直接进入。需要控制台或诊断时，被采集机可用 `VirtualCapture.exe capture`，默认监听 `0.0.0.0:5143`。
2. 主机在“接收端”工作台填写采集端的 IPv4（默认端口 `5143`）。采集端若指定其他端口，接收端填 `IPv4:端口`。角色切换可在任一空闲角色页使用“重新选择角色”。
3. 确认双端网卡、尺寸/fps/源。仅枚举 Windows 状态为 Up 的物理非虚拟网卡，不按厂商或型号偏置；本机按控制连接路由自动选择，对端多候选需明确选择，失效网卡需重选。MTU默认读取双端网卡有效值，可手动设置但不超过已报告限制，不自动修改系统Jumbo。
4. 获准使用真实虚拟卡后开始采集，下游选择VirtualCapture。正常停止/关窗回收本次管理的会话与注册；强杀/断电不保证清理。深蓝画幅为尺寸比例示意，不是实时预览。

可选恢复入口：需要从终端直接启动已保存 ROOT 中的指定角色时使用：

```powershell
.\vc-updater.exe launch --root . --role capture
```

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

首次激活复制旧扁平配置，不移动旧 run 或覆盖证据。新版本就绪后补齐缺失的固定入口，或替换清单与哈希可确认的旧版 `VirtualCapture.exe` 启动器。不要在运行时移动程序目录。

## 更新

主机“应用更新”面板在连接后可直接上传完整Rust ZIP；不再需要配对令牌，控制端口须限制在可信网段。stage校验不切版本；采集/待清理时禁止激活。

应用更新后helper等旧进程退出、复核载荷、切current并确认新版本就绪；失败尝试恢复旧版本，结果记updates/history.jsonl。HTTP202不等于成功。仅保留且验证有效的build可回滚；不兼容旧Python更新端点。不要把配置、旧令牌或run证据放进包。

## 载荷与校验

包内仅两个 EXE：`VirtualCapture.exe`（主机、采集及隔离能力探测）和 `vc-updater.exe`（独立更新/回滚）。保留 `vc_stream.dll`、`vc_vcam.dll`、固定 `libpyrowave-shared-1.dll` 和 MSVC 运行库。无启动脚本、独立诊断 EXE 或许可材料；Npcap/Vulkan 驱动另行安装。不要从旧包补程序或 DLL。

`package-manifest.json` 记录 `unified-v1` 入口、版本、来源和逐文件 SHA-256；ZIP 的 `.sha256` 校验整个包，均不是发布者签名。新版主程序按 `app/current.json` 选择已激活版本。旧包经 OTA 后，历史启动脚本和旧根目录的更新器不改；请从固定 ROOT 使用新版 `VirtualCapture.exe`。旧实例 OTA 重启沿用原监听端口，不会从 15143 自动改到 5143。

## 验收边界

这是开发产品，Rust-R3未完整验收。Loopback、私有WGC窗口与隔离COM验证不等于物理网卡/双机240fps/长稳/画质/完整DShow或OTA部署验收。GUI非空白截图是硬门，历史黑屏失败不能略过。未通过完整打包门的候选不是正式验收包。

主程序使用隔离子进程只读探测实际虚拟卡 DLL 的能力，不枚举已注册设备。归属恢复只接受本根精确证明与全局 owner 一致，未知或跨根注册拒绝接管。
