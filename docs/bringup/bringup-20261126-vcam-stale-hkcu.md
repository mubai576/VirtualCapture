# 虚拟卡旧 HKCU 注册冲突定位（2026-11-26）

## 结论

现场接收端确实运行新包 `20261126-002714-cbcb20`，不是误开旧 EXE。新的冲突提示显示当前注册仍指向 2026-11-14 旧安装里的 `vc-vcam.dll`；此次原生注册表读取证实该项实际存在。此前“注册已清空、只是旧窗口缓存”的判断不成立，D37 的状态刷新不能消除真实注册冲突。

## 只读证据

- 进程 PID `6504`，实际 EXE 为 `D:\Share\VirtualCapture\app\releases\20261126-002714-cbcb20\VirtualCapture.exe`。
- `D:\Share\VirtualCapture\app\current.json`：`build_id=20261126-002714-cbcb20`，`generation=5`。现场 EXE、`vc_stream.dll`、`vc_vcam.dll` 与本仓库 dist 新包逐文件 SHA-256 一致。
- 正在报错的窗口截图：`run/host-6504-printwindow.png`。注册冲突错误包含实际旧 DLL 路径；stdout/stderr 为空，不据此判断无错误。
- Python `winreg` 使用 `KEY_WOW64_64KEY` 读取到：

```text
HKEY_CURRENT_USER\Software\Classes\CLSID\{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}\InprocServer32
(default) = D:\Share\VirtualCapture-Rust-0.2.0-windows-x64-20261114-140047-1a5966\VirtualCapture-Rust-0.2.0-windows-x64-20261114-140047-1a5966\vc-vcam.dll
```

- 同一 HKCU 64 位 CLSID 没有 `VirtualCaptureOwner` 值；旧 DLL 与所属旧安装目录均不存在。
- 对应 HKCU 64 位设备枚举项也存在，FriendlyName 为 `VirtualCapture`，CLSID 与上文一致：

```text
Software\Classes\CLSID\{860BB310-5D01-11d0-BD3B-00A0C911CE86}\Instance\{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}
```

- HKCU 32 位以及 HKLM 的 32/64 位 `InprocServer32` 查询为缺失；HKCR 64 位读取到相同旧 DLL 路径。
- 直接经 shell 传递脚本的 PowerShell 检查仍产生“缺失”结果；改由 Python 启动 `powershell.exe -EncodedCommand`，显式指定 `Registry64` 后与 winreg 得到相同旧路径。不能把此前 shell/PowerShell 查询的阴性结果当作清理完成证据；本次未断言其具体参数变形机制。

## 备份与操作边界

两项 HKCU 64 位旧注册（含子项、值类型及二进制值）已只读备份到：

```text
run/vcam-stale-backup-20261126-004926/registry-backup.json
```

首次按用户先前“只清理旧注册”的授权尝试定点删除，在执行前被自动权限检查以 `Irreversible Deletion (general)` 拒绝；没有改用其他工具绕过，当时仅完成只读备份。

用户随后明确回复“允许清理”。2026-11-26 00:57:19（UTC+08:00）执行前逐项复核两棵注册子树与备份完全一致、注册仍指向上述旧 DLL 且旧目录不存在；随后仅删除上述 HKCU 64 位 CLSID 与设备枚举项。备份 SHA-256：`adf3bf6337e20e0a99e0598a9e7b01b8d6c18dedfe98ab9621499e1d46a4ceff`。

执行与复核结果：

- 定点删除成功，两项注册均已不存在；没有修改其他 CLSID 或其他注册表值。
- Python winreg 复核 HKCU/HKLM/HKCR 的 32/64 位视图，目标 CLSID 与对应设备枚举项共 12 项检查均为不存在。
- 另以正确编码传参的 PowerShell 显式 `Registry64` 读取 HKCU `InprocServer32`，交叉确认不存在。
- 当前同一接收端进程 PID `6504` 无须重启，红色注册冲突提示自动消失，左下角虚拟采集卡由“其他目录已注册”变为“待启动”。窗口仍处于未开始采集状态；顶部现在显示 DDA 静态桌面帧率说明，不是注册冲突错误。
- 本次没有启动采集、注册新虚拟卡、操作 PC2、修改网卡参数、部署或提交代码。

清理结果：`run/vcam-stale-backup-20261126-004926/cleanup-result-20261126-005719.json`。
清理后窗口证据：`run/vcam-stale-backup-20261126-004926/host-6504-after-cleanup.png`。

状态：**DONE——本机旧注册冲突已清除，同一窗口的自动刷新已现场确认。** 此结论仅覆盖旧注册清理和提示刷新，不代表新虚拟卡注册、实际采集、双机收发或画质验收。
