# P4-1 GPU 提案（待 owner 批；本文件只提案，不改代码）

## 1. 现状路径与代价（sender/dda.h，sender/vc_sender.cpp）

- 每帧：AcquireNextFrame(500ms) → 格式 B8G8R8A8 与尺寸检查 → 整帧 CopyResource 到 staging → Map（同步点）→ CPU dda_bgrx_to_nv12（居中裁 640 + 整数 BT.601，全程无浮点）→ Unmap → ReleaseFrame。
- 代价三块：整帧 GPU 拷贝（2K 约 2560x1440x4，每帧约 14MB）+ Map 同步 stall + CPU 双循环（Y 640x640，UV 640x320，逐字节整数运算）。
- 现状测量：vc_sender.cpp:492 把整段 capture() 耗时累进 gpu_accum_ms，经 stats 报 gpu_ms——当前是 wall-clock，不是 GPU dispatch 时间。
- 正确性锁已存在：vc_sender.cpp:324-330 有 white/red 精确向量单测（白帧 Y 全 255、UV 128；红帧 Y=76），GPU 路径直接复用为逐字节一致判定。
- 错误显式化：E_NODDA/E_FULLSCREEN/E_FORMAT/E_SMALL/E_LOST/E_TIMEOUT/E_INTERNAL/E_NOOUTPUT，无静默黑帧；D4 无屏 SKIP 语义保持。

## 2. 目标（PLAN P4-1 原文转述）

- DDA 2K240 → GPU 居中裁 640 → GPU RGB→NV12（整数矩阵，与 CPU 参考逐字节一致）。
- 240fps 下 gpu_ms<4ms、不丢采集帧；为 T5 定独立门限写入 test-matrix（需 owner 批）。
- 无屏/独占全屏仍须显式报错，不许静默黑帧。

## 3. 推荐方案 A（compute shader 直出 NV12 小 buffer）

- 对 AcquireNextFrame 拿到的 texture 起一个 compute dispatch，一次做完居中裁剪 + 4:2:0 下采样 + 整数 BT.601（系数沿用 77/150/29、-43/-84/127、127/-106/-21，右移 8，加 128，clamp8），输出写两块小 GPU buffer（Y 640x640 + UV 640x320 交错，共 614400B）。
- staging 从整帧缩小到 614400B，Map 一次读回；CPU 转换循环归零；Acquire/Release 语义与 E_* 分类不动。
- 不做：双 staging 交叠、常驻映射——若 A 实测达不到 <4ms 再议方案 B。

## 4. 测量与 T5 门限提案（请批数字）

- gpu_ms 口径：沿用 stats 口径 = capture() wall-clock 均值/帧；另增 gpu_dispatch_ms（GPU 时间戳包住 crop+convert dispatch）只做分析，不进判据。
- T5 候选门限：240fps 下 gpu_ms 均值 <4ms 且 p99 <4.16ms；E_TIMEOUT/E_LOST 计数为 0（计入 dropped，不静默）；采样帧 GPU 输出与 CPU 参考逐字节一致（含 white/red 向量 + 至少 3 帧真实桌面）；无屏/独占仍显式报错。
- 预算提醒：4ms 目标相对 4.166ms 帧预算只剩约 0.16ms 给组包发送——若实测组包超支，门限或流水线需再议，先按此测拿数据。

## 5. 待 owner 拍板（6 问）

1. 桌面非 2K 时居中裁逻辑不变（不小于 640 即可，不足已有 E_SMALL），是否接受？
2. 240fps 下 DDA AcquireNextFrame 本身能否跟上（与 GPU 无关的采集侧风险），要不要先做纯采集 fps 摸底？
3. gpu_ms 口径：wall-clock 进判据、dispatch 只做分析，是否同意？
4. GPU 路径失败是否允许回退 CPU——提案不回退只报显式错（回退会掩盖问题），是否同意？
5. T5 上述门限数字是否批？
6. Map/Unmap 先按同步做（简单），双 staging 交叠以后再说，是否同意？

## 6. 不碰项

- proto 包头、test-matrix 现有判据、主 agent（18080）与 PC2：本提案零接触。
- D4 SKIP 语义、整数无浮点约束保持。

## 7. owner 批示 2026-10-22

- 不回退 CPU：同意。GPU 路径失败只报显式错（与不允许静默降级原则一致）。
- 门限数字：暂不批。先把测量改对（当前 gpu_ms 是 wall-clock，需改为 GPU 实际执行时间），拿到实测分布后再定数字。
