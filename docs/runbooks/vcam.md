# vcam runbook（P4-3 DShow 虚拟采集卡）

产物：`build/Release/vc-vcam.dll`（用户态 source filter，无需驱动签名）。
开发版输出：会话 NV12 尺寸/fps；默认仍为640x640@240，单输出 pin `Capture`（PIN_CATEGORY_CAPTURE）。
设备名 `VirtualCapture`，对标本机 GC573（`AVerMedia HD Capture GC573 1`，ffmpeg 列为 `(video)`）。
默认帧源：receiver 的 SHM v3；本地整数motion仅显式bring-up。稳定成品仍按封存版本运行，不因构建替换。

## 构建
cmake -S . -B build
cmake --build build --config Release   # 产物 build/vc-vcam.dll（VS 生成器下在 build/Release/）

## 注册 / 注销（HKCU，免管理员）
regsvr32 build/Release/vc-vcam.dll     # 注册当前构建（HKCU\...\CLSID\{B84C7E21-...} + VideoInputDeviceCategory 实例）
regsvr32 /u build/Release/vc-vcam.dll  # 注销
# 验证注册：ffmpeg -f dshow -list_devices true -i dummy | 应见 "VirtualCapture" (video)
# 选项：ffmpeg -list_options true -f dshow -i video="VirtualCapture"  -> Pin "Capture"，只公布创建实例时的一档实际会话格式
# 开流：ffmpeg -rtbufsize 256M -f dshow -i video="VirtualCapture" -t 5 -f null -   （默认 3MB rtbufsize 只够 ~5 帧会报丢帧）
# 无 receiver 时 D12 不出帧；bring-up 用 motion：
#   reg add "HKCU\Software\Classes\CLSID\{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}" /v Motion /t REG_DWORD /d 1 /f
#   （用完 reg delete ... /v Motion /f）

## receiver 喂帧（P4-3 通路，PC2 用法）
receiver --shm <映射名> --width 416 --height 416 --fps 60  # shm v3，12槽动态步长，完成即发布/置事件
vcam 默认读 Local\VirtualCaptureVcamFrame；改名：在 HKCU\...\CLSID\{B84C7E21-...} 下写 REG_SZ `FrameMap`。
agent /receiver/start 传 {"shm": "<映射名>"} 即透传（本机验证：loopback noise 全链路见 bringup-20261024-vcam-outlet）。
注意：读端禁止对只读映射用 LOCK 前缀指令；torn-read无历史好帧时跳过，有历史才重复（D12）。
- 布局唯一真相：`vcam/vcam_shm.h`（v3，P6-2；保留D15事件/ring设计）。新读端兼容固定v2，旧读端拒绝VCA3。
  writer拒绝已存在的同名映射，不覆盖旧会话。先关闭旧下游/预览、停止采集，再重新开始采集，最后打开下游。
  filter创建时固定尺寸/fps；缺映射时默认640但不出假帧，不能先打开下游再期待它热改格式。
- vcam 事件驱动：新帧到即交付；静默超过1.5个会话帧周期才按会话fps重复好帧；样本时间戳取图时钟（Run tStart 为零点）。
  源本身的抖动会原样透出（ffmpeg 偶见 "non monotonically increasing dts"，同 ms 内到两帧时出现），不是丢帧。
- allocator 2 缓冲：下游最多积压 1 帧；OBS 采集源建议关闭"缓冲"。
- 延迟回写：vcam 每交付一帧写 `<映射名>.vcam` 环，receiver stats 的 `vseg_*` 即"receiver 发布→vcam 交付下游"。

## 不注册的开发验证

`vc-vcam-verify.exe --dll <绝对DLL路径> --shm <私有映射> --width W --height H --fps F --seconds 3 --hashlog <文件>`
直接加载DLL并加入本进程DShow graph，连接真实allocator/IMediaSample sink；结束检查DllCanUnloadNow，无regsvr32。
`--allow-empty`验证无信号；`--block-receive`模拟需BeginFlush解阻塞的下游，验证Stop安全退出。
完整矩阵用 `scripts/p6_raw_verify.py`，历史Python J2仍固定640，不能用于任意尺寸断言。

## 注意事项
- 卸载前先注销（/u），再删 dll；重建后需重新 regsvr32（路径不变则覆盖即可）。
- CLSID：`{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}`，FriendlyName：`VirtualCapture`（旧名 `VirtualCapture 640x640 NV12`，2026-10-25 起改）。
- 历史坑：`AMPROPSETID_Pin` GUID 尾部曾手写错（应为 `B3F1-00AA003761C5`），导致所有 client 查 pin category 失败、
  ffmpeg/OBS 显示 `(none)` / `Could not find output pin`；bringup-20261023-vcam 里"ffmpeg quirk"的结论是误判。
