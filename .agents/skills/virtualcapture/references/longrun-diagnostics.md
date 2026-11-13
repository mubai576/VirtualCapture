# PC1 Raw 长运行诊断（2026-11-08，增量契约）

不改 28B wire、Raw 字节、SHM v2/v3 帧布局、既有 stats 字段含义或验收门限。
不自动部署、注册、停止采集、关闭下游或更改网卡。完整操作见 `docs/runbooks/longrun-diagnostics.md`。

## 独立记录器

`python scripts/longrun_diagnostics.py [--hours 6 | --seconds N] [--output DIR]`

- 默认每秒记录，6 小时自然结束；`--interval .2..60`、`--discovery-interval 15` 可调。
- `--sender http://192.168.2.59:5143`、`--receiver http://127.0.0.1:5143`：只 GET `/sender/stats`、`/receiver/stats`，并行、超时、有界响应、不走代理。要求数字 IPv4，无 DNS/凭据。
- `--pid N` 可重复追加观察对象；默认观察原生发收端、冻结 agent、自身及自动发现加载 `vc-vcam.dll` 的进程。模块发现每 15 秒，记录权限/位数限制，不能把无法读取当成没有下游。
- `--output DIR --mark "开始变卡"` 追加 `marks.jsonl`；也可标记“停止采集”“关闭下游”。只记录用户已经采取的动作。
- `--output DIR --stop` 写停止请求，只让采样器退出，不控制任何业务进程。
- `session.json`、每 3600 样本一个 `samples-NNNN.jsonl`、`events.jsonl`、`marks.jsonl`、`summary.json`；不删除样本，完整保留异常前后。事件 `context_samples` 为前后各 60 个样本索引，间隔非 1 秒时不能解释成 60 秒。
- UTC 用于人工对齐，monotonic 用于本机顺序。远端 QPC 只原样保存，不与 PC1 QPC 相减。
- PDH 用英文 counter 名称；缺少 GPU/磁盘 provider 或权限时明确记 error/null，不伪造为 0。
- 内存/GPU 增长、句柄/线程增长、内核池、DPC/磁盘及 stats 停更阈值仅为诊断标记，不是 test-matrix 门限或根因判定；静止 DDA 的帧数不变不判故障。

## 可选原生阻塞计时

新 receiver Raw 分支、候选 vcam 所在进程启动前设置 `VC_DIAG=1`；未设置/不等于 1 时不读 QPC、不创建诊断映射。不修改驻留进程环境，旧版继续可采资源，但没有这些计时。

- 名称：`Local\VirtualCapture.Diag.<PID>`，同 Windows 登录会话，按进程默认 ACL。文件映射只在目标进程对象/线程持有时存活；不创建后台日志线程。
- 总大小 4160 B；小端，64B header + 32 个 128B lane。声明唯一真相源 `common/diagnostic_timing.h`。
- Header：`uint32 magic=0x56434447, version=1, bytes=4160, lanes=32, pid, reserved; uint64 frequency; padding[32]`。
- Lane：`int32 sequence,lease; uint32 tid,kind; uint64 generation,begin_ticks,count,total_ticks,max_ticks,last_ticks,slow_count,failures; padding[48]`。
- lease 0=历史/空闲，1=使用中，2=初始化；读取前后 sequence 相等且偶数才使用。generation 随重用改变，不能跨 generation 计算增量。
- kind：1=stats_write（临时文件、关闭、原子替换，保留 WRITE_THROUGH），2=hash_write_flush（每帧 append，含原有每 8 帧 flush 及收尾 flush），3=sample_getbuffer，4=downstream_receive，5=stats_prepare（排序、统计、JSON 生成）。Hash 校验计算不包含在 kind2 中。
- begin_ticks 非零表示调用尚未完成；采样器用**同机** QPC 算 pending_ms，因此阻塞不返回时仍可留证。count/total/max/last 按完成调用累计；slow_count 为 >20ms 的诊断计数，failures 保存写失败/HRESULT 失败。
- 固定 32 lane，无空间时跳过计时，不等待、不扩容；每线程独占 lane，seqlock 元数据更新不覆盖实际阻塞时段。
- 既有 vseg 仍是发布至调用 Receive 之前的时刻，新的 downstream_receive 单列，不回填或重定义历史 vseg。

## 验证工具

- `vc-vcam-lifecycle.exe <DLL>`：直接加载 DLL，1000 次释放、外部 pin/枚举器/工厂寿命、50 次真实连接 graph 拆除；不注册。
- `vc-vcam-verify --dll ... --shm ...` 追加 `--hold-samples`、`--block-receive`、`--disconnect-running`；这些仅为隔离测试开关。Stop/Disconnect 超过 2 秒判专项失败，进程总超时由父脚本兜底。
- `python scripts/longrun_verify.py --build <隔离构建目录> --output <新证据目录>`：motion/640x640@240/Raw/batch1/Loopback，保留完整逐帧 hash、重复样本及资源/阻塞证据。不冒充双机或六小时验收。
