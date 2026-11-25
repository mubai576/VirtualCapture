# WGC 约60fps同源对照（2026-11-14）

## 范围

owner要求继续定位WGC+PyroWave约59fps。本轮只运行短时诊断，不改正式vc-config、显示模式、编码默认值或部署程序；不提交。使用原有0.5.1 agent与native。诊断工具与数据在`run/wgc-cadence/`，不入git。

## 对照方法

- PC2 192.168.2.59 capture（RTX5070Ti/Realtek PCIe 2.5GbE）→PC1 192.168.2.100 receive（RTX4070/I226-V）。两端0.5.1，开始前idle。
- 新建私有D3D11可见窗口，整数调用现有`vc_gen_motion`，800²绘制为灰度BGRA，覆盖被采集显示器中心的640²区域；240Hz平均提交目标（绝对deadline，落后时会追赶，不能当严格逐帧限速）、Present(0,0)，记录每次present前后同机QPC。不是已有testwin浮点噪声源；不是改生产pattern算法。
- 新工具通过现有脚本OTA槽位交付固定诊断载荷，并仅在ROOT/run/motion-cadence内展开运行。不替换agent/sender/receiver/DLL、不写注册表。辅助Start-Process已启动但HTTP调用因进程流句柄持续占用而超时，随后Status核对确切PID与路径，未盲目重复启动。
- 输出定位用同一D3D设备EnumOutputs(0)取DXGI_OUTPUT_DESC，再按DeviceName读DEVMODE：`\\.\DISPLAY1`，desktop=(0,0)-(2560,1440)，180Hz，AttachedToDesktop=1。诊断窗口居中放在此输出，无模式切换。
- 每组640²@240、crop、GPU转换、batch69、MTU9000、单播34:5a:60:4f:f2:33；WGC/DDA显示器0，不切窗口。PyroWave两端显式pyrowave:vulkan、80Mbps、在途1。每组独立源MAC/会话与私有SHM，不注册虚拟卡；先收后发，约12秒后先停发、排空再停收。
- 每组结束立即分页拉回完整远端sender hash/stats/stdout，同时保留主机receiver及压缩transport hash。分别按Raw hash或压缩transport_hash逐帧核对，不跨机比QPC、不用有损前后像素相等判编码正确。
- 下表fps按第21帧起sender同机send_qpc_ns跨度计算，不是GUI整数舍入，也不是跨机单程延迟；并非T1/T2长稳/240验收。

## 第一轮结果

| 路径 | 稳态发送fps | 发送/接收帧 | 相邻发送中位ms | hash对账 |
|---|---:|---:|---:|---|
| WGC Raw | 59.994 | 707/707 | 16.6571 | 全部一致 |
| WGC PyroWave | 60.014 | 706/706 | 16.6589 | 全部一致 |
| DDA Raw | 230.421 | 2739/2739 | 4.1988 | 全部一致 |
| DDA PyroWave | 235.066 | 2755/2755 | 4.1668 | 全部一致 |
| pattern motion + PyroWave | 239.656 | 2847/2847 | 4.1668 | 全部一致 |

五组send_errors=0、dropped=0，编码两组及pattern的transport_errors/decode_errors均0。WGC PyroWave总限流等待0.351ms，不支持80Mbps限流导致60fps；DDA/PyroWave存在发送帧率闸等待，不将rate_wait全部解释成码率触顶。

同一动态画面：去除编码后WGC仍60，而DDA/PyroWave明显超过60；pattern/PyroWave近240证明本次编码/传输并非固定60上限。范围缩小到WGC采集路径或其系统帧供给，不能仅凭此断言具体DWM/驱动根因。

五组结束双端均停止，保存config-before/config-after且JSON对象完全相同。没有将235或239.7冒充达到冻结240性能判据。

## 后续采集层探针

另建只调用实际WGCCap/DDACap的私有native探针，不经编码或Npcap，直接记录last_present_qpc（WGC SystemRelativeTime换算/ DDA LastPresentTime）、convert_qpc、源帧丢弃；读取DwmGetCompositionTimingInfo与MinUpdateInterval支持状态。

首次Monitor/Window均481帧/8s，同机源时间戳约60.025fps、中位16.6683ms，MinUpdateInterval getter均返回160000（100ns单位，即16ms）。DWM timing返回refresh/compose=10000000/41671（约239.975Hz）；这与同一输出DEVMODE报告的180Hz不是同一指标，不混作物理显示刷新率，也不据此归因虚拟显示驱动。

首次DDA stdout被/exec的128KB上限截断（truncated=true），不用于完整帧数结论。后续探针全部写远端独立文件，再用/files分页全量拉回，解决截断。

## MinUpdateInterval私有A/B

两轮均保持同一源窗口、640²/GPU转换/crop，单组8秒，不经网络/编码。初始化后、消费帧前显式设置该属性；不是正式sender修复，也没有部署替换。表中计算去掉前20帧，以同机SystemRelativeTime跨度为分母；“不同时间戳率”仅统计时间戳，不等于逐像素不同或独立画面验证。

