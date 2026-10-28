# vcam runbook（P4-3 DShow 虚拟采集卡）

产物：`build/Release/vc-vcam.dll`（用户态 source filter，无需驱动签名）。
输出：NV12 640x640，AvgTimePerFrame=41666（240fps），单输出 pin `Capture`（PIN_CATEGORY_CAPTURE）。
设备名 `VirtualCapture`，对标本机 GC573（`AVerMedia HD Capture GC573 1`，ffmpeg 列为 `(video)`）。
当前帧源：本地整数 motion（`sender/pattern.h`）；receiver 喂帧是 P4-4 接线事项。

## 构建
cmake -S . -B build
cmake --build build --config Release   # 产物 build/vc-vcam.dll（VS 生成器下在 build/Release/）

## 注册 / 注销（HKCU，免管理员）
regsvr32 build/Release/vc-vcam.dll     # 注册当前构建（HKCU\...\CLSID\{B84C7E21-...} + VideoInputDeviceCategory 实例）
regsvr32 /u build/Release/vc-vcam.dll  # 注销
# 验证注册：ffmpeg -f dshow -list_devices true -i dummy | 应见 "VirtualCapture" (video)
# 选项：ffmpeg -list_options true -f dshow -i video="VirtualCapture"  -> Pin "Capture", nv12 640x640 24/30/240
# 开流：ffmpeg -rtbufsize 256M -f dshow -i video="VirtualCapture" -t 5 -f null -   （默认 3MB rtbufsize 只够 ~5 帧会报丢帧）
# 无 receiver 时 D12 不出帧；bring-up 用 motion：
#   reg add "HKCU\Software\Classes\CLSID\{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}" /v Motion /t REG_DWORD /d 1 /f
#   （用完 reg delete ... /v Motion /f）

## receiver 喂帧（P4-3 通路，PC2 用法）
receiver --shm <映射名>      # shm v2：原地重组进 12 槽 ring，完成即翻 seq + 置事件 <映射名>.evt0/1
vcam 默认读 Local\VirtualCaptureVcamFrame；改名：在 HKCU\...\CLSID\{B84C7E21-...} 下写 REG_SZ `FrameMap`。
agent /receiver/start 传 {"shm": "<映射名>"} 即透传（本机验证：loopback noise 全链路见 bringup-20261024-vcam-outlet）。
注意：读端禁止对只读映射用 LOCK 前缀指令（血泪见 bringup）；torn-read 回退重复上一帧（D12）。
- 布局/协议唯一真相：`vcam/vcam_shm.h`（v2，2026-10-28，D15）。v1↔v2 不互通：receiver 与 vc-vcam.dll 必须一起更新，
  下游（OBS/ffmpeg）重开才会加载新 dll；旧 v1 映射被占用时新 receiver `--shm` 报 "shm open failed"，先关旧读端。
- vcam 事件驱动：新帧到即交付（vseg≈0.03ms）；静默 >6.25ms 才按 240Hz 重复上一帧；样本时间戳取图时钟（Run tStart 为零点）。
  源本身的抖动会原样透出（ffmpeg 偶见 "non monotonically increasing dts"，同 ms 内到两帧时出现），不是丢帧。
- allocator 2 缓冲：下游最多积压 1 帧；OBS 采集源建议关闭"缓冲"。
- 延迟回写：vcam 每交付一帧写 `<映射名>.vcam` 环，receiver stats 的 `vseg_*` 即"receiver 发布→vcam 交付下游"。

## 注意事项
- 卸载前先注销（/u），再删 dll；重建后需重新 regsvr32（路径不变则覆盖即可）。
- CLSID：`{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}`，FriendlyName：`VirtualCapture`（旧名 `VirtualCapture 640x640 NV12`，2026-10-25 起改）。
- 历史坑：`AMPROPSETID_Pin` GUID 尾部曾手写错（应为 `B3F1-00AA003761C5`），导致所有 client 查 pin category 失败、
  ffmpeg/OBS 显示 `(none)` / `Could not find output pin`；bringup-20261023-vcam 里"ffmpeg quirk"的结论是误判。
