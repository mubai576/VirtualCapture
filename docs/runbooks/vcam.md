# Rust DirectShow 虚拟采集卡

产物 `rust/target/release/vc_vcam.dll`，由Cargo构建；固定CLSID `{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}`，设备名 `VirtualCapture`，单输出pin `Capture`（`PIN_CATEGORY_CAPTURE`）。输出会话NV12尺寸/fps，默认640×640@240；这是用户态COM filter，不是内核驱动。

## 不注册的开发验证

```powershell
python -I rust/tests/vcam_dll_smoke.py
```

默认加载Rust Release DLL与Cargo构建的 `vc-vcam-verify.exe`。用私有映射覆盖真实COM/allocator/IMediaSample、正常帧/无信号/最后好帧重复/保留样本/阻塞Receive/运行中Disconnect等七场景；不枚举已注册设备、不调用regsvr32。具体路径参数以 `--help` 为准。

独立工具示例（映射须由隔离测试先提供）：

```powershell
.\rust\target\release\vc-vcam-verify.exe --dll .\rust\target\release\vc_vcam.dll --shm Local\VirtualCapture-Isolated --width 416 --height 416 --fps 60 --seconds 3 --hashlog .\run\isolated-outlet.jsonl
```

`--dll` 必填，没有真实注册枚举回退。便携主程序内部另起 `VirtualCapture.exe __vcam-probe --dll <绝对路径>`，只读实际 DLL 能力导出，不运行 COM graph。构建树工具的 `--capabilities` 也只读能力，不能替代 COM 收流验证。`--allow-empty`允许无信号，`--block-receive`检查BeginFlush解阻塞，`--hold-samples`与`--disconnect-running`检查生命周期；Stop/Disconnect须小于2秒。结束要求 `DllCanUnloadNow=S_OK` 后才卸载。

## SHM与格式

布局见 `docs/spec/shm-v3.md`：v3动态步长、12槽、只读seqlock、交替事件；读端兼容固定v2，写端不覆盖已有同名映射。不能对只读映射使用LOCK/RMW。

filter创建时从有效映射固定唯一一档格式；没有映射时默认640但不输出假帧。应先开始接收，再在下游打开设备；切尺寸必须关闭并重新打开下游。D12：从无有效帧不输出，有过有效帧后才可重复最后好帧。allocator两缓冲，不扩FIFO掩盖积压。

`<映射名>.vcam`反馈环记录同机交付时段，不能与远端QPC相减。测试可用仅本进程环境 `VCAM_FRAME_MAP` 选择私有映射，不改HKCU或系统环境。

## 产品注册边界

真实注册/注销必须单独授权；开发验证不用它。获准后由HostController执行精确owner保护流程，不手动regsvr绕过。要求全局 `VirtualCaptureOwner` 与本ROOT的schema2证明一致，DLL路径/ROOT/PID/进程创建时间共同核对，CLSID互斥串行化。

共享native-dir不代表共享运行根。其他根、存活属主、路径或存活状态未知、旧注册无全局owner，全部拒绝接管/注销。日志pending保留中断恢复证明，不承诺断电跨介质原子提交。细节见skill `references/rust-control.md`。

正常停止先停精确远端会话，再停本地DLL，最后只回收本控制器注册的虚拟卡。stop/destroy未确认则保留句柄与DLL，不能强制卸载、清理owner证明或覆盖旧部署。

本机隔离通过不代表物理网卡/双机/长稳、通用下游完整产品编排或真实注册验收。旧P4/P6历史报告继续保留，不回填当前PASS。
