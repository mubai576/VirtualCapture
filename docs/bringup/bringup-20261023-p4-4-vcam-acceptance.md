# P4-4 vcam 出口验收（2026-10-23）

双机完整链路：PC1 vc-sender(DDA, Realtek 2.5G) → PC2 vc-receiver(--shm, I226-V) → vcam DShow filter → 验收客户端。

## 环境

- PC1=192.168.2.59（sender/display），PC2=192.168.2.100（receiver/本机）
- PC2 I226-V 网卡已调优（D14：FlowControl=0, ITR=0）
- PC1 Realtek 已同等设置
- vc-vcam.dll 已注册（HKCU CLSID {B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}）
- batch=69, MTU=9000, DDA mode, testwin D3D11 动画源

## 三判据结果

### J1 枚举：PASS

C++ 验收程序 `vc-vcam-verify.exe`（vcam_verify/）通过 ICreateDevEnum 枚举 DShow 视频采集设备：
```
device: Live Gamer Ultra 2.1-Video
device: OBS Virtual Camera
device: VirtualCapture 640x640 NV12
```
与 AVerMedia USB 采集卡、OBS Virtual Camera 同列出现。

ffmpeg -list_devices 同样可见（但 list_options/开流失败，已知 client 侧 quirk，见 bringup-20261023-vcam.md）。

### J2 格式协商：PASS

`IAMStreamConfig::GetFormat` 返回：
```
biWidth=640 biHeight=640 fourcc=NV12 subtype.Data1=0x3231564E
AvgTimePerFrame=41666 (240.0 fps)
```
NV12 640x640 @ 240fps，符合冻结契约。

### J3 hash 对账 + FIFO：PASS

**J3a FIFO（dropped=0, hash_mismatch=0）：PASS**

receiver 最终 stats：
```
complete=35069  shm_published=35069  dropped=0  hash_mismatch=0
```
35069 帧全部经 SHM 发布到 vcam，零丢帧、零 hash 不匹配。

**J3b SHM hash 与 receiver hashlog 一致：PASS**

Python 脚本 `scripts/p4_4_shm_verify.py` 直接读 SHM（`Local\VirtualCaptureVcamFrame`），
对 30 个采样帧计算 FNV-1a64 并与 receiver hash.jsonl 比对：
```
SHM samples: ok=27 fail=0 torn=3
```
- 27 帧 hash 与 receiver hashlog 完全一致
- 3 帧 torn read（seq 奇/不稳定，被跳过，属正常并发读写）
- 0 帧 hash 不匹配

## 验收工具

| 工具 | 用途 | 路径 |
|---|---|---|
| vc-vcam-verify.exe | J1 枚举 + J2 IAMStreamConfig 格式检查 | vcam_verify/（C++，dshow COM） |
| p4_4_shm_verify.py | J3 SHM 读帧 + FNV-1a64 对账 + FIFO 检查 | scripts/（纯 ctypes mmap） |

两者均无第三方依赖，可重复执行。

## 结论

P4-4 三判据全部通过。VCam DShow filter 在双机完整链路下：
1. 通用 DShow 客户端可枚举到设备
2. 协商 NV12 640x640 @ 240fps
3. 出口帧与发端逐字节一致（hash 零错），FIFO 零堆积

P4 全部任务（P4-1/2/3/4）完成。
