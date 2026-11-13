# PC1 Raw 长运行卡顿：候选修正、诊断与隔离验证

日期：2026-11-08。Owner 已批准实现独立采样、生命周期候选、分项计时和隔离验证。

**编译/本机验证已完成；稳定部署未替换；六小时现场记录已启动但未完成。没有宣称数小时整机卡顿已经解决。**

## 来源与边界

- 稳定成品：`dist/VirtualCapture/` 0.4.1。对照源码 `archive/baseline-20261102-230617/source/worktree/`。本轮结束核对成品清单 87 项，全部 SHA-256 一致。
- 当前开发工作区有既存未提交的 P6/NVENC/MJPEG 改动。开始前保留 `run/longrun-20261108/before-status.txt`、`before-worktree.diff` 及本轮涉及的原文件。没有提交、打绿色包、OTA、修改网卡或重启驻留服务。
- 候选目录：`run/longrun-20261108/candidate-build/`。这是**当前开发源码隔离构建**，含既有开发能力，不冒充稳定成品的单一补丁发布。未修改/注册/覆盖稳定 DLL，不自动复制到 dist 或 PC2。
- 当前源码在本轮开始前已经采用 pin 外部引用委托 filter、filter 直接拥有 pin、graph 弱引用的所有权。旧版 DLL 尚未含这些修正；本轮将实际候选编出并验证，同时修复下面的停止和模块生命周期遗漏。

## 实际改动

1. `vcam/vcam.cpp`：串行化 Run/Pause/Stop/Disconnect；Disconnect 先保留连接执行 BeginFlush/Decommit/线程退出，再移除 allocator/input/peer，修复先清指针导致无法解除等待的问题。stop event、线程句柄、映射在工作线程真正退出后回收。补 allocator Commit/事件创建失败清理。工厂和脱离 filter 的媒体类型枚举器纳入 DLL 存活计数，外部仍持接口时不误报可卸载。
2. `common/diagnostic_timing.h`、Raw receiver、vcam worker：可选 `VC_DIAG=1`、固定 4160B/32 lane 的旁路映射；记录 stats_prepare、stats_write、hash_write_flush、sample_getbuffer、downstream_receive 的累计/最大/最近/慢调用/失败及当前 pending。生产热线程无新增日志写盘；未启用时不查询 QPC、不创建诊断映射。既有 stats、rseg/vseg、Raw/协议/帧 SHM 均不重定义。
3. `scripts/longrun_diagnostics.py`、`longrun_windows.py`：每秒双端只读 HTTP stats、本机进程/下游自动发现、GPU、内核池、DPC/中断、磁盘延迟；有界状态与请求、缺失 provider/权限记明错误。原始样本全留，按 3600 条分卷，事件上下文前后各 60 样本，支持手动标记/仅结束记录器。停止业务与恢复动作由用户实施后标记。
4. `vc-vcam-lifecycle`、增强的 `vc-vcam-verify`、`scripts/longrun_verify.py`、pytest：直接加载 DLL，不注册；覆盖正常交付、Receive 阻塞、分配器耗尽，分别走 Stop 和直接 Disconnect，并验证自动发现及 pending 计时。

操作与 IPC 契约：`docs/runbooks/longrun-diagnostics.md`、skill `references/longrun-diagnostics.md`、PLAN §1.7。Raw hash/flush 批次、128MiB Npcap 请求缓冲和已有验收门限未改变；本轮没有未经证据的收包优化。

## 对照与生命周期实测

证据根：`run/longrun-20261108/`。

| 测试 | 结果 | 证据 |
|---|---|---|
| 旧稳定 DLL 100 次创建/释放 | 100 次 Release 都返回 1；私有内存 +61,988,864 B，约 59.1MiB；factory 释放后 DllCanUnloadNow=S_FALSE | `baseline-release.json` |
| 候选 DLL 1000 次 IUnknown 创建/释放 | 1000 次全部返回 0；首次使用分配器后总增量 602,112 B（约 0.57MiB）；全部释放可卸载 | `candidate-release.json` |
| 候选预热 32 次后再 1000 次 IBaseFilter 创建/释放 | private_delta=0 B；全部 Release=0 | `regression-final/lifecycle.log` |
| 保留外部 pin、pin 枚举器/clone、媒体类型枚举器、factory | 正确保活；对应外部接口释放后允许卸载，无悬空 filter | 同上 |
| 50 次真实 graph + 系统 Null Renderer 连接后销毁 | graph 退出时正确解绑，最后 DllCanUnloadNow=S_OK | 同上 |

可复现命令：`python scripts/analysis/vcam_release_probe.py --dll <DLL> --count 100`，以及 `vc-vcam-lifecycle.exe <DLL>`。泄漏对照只在隔离子进程运行，退出由 OS 回收，不双重 Release、不强行卸载仍有对象的旧 DLL。

## motion/Raw 本机回归

