# P6-R2 编码延迟、类别与实际设备上报（2026-11-13）

## 范围

owner要求修复编码路径发送/重组p99空白、PyroWave误标CPU，以及实际Vulkan设备身份不可见。
只改开发源码/build，未提交、未打包、未部署或更新dist，不启停线上5143服务/会话，不访问远端exec/OTA。
未改变设备选择、编解码算法、CPU上传回读路径、28B协议头、SHM或Raw判据。

## 实现

- 编码sender补pc1_seg：源帧准备完成→末包Npcap调用返回，包含编码/封装/限速/发送，
  不含采集等待和后置审计hash/log；与Raw分段边界一致，不能用transmit调用或capture_to_send冒充。
- 编码receiver补rseg：首片→AU完整，仅通过封装/hash校验的非EOS帧，不含解码/队列。
- 共用有界最新10000帧窗口、同机QPC、samples/scope；零样本null，真实零值保留。
- GUI读取相同指标，明确10000帧统计/60秒趋势；不对缺指标的旧原生数据伪造0。
- PyroWave类别只读显示Vulkan计算，MJPEG显示CPU/SIMD；解释hardware=false不是CPU开关，
  明确CPU上传/回读/UV交错、非全GPU零拷贝，不把软件Vulkan CPU设备标为物理GPU。
- PyroWave从已选设备取得VkPhysicalDevice，查询名称/type/vendor/device/API/driver/UUID/LUID；
  保持默认选择不变，查询不可用明确unavailable，不另建探测设备。
- sender.encoder_configuration和receiver.decoder_configuration上报同构设备信息，后者用互斥保护快照。
  UI设备卡片显示实际设备，不使用候选列表代替。驱动版本保留原始整数，不按通用格式误解厂商编码。

## 本地验证

- 原生构建通过。
- GUI/设备JSON转义/缺设备/CPU设备分类/零样本/窗口淘汰/watchdog专项：31 passed，日志
  `build/pyrowave-metrics-regression.log`。
- 早期stub编译遇Windows min宏，头文件显式清理后通过；转义测试期望写错导致首轮总闸
  1 failed / 302 passed / 6 skipped，修正测试期望后重跑，失败日志保留。
- 隔离GUI在5145、私有ROOT、无peer/native启动；真实浏览器确认PyroWave类别与路径说明，
  本机回环证据作为临时UI调试fixture确认发送3.29ms/重组0.50ms及设备卡片，无横向溢出。
  fixture不是双机实测。私有ROOT缺native导致/adapters两次503，非JS异常、不冒充完整产品链路检查。

### 真实编解码/SHM/隔离DShow

所有组：motion、416×416@30、Loopback、batch1、60帧；私有source MAC和SHM，不注册系统DShow。

| 场景 | 发送/解码 | 发送段p99 ms | 重组段p99 ms | 设备 |
|---|---|---|---|---|
| PyroWave | 60/60 | 3.2931 | 0.4957 | 编解码均RTX 4070/discrete_gpu |
| MJPEG | 60/60 | 0.6044 | 0.4652 | turbojpeg CPU/SIMD |
| PyroWave显式丢帧20首片 | 60/59 | 2.4370 | 0.1209 | RTX 4070；reset1，discard0 |

压缩hash对账、DShow有序推进/到达末帧、计时samples=完整帧数、设备schema均通过。
同机编码/解码设备UUID一致，`6c5df7eedc17f0ca4e60d8d32577ba39`，vendor4318/device10118；
不是PC2设备或双机吞吐结论。有损不做源像素相等声明。

证据：
- `run/p6-codec-transport-20261113-232723-pyrowave-0b0f65/`
- `run/p6-codec-transport-20261113-232731-mjpeg-2650e1/`
- `run/p6-codec-transport-20261113-232739-pyrowave-2d1d7f/`
- `build/pyrowave-metrics-live.log`、`build/mjpeg-metrics-live.log`、`build/pyrowave-metrics-loss.log`。

最终 `python scripts/verify.py`：构建、双selftest、pattern_ref与pytest全部通过，303 passed / 6 skipped，
日志`build/pyrowave-metrics-verify-final.log`。跳过为默认非live用例、旧dist/vc-agent.exe路径冻结OTA用例和可选WGC live；
本轮真实编码/DShow与GUI已独立验证，未重跑WGC live或冻结OTA。
本轮修复未替换已部署0.5.0，线上要看到新字段须另行打包更新两端。
