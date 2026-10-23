# vcam runbook（P4-3 DShow 虚拟采集卡）

产物：`build/Release/vc-vcam.dll`（用户态 source filter，无需驱动签名）。
输出：NV12 640x640，AvgTimePerFrame=41666（240fps），单输出 pin `Out`。
当前帧源：本地整数 motion（`sender/pattern.h`）；receiver 喂帧是 P4-4 接线事项。

## 构建
cmake -S . -B build
cmake --build build --config Release   # 产物 build/vc-vcam.dll（VS 生成器下在 build/Release/）

## 注册 / 注销（HKCU，免管理员）
regsvr32 build/Release/vc-vcam.dll     # 注册当前构建（HKCU\...\CLSID\{B84C7E21-...} + VideoInputDeviceCategory 实例）
regsvr32 /u build/Release/vc-vcam.dll  # 注销
# 验证注册：ffmpeg -f dshow -list_devices true -i dummy | 找 "VirtualCapture 640x640 NV12"
# 注意：本机 WinGet 版 ffmpeg 打不开虚拟机（对 OBS 同样失败，见 bringup-20261023-vcam）；
# 开流验证用 GraphStudioNext/GraphEdit 或自研 SampleGrabber client，不用该 ffmpeg 判流。

## receiver 喂帧（P4-3 通路，PC2 用法）
receiver --shm <映射名>      # 每完成一帧即发布最新完整帧（只留最新 1 帧）
vcam 默认读 Local\VirtualCaptureVcamFrame；改名：在 HKCU\...\CLSID\{B84C7E21-...} 下写 REG_SZ `FrameMap`。
agent /receiver/start 传 {"shm": "<映射名>"} 即透传（本机验证：loopback noise 全链路见 bringup-20261024-vcam-outlet）。
注意：读端禁止对只读映射用 LOCK 前缀指令（血泪见 bringup）；torn-read 自动 fallback 本地 motion。

## 注意事项
- 卸载前先注销（/u），再删 dll；重建后需重新 regsvr32（路径不变则覆盖即可）。
- CLSID：`{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}`，FriendlyName：`VirtualCapture 640x640 NV12`。
