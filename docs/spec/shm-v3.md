# NV12 帧共享内存 v3

这是 receiver → DirectShow 的 IPC 契约，不属于冻结的 28B 网络包头。实现与布局测试在
`rust/crates/vc-stream/src/shm.rs`（写端）和 `rust/crates/vc-vcam/src/shm.rs`（读端）。
旧 C++ `vcam/vcam_shm.h` 已移除，历史版本可从 Git 查询。

## 布局（Windows 小端）

默认映射名 `Local\VirtualCaptureVcamFrame`。单写进程、多个只读读端；会话几何在映射存续期内不变。
映射总字节数 `4096 + 12 × frame_bytes`，`frame_bytes = width × height × 3 / 2`。

### 64B 头（偏移 0）

| 偏移 | 类型 | 字段 / 值 |
|---|---|---|
| 0 | u32 | magic = `0x33414356`（VCA3） |
| 4 | u32 | version = 3 |
| 8 | u32 | slots = 12 |
| 12 | u32 | frame_bytes |
| 16 | u32 | data_off = 4096 |
| 20 | i32 | latest_slot，-1 表示从未发布 |
| 24 | i32 | latest_seq，发布计数，初值 0 |
| 28 | u32 | writer_pid |
| 32 / 36 / 40 | u32 | width / height / fps |
| 44 | 20B | 保留 |

每个槽的元数据位于 `64 + i × 64`，像素位于 `4096 + i × frame_bytes`。

### 64B 槽元数据

| 槽内偏移 | 类型 | 字段 |
|---|---|---|
| 0 | i32 | seq，奇数正在填充，偶数稳定 |
| 4 | u32 | frame_id |
| 8 | u64 | qpc_first_ns，收端处理本帧首片时刻 |
| 16 | u64 | qpc_pub_ns，收端发布时刻 |
| 24 | u64 | hash，整帧 FNV-1a64 |
| 32 | u32 | hash_fid，hash 对应的 frame_id |
| 36 | 28B | 保留 |

宽高为 2..8192 的偶数，fps 为 1..1000；字节数、槽数和 data_off 必须一致。
新读端兼容 magic=VCA2、version=2、固定 640×640@240 的旧布局；旧读端拒绝 v3。
writer 拒绝复用已有同名映射，不能接管其他会话。

## 发布与只读 seqlock

1. 写端从非 latest、非填充、非哈希占用的槽中按 LRU 选空闲槽；seq 变奇数，hash_fid 置无效值 `0xffffffff`。
2. 直接向槽中重组/解码，完整成功后填 frame_id 与 QPC；失败则放弃，绝不发布部分像素。
3. seq 变偶数 → 写 latest_slot → 增加 latest_seq。发布后在延迟路径之外完成哈希，再写 hash 和 hash_fid。
4. 令发布计数为 n，先 ResetEvent(evt[(n+1)&1])，后 SetEvent(evt[n&1])。
   事件名 `<映射名>.evt0/.evt1`，均为 manual-reset，支持多读端。

读端映射只读，**不得使用 LOCK 前缀或读改写原子指令**。读取 latest_seq/slot 与槽 seq=s1，
奇数则跳过；复制元数据和像素后再读 seq=s2，只有 s1==s2 且稳定才接收。
按 `(slot, seq)` 去重，不按可能变化的 latest_seq 去重。只有复制边界内的 hash_fid==frame_id
且 hash 标记不是无效哨兵时才把 hash 当成证据；frame_id=`0xffffffff` 与哨兵冲突，不得误认旧 hash 有效。
等待下一发布用 evt[(latest_seq+1)&1] 加有界超时；漏边沿不能无限阻塞。

D12：从未有好帧时不输出样本；已有好帧后停流/撕裂可重复最后好帧，不生成假运动。
DShow 实例创建时固定会话格式；改尺寸后下游须关闭并重新打开。

## 虚拟卡 → receiver 反馈环

映射 `<帧映射名>.vcam`，只用于同机测量。头为 16B：magic=`0x54535656`（VVST）、
widx:i32、cap:u32=1024、reserved:u32。随后 1024 个 32B 项：
`n:u32, frame_id:u32, pub_ns:u64, deliver_ns:u64, flags:u32, reserved:u32`。

写端原子预约 k，先 n=0，填字段，最后 n=k+1；读端只接受读取前后 n 均为 k+1 的项。
flags bit0 表示重复交付。deliver_ns 在调用下游 Receive 之前取值；
vseg=deliver_ns−pub_ns，只统计同机非重复发布，不代表跨机端到端延迟。
QPC 转 ns 采用商余分解，避免直接 `counter × 1e9` 溢出。
