# PC1 长时间卡顿现场记录

采样器独立于 GUI 和下游，只观察资源和现有 stats。默认连续 6 小时，不会启动/停止采集或更改注册表、网卡。

## 正常使用时记录

在本仓库目录运行（Windows Python，无额外包）：

```powershell
python scripts/longrun_diagnostics.py
```

控制台打印本次 `run/diagnostics-日期时间/` 目录。照常打开软件、开始 Raw。原生接收、虚拟卡所在下游进程会被发现；默认每 15 秒扫描一次 DLL，之后逐秒记进程私有内存/工作集、线程、句柄、CPU、GPU 专用/共享内存。全机同步记录提交量、可用内存、分页/非分页内核池、DPC/中断和物理磁盘平均读写延迟。双端 HTTP 不在线也不会停止资源采样，错误和请求耗时会保存。

权限受限或 32/64 位差异可能阻止模块发现；`discovery` 保存扫描成功/失败计数。请在与下游相同的 Windows 登录会话使用 64 位 Python；已知进程可追加 `--pid 1234`。不能读取的指标标错误/null。

## 开始变卡时

另开终端，填**本次实际目录**：

```powershell
python scripts/longrun_diagnostics.py --output run/diagnostics-日期时间 --mark "开始变卡，鼠标和画面延迟升高"
```

先标记并保留现场，再按实际情况分别停止采集、关闭下游，记录动作与恢复情况。例如停止采集后：

```powershell
python scripts/longrun_diagnostics.py --output run/diagnostics-日期时间 --mark "已停止采集，仍然卡顿"
python scripts/longrun_diagnostics.py --output run/diagnostics-日期时间 --mark "已关闭下游，恢复流畅"
```

不要为了记录器自动重启电脑。记录器自然结束，也可 Ctrl+C 或：

```powershell
python scripts/longrun_diagnostics.py --output run/diagnostics-日期时间 --stop
```

该命令仅结束记录器。所有样本逐行 flush；异常退出仍保留已写样本。全量样本按 3600 行分卷，不循环覆盖；事件附前后 60 个样本索引。停止/进程退出/重启前未采到的内容不会被虚构成完整上下文。

## 候选版细分计时

已用版本不会因脚本启动而换版。新编译的 Raw receiver 与 vcam 仅在它们所在进程启动前带有 `VC_DIAG=1` 时提供细分计时。vcam 在**下游进程**里，给 GUI/receiver 设置变量不会自动传给已经打开的独立下游。不要为了开计时修改下游业务逻辑。

计时单列 stats_prepare、stats_write、hash_write_flush、sample_getbuffer、downstream_receive；未结束调用也能看到 pending_ms。收包/交付线程不新增日志写盘，关闭诊断时不创建映射。既有 rseg/vseg 含义不变，因而历史 vseg 仍不包含 Receive 内部耗时。具体布局与限制见 skill `references/longrun-diagnostics.md`。

若下游遵循 DirectShow BeginFlush，Stop/Disconnect 会解除 Receive 等待；Decommit 会解除 GetBuffer 等待。拒绝响应 flush 的下游仍可能导致 Stop 等待；不会超时后释放仍被工作线程使用的对象。

## 隔离构建与回归

```powershell
cmake -S . -B run/longrun-candidate-build
cmake --build run/longrun-candidate-build --config Release --target vc-vcam vc-vcam-lifecycle vc-vcam-verify vc-sender vc-receiver
python scripts/longrun_verify.py --build run/longrun-candidate-build
python scripts/verify.py
```

生命周期工具默认不读真实 FrameMap，可通过进程环境 `VCAM_FRAME_MAP=Local\不重名的隔离名称` 强制隔离；专项父脚本已自动处理。所有 DLL 均直接加载，不运行 regsvr32，不访问驻留 agent，不替换 dist 或远端。

## 如何判读

- `samples-*.jsonl` 是事实；`marks.jsonl` 是人工反馈；`events.jsonl` 是启发式提醒，不能当成已确定根因或验收通过。
- 对齐“开始变卡”前后的同一 PID + created_100ns，比较 private/GPU/句柄/线程；进程退出后资源释放只能支持关联，不能单凭这一点判定泄漏来源。
- 内核池/DPC先恶化而进程平稳，支持继续查驱动；磁盘耗时与 stats/hash pending 同时恶化，支持查写盘阻塞；GetBuffer/Receive pending 先增大，支持查下游背压。
- stats 成帧数不变可能只是桌面静止；只有 stats 自身运行时间也不更新才标停更。远端 QPC 绝不与本机 QPC 相减。
- 修好“每次创建泄漏”不等于证实“持续逐帧泄漏”；未复现数小时卡顿前，只报告候选版和测试事实。
