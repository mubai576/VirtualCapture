# P6-3 Windows MF 编解码后端原型（2026-11-03）

## 当前状态

不再只有下拉框或MFT候选枚举：`common/mf_codec.h`已实现显式后端选择、同步/异步MFT事件、
NV12输入、H.264/HEVC访问单元输出、解码、stream-change、CBR/低延迟请求、强制IDR、drain与有界排队。
`vc-codec-roundtrip.exe`实际运行motion→编码→复制压缩字节到新sample→解码→NV12，
记录源/解码hash、luma MAE与本机回环耗时。**没有套用有损前后像素相等门**。

这仍是开发原型，**没有接到sender/receiver压缩分包、解码后SHM或产品启动**，P6-3不得标DONE。
agent的hardware字段仍unprobed，H.264/H.265依旧409拒绝，不把开发机结果外推到PC2。

## 后端选择与依赖

- 仅Windows Media Foundation/系统SDK，不引入FFmpeg、NVENC私有SDK或新便携DLL。
- 必须显式传encoder/decoder CLSID；无CLSID的打包MFT可用精确friendly name，匹配必须唯一。
  硬件枚举类别也显式选择，失败不换codec/软件/设备或降低尺寸、fps。
- HEVCVideoExtension是本机已安装组件，不随便携包假定存在，也没有自动安装。
- `vc-codec-probe`只有候选/激活/媒体类型探测，固定frames_tested=0/stream_supported=false。
  某些打包MFT没有CLSID属性，探测输出null及HRESULT，不把GUID_NULL当有效身份。
- 开发目标与工具不在便携载荷白名单中；构建不等于发版或生产可用。

## 实际发现与尚待处理

本机RTX4070的NVIDIA H.264/HEVC编码器可编码；主机端Microsoft H264与HEVCVideoExtension可解码。
AMD注册项激活失败，H264软件编码器在当前严格媒体类型下拒绝输入；未绕过或静默选作后备。

1. NVIDIA编码器的AVLowLatencyMode/CBR/平均码率/强制IDR设置成功，async事件驱动。
   B-picture控制返回0x80070057，明确记录，不声称该旋钮可用；GOP控制成功。
2. H.264在单帧in-flight下输出；HEVC默认单帧诊断明确超时（encoded1/decoded0）。
   显式设置`--max-in-flight 2`后可完整回环，表明该解码路径至少多缓存一帧。
   不能把这个结果宣传成与Raw同延迟，更不能在产品里悄悄加深队列。
3. 解码面可能是宏块/CTU补齐尺寸：如HEVC720p给1280×736、1080p给1920×1088。
   只按显式display aperture取回精确会话NV12，尊重stride及UV平面偏移；不缩放/降尺寸。
   无精确、色度对齐aperture或缓冲容量错误时拒绝。有纯内存selftest防止复制到padding/错UV平面。
4. 20Mbps是请求的CBR目标，不是本原型证明的最大瞬时码率；短序列IDR峰值仍可能高于均摊预算。
   luma MAE仅诊断，没有未经owner同意建立画质PASS门；色彩信令、实际画质及码率窗口仍需独立验收。

## 开发命令

```powershell
cmake --build build --config Release --target vc-codec-probe vc-codec-roundtrip
build/Release/vc-codec-probe.exe --width 640 --height 640 --fps 60
build/Release/vc-codec-roundtrip.exe --selftest
build/Release/vc-codec-roundtrip.exe --codec h264 --width 640 --height 640 --fps 60 --frames 30 `
  --bitrate 20000000 --hardware-encoder --encoder-clsid '{60F44560-5A20-4857-BFEF-D29773CB8040}' `
  --decoder-clsid '{62CE7E72-4C71-4D20-B15D-452831A87D9D}' --hashlog run/h264-dev.jsonl
build/Release/vc-codec-roundtrip.exe --codec hevc --width 640 --height 640 --fps 60 --frames 30 `
  --bitrate 20000000 --max-in-flight 2 --hardware-encoder --encoder-clsid '{966F107C-8EA2-425D-B822-E4A71BEF01D7}' `
  --decoder-name HEVCVideoExtension --hashlog run/hevc-dev.jsonl
```

上述CLSID只对应本机已验证后端，不是对所有机器的默认选择。`fps`在此工具是媒体类型元数据，
测试不按墙钟节拍送帧；输出耗时不是实时60/240fps或跨机延迟证明。每次子进程还应由调用方设置总超时。

初次实际测试/失败记录保留于 `run/p6-codec-20261103/`；修正显式aperture后的最终矩阵为 `run/p6-codec-20261103-012310/matrix.json`：
H.264/HEVC各416²、640²、720p、1080p、1440p及418×282六组，**12/12组、360帧实际编码/解码通过**，
每组30帧、motion、20Mbps请求、fps元数据60，H.264最多1帧/HEVC最多2帧in-flight，各验证两次强制IDR。
1080p解码面1920×1088、HEVC720p为1280×736，均按精确aperture取回目标NV12，不是缩放。
这批成功只证明上述原型功能，不是2.5G传输、实时60fps、240fps或画质/最大码率验收。
成功要求真实编码/解码帧数、顺序/时间戳、输出尺寸/长度与强制IDR标记正确，不拿有损像素相等判定。

## 后续接线

- 编码访问单元封装、可变压缩长度/上限、传输字节完整性日志；28B包头仍冻结，不占保留flags。
- 接收端有界重组/解码、丢包丢整帧、等待IDR/刷新恢复；解码后继续给SHM v3/DShow NV12。
- 双端能力API的实际探测与明确后端选择，不能自动回退；分别核对PC2 encoder和PC1 decoder。
- 与原有Raw路径分开统计、门限和证据；长稳/恢复/画质/最大码率及实时吞吐未完成前不开放产品编码选项。
