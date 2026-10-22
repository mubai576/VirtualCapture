# 2K240本机显示 + 640x640@240 NV12跨机采集方案

## Summary
* 目标：PC1保持`2K240`正常使用，只取`640x640@240 NV12`给PC2，性能以`640@240`为下线，之后只许转NV12一次，不许二次有损压缩。
* 链路：软采2K240 → GPU裁/缩到640 → RGB转NV12一次 → 2.5G网线直连L2裸发 → PC2重组 → 虚拟采集卡。

## Implementation Changes
* 采集端：DXGI抓2K240 backbuffer，GPU居中裁/缩到640，矩阵与真卡对齐，输出`NV12 12 bits per pixel`，单帧`width*height*3/2`即614400B。
* 传输：Npcap L2自定义EtherType如0x88B5，网卡不配IP，MTU 9000，`~69包/帧，16.5k pps，~1.2Gbps`；`decreases number of pps`，`Fewer packets per byte, lower CPU`；单帧不跨帧`Packets SHALL NOT include data from multiple frames`；`must use pcap to send RAW Ethernet`，经`WritePacketData`直写网线；`Defender does not see any open sockets`，`Below tcpip.sys only NDIS.sys`；跨路由不可达`not reliable path for arbitrary Layer-2 EtherType`故必须直连。
> SUPERSEDED-NOTE 2026-10-22: the no-IP rule in the line above is void (shared-NIC, PLAN S11). Budget numbers stand - measured ~1.18Gbps (S23).
* 接收+虚拟卡：按frame_id/timestamp重组，缺包整帧丢等下一帧；环形只留最新1帧；虚拟端暴露`YUY2,NV12,RGB24,P010,MJPEG`中的NV12 640x640 AvgTimePerFrame 41666，不跟墙钟，因`doesn't handle reference clock correctly`；DShow/MF二选一，DShow虚拟对MF不可见`DirectShow-only invisible to Media Foundation`。
* 弃用项：MJPEG`each frame is JPEG`且`JPEG is lossy`；NDI `~15:1 visually lossless`；Moonlight`H.264/H.265 feed`，均有损出局。

## Test Plan
* 带宽/pps：9000下`69包/帧`持续240Hz无丢，`102k pps`仅作反例验证1500不可用。
* 无损：NV12平面逐字节对比，RGB→NV12矩阵与真卡一致，无DCT。
* 延迟：采集到推理Read<4.16ms，抖动<1帧，FIFO堆积为0。
* 兼容：推理枚举到虚拟NV12 640@240，`raw-injected frame does not reach local socket`故必须双机联调。

## Assumptions
* 双Win，2.5G直连，全链9000，Npcap可装；NV12的420下采样可接受，之后零压缩；推理开卡方式DShow/MF/私有SDK待设备到手确认；640为居中裁默认。