统一：640x640 NV12、配置 240fps、motion、batch=1、adapter=Loopback、每组发送 720 帧。六组每组均接收 720 帧，无缺失/重复接收/哈希不匹配；完整发送、接收和虚拟卡样本 hash 均保存。

| 用例 | 观察到的阻塞 | Stop/Disconnect 实测 |
|---|---|---|
| 正常，诊断开 | 五项计时齐全，自动发现 DLL | 0.423ms |
| 正常，诊断关 | 无诊断 lane；交付 hash 仍正确 | 0.830ms |
| Receive 阻塞后 Stop | downstream_receive pending >100ms | 0.225ms |
| Receive 阻塞后直接 Disconnect | 同上 | 0.206ms |
| 留住两个样本使 GetBuffer 阻塞后 Stop | sample_getbuffer pending >100ms | 0.864ms |
| GetBuffer 阻塞后直接 Disconnect | 同上 | 0.914ms |

正常组的虚拟卡样本为 1176/1180 个，均为 720 个已接收帧的 hash；发送停止后的重复好帧为既有 D12 行为，证据明确区分样本数和唯一 hash。阻塞组只交付少量帧是有意制造下游背压，不能把样本数包装成有效高帧率。

证据：`regression-final/results.json`、各组完整 `tx/rx/vc.jsonl`、`resources.jsonl`、原生日志、`binaries.json`。这是本机正确性/生命周期验证，不是双机吞吐、六小时稳定性或实际显示端到端延迟结论。顺序执行回环，未与另一套 live 回环混跑。

另直接启动**未更改的稳定 EXE**，连接候选 DLL 到独立 SHM v2：同样 motion/batch1/Loopback，720→720 帧、hash 全一致，候选收到 1177 样本/720 唯一 hash，最终可卸载。证据 `legacy-interop/result.json` 及完整日志。该测试只证明本机旧版 Raw/SHM 兼容，不代替实际下游验收；未访问驻留 agent。

## 完整检查

- `python scripts/verify.py`：404 passed / 6 skipped；sender/receiver 自测、pattern Python 对账通过。第一次构建暴露 SDK 已无 qedit.h NullRenderer 声明，改用已核验的系统 CLSID 后重跑通过，保留失败日志。
- 最终 `python scripts/verify.py --live Loopback`：**408 passed / 2 skipped**，包含最终源代码的构建和 live 用例；`verify-final.log` 为 ALL PASS。
- 候选最终独立构建、生命周期和六组回归均重新运行，见 `candidate-final-build.log`、`regression-final/`。
- 采样器实机读到 CPU/DPC/中断/磁盘与内核池；GPU PDH provider 可用，实测返回 36 个进程实例。模块检查存在 9 个访问受限进程及个别快退出进程，记录中显式保留这些限制。
- `stable-hashes-after.json`：87 个稳定载荷全部匹配，无变更；不存在自动部署或稳定版“已经生效”的结论。

## 六小时现场状态

- 已启动隐藏只读记录器 **PID 25976**。
- 目录：`run/longrun-20261108/observe-6h-204449/`。
- 开始：**2026-11-08 20:44:50 +08:00**；计划 21600 秒，约 **2026-11-09 02:44:50 +08:00** 自然结束。
- 启动后已确认样本持续增长、stderr 为空；约 3 分钟时工作集约 33MiB、236 个句柄。最终交付时的最新健康核验另存 `observation-health.json`，不把该短时观察说成六小时完成。
- 启动核验时 `.59:5143` 与 `127.0.0.1:5143` stats 超时、未看到正式使用中的下游。采样仍记录本机系统和自身资源；接口上线后自动接续。前期含最后一次隔离兼容回归，已经写明人工时间标记，与实用故障分开。
- 同目录已生成 GBK/CRLF 中文操作脚本：`标记卡顿.cmd`、`标记已停采集.cmd`、`标记已关下游.cmd`、`结束记录.cmd`。前三个只记时间，第四个只结束记录器；都不会替用户控制业务程序。已验证中文编码往返与实际 marker 写入。

现场记录与触发窗口后续保留在原目录。当前已用版没有旁路计时：要观察这五项原生细分，须经后续明确维护切换，使候选 receiver 和加载候选 DLL 的下游进程以 VC_DIAG=1 启动；本轮不擅自切换。

## 尚未证明

没有复现连续使用数小时后的整机恶化；没有取得异常时的实际下游业务负载/显存增长/驱动证据。因此现在只能确认旧 DLL 每创建一次泄漏、候选释放/停机测试通过、诊断能力已具备并开始现场采样。下一步根据“开始变卡”时间标记判断资源增长、写盘或下游阻塞哪个先发生；不把关闭软件后恢复单独视为因果证明。

下游不响应 BeginFlush 仍可能让 Stop 等待；为避免后台线程访问已释放内存，不恢复旧版的超时后强行关闭句柄。当前测试覆盖遵守 DirectShow flush 的阻塞下游。