| 采集 | 属性值（100ns） | 完整记录帧数 | 帧记录率（/s） | 不同时间戳率（/s） | 相邻时间戳中位ms |
|---|---:|---:|---:|---:|---:|
| 显示器，系统默认 | 160000 | 481 | 60.027 | 60.027 | 16.6683 |
| 显示器，无最小间隔 | 0 | 3257 | 407.002 | 207.023 | 4.1667 |
| 窗口，无最小间隔 | 0 | 1908 | 238.460 | 112.722 | 0 |
| 显示器，240fps间隔 | 41667 | 1650 | 205.964 | 203.435 | 4.1672 |
| 窗口，240fps间隔 | 41667 | 949 | 118.519 | 118.264 | 8.3341 |

- setter均成功并读回请求值。这个系统支持IGraphicsCaptureSession5，不能把结果泛化为所有Windows版本。
- 零间隔显示器稳态3237条中有1590个相邻同时间戳；窗口1888条中995个。不能用407回调/帧记录率宣称407独立画面。按240间隔时分别还有20/2个相邻同时间戳，也没有验证逐像素唯一。
- 无编码、无网络且只改变MinUpdateInterval即可解除约60限制，直接支持当前60瓶颈来自未配置的WGC会话默认最小更新间隔。`sender/wgc.h`在CreateCaptureSession后仅设置光标/边框并StartCapture，没有设置该属性。
- 配置240间隔并不保证240实际画面；窗口仍约119条/s。确切剩余限制、源内容唯一性与真实客户端效果本轮未解决。显示器/窗口不能互相代替验收。
- 诊断源使用绝对deadline，有追赶提交；平均约240不代表严格每4.167ms一帧。探针DDA也可能观察到光标等桌面更新，不能将其帧数等同源窗口独立内容帧数。第一轮sender有严格发送上限，仍仅作60瓶颈对照，不提升为240性能验收。

完整证据：`run/wgc-cadence/intervals/`、`interval240/`，含远端完整日志、源present日志、退出状态、配置复核；汇总`interval-analysis.json`。首次时间戳证据亦保留。最后两轮均确认owned诊断PID退出、两端/health.running=false、/config前后相同；未修改显示模式、未注册虚拟卡。

## 结论与源码边界

约59/60fps不是PyroWave固定上限，也不是本次链路吞吐造成；已定位到此PC2上WGC默认16ms最小更新间隔。下一步源码修复应在StartCapture前按会话fps显式设置该属性，并明确处理旧系统无接口与setter失败，不吞错声称达标；需要对修复后的真实sender做双机复核。本文仅完成诊断与文档记录，**未修改生产采集实现、未重新打包/部署/提交**。本轮没有生产代码变更，未重跑完整build/pytest总闸。

## D27：固定1ms源码修正（2026-11-14）

owner先指定3.3ms，随即改为“延迟1”；最终仅实现固定1ms，不保留3.3ms。`WGCCap::init`在StartCapture前查询IGraphicsCaptureSession5，设置10000×100ns并getter读回确认。无接口以不可用能力拒绝启动；setter异常或读回不符为初始化错误，不静默沿用默认间隔、不回退DDA。发送fps上限及协议/编解码不变，不把1ms称为端到端延迟。

纯回归增加常量编译期断言，私有窗口live测试增加真实getter校验。第一次总闸构建成功，但PC1无IGraphicsCaptureSession5，因初版误将无接口标为内部错误导致sender selftest失败；随后改为E_NODDA（不可用能力，实际启动仍明确失败），自检可按原约定SKIP。第一次日志`build/verify-wgc-1ms-20261114.log`保留；最终`VC_WGC_LIVE=1 python scripts/verify.py`通过：build、双selftest、pattern_ref、350 passed / 6 skipped（177.73s），日志`build/verify-wgc-1ms-20261114-final.log`。PC1无接口，WGC selftest及私有窗口CPU/GPU测试按不可用跳过，不能据此称本机WGC live通过；支持接口的PC2实采验证另列。不替换正式sender/agent。

PC2私有探针直接编译修改后的真实`sender/wgc.h`，不在探针中另设interval：Monitor/Window初始化后getter均为10000，证明1ms由生产初始化路径设置。每组8秒，完整文件分别2521/1419条；去前20帧按源时间戳跨度计算记录率315.59/176.94条每秒，不同时间戳率200.84/111.00，相邻重复时间戳909/521个。后者不是逐像素不同帧判据，不将315.59冒充独立画面fps或发送吞吐。证据`run/wgc-cadence/one-ms/`包含完整日志/analysis/源present/退出状态。诊断PID已退出，两端空闲，正式配置前后相同；未进行修改后真实sender的双机吞吐或虚拟卡验收。

API参考：[Microsoft Learn MinUpdateInterval](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.minupdateinterval?view=winrt-26100)；页面没有说明默认值，本报告16ms来自本机实际getter和A/B，不冒充文档默认契约。
