# P6 采集/编解码 code review 修正与本机回归（2026-11-13）

## 范围与边界

修复近期 P6 review 的8项发现（1 P1、7 P2）：WGC尺寸/关闭与回调生命周期、PyroWave同步等待、OTA DLL管理、
低层fps/采集目标校验、无效目标码率约束、DShow冻结漏检。只修改开发源码及build产物。
未提交、未打包、未修改dist、未注册虚拟卡、未访问PC2或执行真实部署/OTA；所有OTA测试使用临时私有根。
以下都是本机功能回归，不是双机、长稳、画质或吞吐验收，不改变历史Raw判据或28B包头。

## 实现

- WGC以帧ContentSize驱动放大/缩小后的帧池重建，释放旧帧/纹理/SRV，输出会话不变。
  有效区域不足crop时明确失败；共享回调状态跨撤销存活，取帧与Recreate用锁串行。
  Closed、原始HWND/进程失效和回调异常均为致命错误，不重新找同标题窗口、不回退DDA。
- PyroWave创建、编码/取包、解码、重置和销毁由独立5秒看门狗覆盖；超时直接退出当前原生子进程（124），
  避免在挂起GPU调用上清理/展开栈/join。agent明确报告原因，正常无输入不计时。无自动设备恢复。
- 必需的libpyrowave-shared-1.dll纳入OTA白名单/槽位/prev/回滚。回滚恢复文件不存在状态；
  激活复制或删除失败恢复事务前状态，未改变的锁定exe不重写，恢复失败保留唯一事务快照。
  旧agent仍需先独立升级白名单，再stage包含DLL的新运行时包，本轮未部署。
- receiver独立验证/传递fps，编码会话无几何字段也显式下发实际默认几何/fps。
  非法采集索引/标题/模式在停止旧进程前拒绝；不静默切默认源。
- MJPEG/PyroWave不生效的目标码率不再约束最大码率，两项分别校验并保留原描述符值；Raw历史校验不变。
- DShow从集合成员校验提升为按解码出现序列关联、motion实际推进及尾部检查。
  D12启动/尾部重复、短停顿、静态桌面及A/B/A往返允许；不做跨机QPC比较或有损源像素相等声明。

## 本地总闸

- `python scripts/verify.py --live Loopback`：构建、双selftest、pattern_ref、pytest全部通过，299 passed / 4 skipped。
- 最终OTA事务细节修正后 `python scripts/verify.py --no-build --live Loopback`：ALL PASS，301 passed / 4 skipped。
  四项跳过：默认T3反例、未构建冻结agent的OTA、默认不运行的两个私有窗口live用例（后者已另行显式通过）。
- 最终故障恢复专项：4 passed；完整OTA专项先前9 passed / 1 skipped。
- `git diff --check`通过（仅LF/CRLF提醒）。
- 总闸日志：`build/review-fixes-verify-final.log`、`build/review-fixes-final-gate.log`。

## WGC 真实私有窗口（无网卡）

`VC_WGC_LIVE=1 python -m pytest ctrl/tests/test_wgc_regression.py -q -s`：3 passed。
日志 `build/review-fixes-wgc-final.log`。

| 路径 | 初始源 | 放大后 | 缩小后 | 输出 | 关闭 |
|---|---|---|---|---|---|
| CPU | 886×693 | 986×713 | 746×553 | 640×640不变 | E_CLOSED |
| GPU | 886×693 | 986×713 | 760×560 | 640×640不变 | E_CLOSED |

首次live测试确实发现Closed回调在本机未及时送达，关闭后仍timeout；补原HWND/进程检查后上述两路径通过。
早期测试编译还暴露Windows min/small宏冲突，测试目标补齐NOMINMAX并改局部变量名后通过。
这些失败日志保留，不删改为通过。私有窗口是纯色功能源，不据此作性能结论。

## PyroWave 阻塞故障注入

`ctrl/tests/test_operation_deadline.py`：9 passed。
使用真实Codec wrapper配stub C API，分别在创建、编码、compute_num_packets、同步解码、重置销毁、设备销毁
模拟30秒阻塞，每项均在5秒看门狗内以124退出；另测成功、异常、空闲与重新计时。
无真实GPU驱动挂死注入，因此不声称整机/驱动故障恢复。

## 真实编码/SHM/DShow

均为pattern=motion、adapter=Loopback、batch=1，真实本机sender/receiver和隔离DShow DLL加载，注册操作为替身。

| 场景 | 尺寸/fps | 发送/解码 | DShow推进 |
|---|---|---|---|
| MJPEG transport | 416² / 30 | 60/60 | 60不同帧、到达末帧 |
| PyroWave transport | 416² / 30 | 60/60 | 60不同帧、到达末帧 |
| PyroWave故障注入帧20缺片 | 416² / 30 | 60发送，59有效解码 | 帧21起恢复，59不同帧、到达末帧 |
| MJPEG隔离产品 | 418×282 / 30 | 121/121 | 91个不同样本、尾差0 |
| PyroWave隔离产品 | 418×282 / 30 | 117/117 | 92个不同样本、尾差0 |
| PyroWave transport | 640² / 240 | 480/480 | 480不同帧、尾差0，零重置/丢弃 |

240档显式最大码率400Mbps，实际发送约239.99fps；仅说明本轮本机回归通过。
真实两种后端 `codec-check` 目标50Mbps/最大20Mbps均configured，不送帧、不冒充实帧验收。

证据目录（全部gitignore原始数据，完整hashlog留存）：
- `run/p6-codec-transport-20261113-224107-mjpeg-f8256e/`
- `run/p6-codec-transport-20261113-224115-pyrowave-737803/`
- `run/p6-codec-transport-20261113-224123-pyrowave-c23b3e/`（显式丢失注入）
- `run/p6-product-20261113-224131-mjpeg-d1c6d1/`
- `run/p6-product-20261113-224141-pyrowave-4564e4/`
- `run/p6-codec-transport-20261113-224426-pyrowave-efc759/`

契约同步在skill references的agent-api、stream-config、encoded-stream、packaging，以及PLAN §1。
