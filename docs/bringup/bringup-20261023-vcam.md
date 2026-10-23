# bringup-20261023-vcam（P4-3 DShow source filter 本机联调）

本机（dev 机，非 PC2）：`vc-vcam.dll` 构建 → HKCU 注册 → 枚举 → 建图推流 3s 全绿。
帧源为本地整数 motion（`sender/pattern.h`，与收发端同源）；receiver 共享内存喂帧是 PC2 接线步骤（P4-4）。

## 构建
- `cmake -S . -B build && cmake --build build --config Release`：`vc-vcam.dll` 一次通过
  （修过 4 个新手问题：UNICODE 定义、QueryVendorInfo 缺失、INITGUID 链接、UV 平面越界见下）。
- hand-rolled COM：无 ATL、无 strmbase（新 SDK 已移除），仅 `dshow.h + strmiids + winmm`。

## 注册与枚举
- `regsvr32 /s build/Release/vc-vcam.dll`（HKCU only，免管理员，`Version independent`）rc=0。
- `ffmpeg -f dshow -list_devices true -i dummy`：
  `"VirtualCapture 640x640 NV12" (none)`，与 OBS Virtual Camera 同列（同为 `(none)` 分类）。
- probe（临时，自删）：moniker 绑定 → `EnumPins/Next` → `QueryDirection=PINDIR_OUTPUT` →
  `IKsPropertySet=PIN_CATEGORY_CAPTURE` → `IAMStreamConfig GetFormat = 640x640 NV12 AvgTimePerFrame=41666` 全 S_OK。
- `regsvr32 /u` 可注销（见 `docs/runbooks/vcam.md`）。

## 推流（Filter Graph → NullRenderer，motion 帧源，3s）
- `ConnectDirect` S_OK；`Run → Sleep(3s) → Stop`，rc=0，无崩溃。
- DLL 内 tracer（事后已删）Stream 标记 12 个（每 60 帧）：fid=1..661，
  时间戳严格 `fid*41666`（如 fid=61 → 2541626），`Receive` 全 S_OK。
- 实测约 220fps（Sleep(4) 步进+开销），bring-up 阶段可接受；240 对齐待 PC2 实测。

## 修过的 bug（已进最终代码）
- UV 平面声明误写 `UV[640*320*2]`（409600）且按 `sizeof` 拷贝 → 堆越界崩溃；
  改为 `VC_Y_BYTES/VC_UV_BYTES` 精确平面（tracer 定位到 memcpy 行）。

## 给 P4-4 的 client 结论（重要）
- 本机 ffmpeg（WinGet 版）`list_options`/开流对**自家实现和 OBS Virtual Camera 同样报
  `Could not find output pin`**：ffmpeg 用 property set `{9B00F101-1567-11D1-B3F1-...}`（非标准
  AMPROPSETID_Pin 的 B3A9 尾）查 pin category，两家都不匹配。属该 ffmpeg 构建的 client 侧 quirk，
  不是 filter 缺陷。P4-4 出口验收改用 GraphEdit/GraphStudioNext 或 SampleGrabber 式自研 client，
  不以该 ffmpeg 版本为判据（已在 P4-4 卡备注）。

## 未做（后续）
- 帧源切 receiver 共享内存（PC2 接线，P4-4）。
- `FilterData` 注册值（多数 client 不需要，按需加）。
- 240fps 精确起搏（PC2 实测后再定；当前 Sleep(4) 约 220fps）。
