# P4-3 虚拟采集卡选型报告（先选型后实现；本文件只选型，不写代码）

## 1. 需求（PLAN P4-3/P4-4 原文转述）

- PC2 虚拟采集卡：NV12 640x640，AvgTimePerFrame=41666（100ns 单位，即 240fps），只留最新 1 帧，不跟墙钟。
- 推理端联调：枚举到虚拟设备、逐字节无损（与发端 hash 对账）、FIFO 堆积为 0。
- 先出 DShow vs MF 选型报告（MF 可见性、推理端开卡方式），批后实现。

## 2. 选项 A：DirectShow source filter（用户态 COM）

- 用户态 COM filter（regsvr32 注册），无需内核驱动、无需驱动签名（OBS 虚拟机、UnityCapture 均此路线）。
- 可见性：DShow 应用、Chromium 系（含开 DShow 回退）、Zoom/Teams/Discord/OBS 均可枚举；纯 MF/UWP 应用看不见（例：Win11 自带相机应用用 MF API，看不见任何 DShow 虚拟机——这是平台限制，不是某一家的问题）。
- 240fps：DShow 时间戳由 source 自增，可按 41666 推进；下游是否真按 240 取决于消费端（多数按需取样，只留最新帧在我们这端保证）。

## 3. 选项 B：Media Foundation virtual camera（用户态，Win11）

- MFCreateVirtualCamera（Win11 22H2+；Win10 无等价物）注册用户态虚拟机，无需驱动签名；相机生命周期与注册应用绑定，系统级可见。
- 可见性：MF/UWP/浏览器（Chromium 默认 MF 优先）可见；DShow-only 老应用可能看不见（与 A 互补）。
- 与 Windows Frame Server 集成，走标准 MF 采集管线；时间戳同样由 source 自增，不跟墙钟。

## 4. 对比（决策只看三行）

- 推理端用什么 API 开卡，决定选型：DShow 系（OpenCV 默认后端、OBS、老采集程序）→ 选 A；MF/UWP 系 → 选 B。
- PC2 系统版本决定 B 是否可选：Win10 只能 A（Win10 无 MF 虚拟机等价物）；Win11 两者皆可。
- 管理成本两者相当（都是用户态注册，无内核签名负担）；240fps 下游节流风险两者共有，靠 只留最新 1 帧 + 开卡实测交付 fps 对冲。

## 5. 推荐

- 先回答两个问题再定（见 §6 问 1/问 2）；在答案未知时默认推荐 A（DShow）：覆盖面最广（浏览器/OBS/Zoom/Discord/OpenCV 默认后端全吃），Win10/Win11 通吃，无系统版本门槛。
- 若推理端明确走 MF/UWP 且 PC2 为 Win11，改选 B；双栈并存不推荐（两套注册/两套测试，维护翻倍，P4-4 对账口径也会分裂）。

## 6. 待 owner 拍板（4 问）

1. 推理端用什么开卡（DShow / MF / OpenCV 哪路后端 / 其他）？
2. PC2 系统版本是 Win10 还是 Win11？
3. 是否接受默认 A、对 B 只做备选（或反之）？
4. 240fps 下游节流若实测不达标，是否接受降帧交付（仍只留最新帧）还是一票否决？

## 7. 不碰项与参考

- 本文件不写代码不动现网；主 agent（18080）与 PC2 零接触。
- 参考：MS Learn IMFVirtualCamera（learn.microsoft.com），OBS 虚拟机 DShow 路线说明，MFCreateVirtualCamera 需 Win11 说明。
