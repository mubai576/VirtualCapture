# bringup-20261028 — 端到端延迟优化 1+2 组（D15）

本机回环（PC1，Npcap Loopback），`--mode pattern --pattern motion --fps 240 --batch 1 --frames 2400`，
receiver `--shm Local\VirtualCaptureVcamFrame`，新版另挂 ffmpeg dshow 消费 `VirtualCapture`。
**回环数据，不代表双机结论**（硬规则 3）；PC2 部署后需按同口径重测。

| 段（同机 QPC） | 旧（run/baseline-lat，改前二进制） | 新 |
|---|---|---|
| pc1_seg avg/p99/max（生成完成→末包发出） | 1.029 / 1.321 / 1.740 ms | 0.373 / 0.426 / 0.970 ms |
| rseg avg/p50/p99/max（首包→重组完成） | 2.342 / 2.233 / 4.426 / 5.580 ms | 0.388 / 0.359 / 0.516 / 4.367 ms |
| vseg avg/p50/p99/max（receiver 发布→vcam 交付下游） | 未测；240Hz 轮询理论均值 +2.1ms、最差 +4.2ms | 0.033 / 0.030 / 0.078 / 0.222 ms |
| complete / dropped / mismatch | 2400 / 0 / 0 | 2400 / 0 / 0 |

二次复跑：pc1 0.370/0.417/0.533，rseg 0.384/0.357/0.513/4.372，vseg 0.032/0.031/0.074/0.150。

## 改动
- sender：整帧 FNV 移到末包之后；包载荷直写以太帧缓冲（去掉每包 vector）；sendqueue 复用（重置 len，免每批 2MB alloc/free）；
  CRC32 slice-by-8（selftest 对逐字节参考 72 例 + 标准向量 cbf43926）；MMCSS "Capture"；dda 不再按 fps 睡眠；
  CPU 路径 `CopySubresourceRegion` 只拷中心 640²（CPU 路径哈希与旧版逐帧一致：82f643bb…/9efd5e44…）；新增 `cap_lag_*`。
- receiver：shm v2 原地重组（零整帧拷贝、无每帧 614400B 分配清零）；发布 = 翻槽 seq + 事件；哈希/图案比对/hashlog 移到哈希线程
  （`hash_queue_max`=1）；MMCSS；读 vcam 回写环出 `vseg_*`。
- vcam：事件驱动；shm→sample 单次拷贝；图时钟时间戳；allocator 3→2；worker 线程 MMCSS（不改宿主进程优先级）。
- 顺带修：`qpc_ns_now()` 的 `c*1e9/freq` 在开机约 30 分钟后溢出（旧版 hashlog qpc_ns=711042895862，新版 188865717480800）；
  这也是 receiver stats 出现 `uptime_s=18446743687` 的根因。

## 验证
- `python scripts/verify.py` ALL PASS；`--no-build --live Loopback` 25 passed / 1 skipped。
- `scripts/p4_4_shm_verify.py`（已切 v2 读端）：noise 240fps，30/30 一致，J3a/J3b PASS。
- `vc-vcam-verify.exe` J1/J2 PASS。

## 观察 / 未决
- ffmpeg 偶报 `non monotonically increasing dts`：dshow 按 ms 打戳，同 1ms 内到两帧即冲突；是源抖动原样透出（旧版 240Hz 轮询把它吞成了重复/跳帧）。
  如需平滑可加"最小间隔"选项，未做。
- dda 实测（本机静止桌面 ~65fps 出帧）：cap_lag avg 2.49 / p99 9.93 / max 24.6 ms，主要是 DWM present→AcquireNextFrame 返回的系统侧等待，需 PC1 2K240 动画源复测。
- 运行中的 OBS 仍持有旧 vc-vcam.dll（v1，读 v2 映射会判无信号）；重开 OBS 后生效。

## 双机实测（2026-10-28，0.4.2）
拓扑（当前实际）：**PC2 192.168.2.59 发**（Realtek 2.5G，sender 经 agent `/sender/start`）→ **PC1 收**（I226-V，`vc-receiver --shm`）→ PC1 vcam → ffmpeg dshow 消费。
参数：pattern motion，240fps，7200 帧，batch 69，MTU 9000，单播 dst 34:5A:60:4F:F2:33。PC2 sender session `s-78f65fa8`，PC1 receiver `d42-rx`。

| 段（同机 QPC） | 值 | 对照（D14 后旧版双机，PLAN §0） |
|---|---|---|
| PC2 发端段 avg/p99/max | 2.187 / 2.302 / 3.665 ms | pc1_seg_p99 3.14 ms |
| PC1 rseg avg/p50/p99/max | 1.939 / 1.939 / 2.054 / 2.263 ms | rseg_p99 6.70 ms，rseg_max 35 ms |
| PC1 vseg avg/p50/p99/max | 0.034 / 0.032 / 0.083 / 0.375 ms | 旧 240Hz 轮询，理论 +2.1 ms 均值 |
| complete / dropped / gaps / lost / mismatch | 7200 / 0 / 0 / 0 / 0 | |
| 逐帧对账（PC2 sender hashlog 已拉回 run/s-78f65fa8.sender.hash.jsonl） | 7200/7200 一致 | |

发端段 ≈2.2ms 的主体是 batch 69 整帧 sendqueue 的线速时间（614400B@2.5G≈2.0ms），已到物理下限。
vcam_repeats=1927 来自 ffmpeg 开流早于发端约 8s 的静默期（按 240Hz 重复上一帧），不是丢帧。

部署记录：PC2 原运行 Oct-22 冻结 vc-agent.exe，白名单无 vc-vcam.dll → 先推 0.4.2a（重打 PyInstaller agent，源码未改）→ 0.4.2（sender/receiver/vcam）→
0.4.2c（新增 scripts/vcam_register.ps1，HKCU regsvr32，dll 落在 ROOT\build\）。PC2 dll sha256 1A5DBBFC… 与本机一致。
另：先误按"PC1 发 / PC2 收"跑了一轮（PC2 无 I226，receiver 未起），7200 帧发往 PC2 MAC 无人接收，无副作用，数据作废。
