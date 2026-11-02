# P6-2 可变尺寸 Raw 与真正帧率上限（2026-11-03）

## 结论与保护边界

P6-2 开发实现完成：控制面显式会话、原生可变尺寸 Raw、四种整数变换、发送上限、动态 SHM v3、
Python 读端、预览、DShow media type/allocator 和验证工具均已接通。本机垂直验证通过；**不是双机新模式验收或发版**。

- 先提交既有工作：`d15e97f`（P5-U1/U3 + P6-1）。本报告不把该提交当作后续 P6-2 来源。
- `proto/vc_wire.h` 28B 与保留 flags 不改；Raw 不压缩，旧640/crop整数结果保留。
- 稳定 ROOT、5143 服务、网卡、虚拟卡注册与 OTA 都不替换。构建只写开发 `build/`。
- 默认仍640×640@240；新组合不自动切尺寸、帧率、变换、codec或GPU/CPU路径。

## 实现要点

1. `common/stream.h`：尺寸校验、整数 crop/fit/fill/stretch、BT.601 CPU 参考、严格限速和CLI数字校验。
   GPU 使用同构整数采样，支持uint尾部及超过65535组的二维dispatch；旋转DDA输出明确拒绝。
2. `--fps` 是发送开始的硬上限，间隔至少 `ceil(1e9/fps)` ns；采集前靠近发送槽等待，转换后有最终发送闸。
   不补发追赶；DDA timeout、鼠标更新和重复 LastPresentTime 不发帧/递增ID，不重播旧缓冲制造FPS。
   `send_qpc_ns`单独记录真实开始分包时刻；旧`qpc_ns`保留采集/生成开始含义。
   DDA无新图像仍刷新stats心跳/uptime，不停留在上次好看的fps，也不制造帧数。
3. sender/receiver 动态帧长度和分包数。receiver精确校验total/seq/每包长度/pattern/CRC与完整覆盖，迟到帧不重发。
4. `stream_revision=2`，检查实际exe/DLL能力；descriptor逐键确认尺寸/fps/MTU/NV12/raw/transform/随机源MAC。
   MAC只是流隔离，不是认证，也不改系统NIC配置。旧后台、旧原生或错误回显在启动前拒绝或回滚双端。
5. SHM v3（VCA3）含不可变width/height/fps，12槽动态步长；新读端兼容固定v2。
   writer拒绝已有同名mapping，防止旧下游持有时改写几何；改尺寸必须关闭下游、重新开始采集，再打开下游。
6. DShow只公布实例创建时的一档真实格式；D12无首帧不输出、有历史才重复好帧。
   修复media type释放后使用、pin/filter及graph/filter引用循环；Stop以BeginFlush/Decommit解阻塞后等待线程退出，
   不在存活worker下释放缓存/事件。不能保证不遵守BeginFlush契约的第三方下游永不阻塞。

完整CLI/API/IPC定义：`.agents/skills/virtualcapture/references/stream-config.md`。

## 验证

### 本地总闸

`python scripts/verify.py`：build、sender/receiver selftest、Python参考/原生向量对账、pytest全部通过。
第一轮 **141 passed / 4 skipped**（87.41s），日志 `run/p6-raw-20261103-final/verify.log`。
含编解码工具与生命周期收尾的总闸 **153 passed / 4 skipped**（88.32s），日志 `verify-final.log`。
最后补充DDA idle状态心跳后的最终总闸仍为 **153 passed / 4 skipped**（88.33s），同目录 `verify-complete.log`。
默认跳过的live/T3不代表双机新参数验收。

- 旧测试向量和旧receiver selftest计数保留。
- 40组可变尺寸/五图案向量与独立Python全帧参考一致。
- 16组CPU/GPU逐字节转换用例，含416、720p、1080p、1440p、非4整除宽度、2×2、2×8192、4096²二维dispatch。
- 六组动态重组：倒序包、错误total/尾长与迟到包拒绝。
- HTTP/GUI会话、旧原生拒绝、参数精确转发、错误回显双端回滚、打包排除开发脚本均覆盖。

### motion Raw 全链路矩阵

命令：`python scripts/p6_raw_verify.py`。**同机 Loopback、pattern=motion、batch=1**，每组真实连接DShow样本sink，
没有regsvr32，也不访问稳定5143。完整sender/receiver hashlog逐帧对账，所有DShow样本哈希属于对应sender集合。

最终graph生命周期收尾后的完整矩阵：`run/p6-raw-20261103-011440/matrix.json`。
前两轮 `run/p6-raw-20261103-004502/`、`run/p6-raw-20261103-005655/` 均保留，未覆盖PASS证据。

| 输出/上限 | sent=received | 发送实测fps | 最小发送间隔ns |
|---|---:|---:|---:|
| 416×416 /60 | 180 | 59.9783 | 16666700 |
| 640×640 /240 | 720 | 239.5451 | 4166700 |
| 418×282 /120 | 360 | 119.9056 | 8333400 |
| 1280×720 /60 | 180 | 59.9315 | 16666700 |
| 1920×1080 /30 | 90 | 29.9620 | 33333400 |
| 2560×1440 /15 | 45 | 14.9909 | 66666700 |
| 2×2 /30 | 90 | 29.9994 | 33333400 |

合计1665帧，dropped/hash_mismatch均0；所有最小间隔满足上限，退出检查DllCanUnloadNow通过。
虚拟卡样本包含发送结束后的D12重复，不把它们算成新采集FPS；640这轮观察到719个不同hash，
这是latest-frame消费边界，不宣称每个receiver帧都交给下游。上表是本机数据，不是2.5G物理链路吞吐。

### 真实DDA + GPU + fit

只启动并清理本轮自己创建的`vc-testwin.exe`窗口。命令 `python scripts/p6_raw_verify.py --dda`，
**真实2560×1440桌面、Loopback、batch1、GPU转换、fit**；没有修改显示设置。
最终复跑证据 `run/p6-raw-20261103-012739/matrix.json`（前轮`run/p6-raw-20261103-004820/`保留）：

| 输出/上限 | sent=received | 发送实测fps | 最小发送间隔ns | DShow不同hash |
|---|---:|---:|---:|---:|
| 416×416 /10 | 30 | 9.9946 | 100000000 | 30 |
| 418×282 /30 | 90 | 29.9537 | 33333400 | 90 |
| 1280×720 /60 | 180 | 59.8287 | 16666700 | 180 |

300帧完整对账零错。这不是2K240桌面源的240fps专项，也不新增双机延迟结论。
动画窗口精确PID清理见 `run/p6-raw-20261103-final/testwin-complete-cleanup.json`。

### 产品控制面到真实原生管线

`python scripts/p6_product_verify.py`：两个独立模块/ROOT、随机HTTP端口、私有mapping。
主机实际调用capture HTTP，读取真实原生能力，协商descriptor、启动真实收发进程并接真实DShow graph。
仅注册/回收函数为可观察替身，端口重定向用于避开5143；不宣称真实注册表或稳定GUI已经切换。

最新证据 `run/p6-product-20261103-011047/result.json`：418×282@30、motion、fill、batch1、Loopback，
121/121完整哈希一致、91个DShow样本均来自sender集合，退出后DllCanUnloadNow通过。

负例 `run/p6-raw-20261103-final/negative-result.json`：同名mapping拒绝、不匹配源MAC过滤、从未接到有效帧时0个DShow样本。
`flush-result.json`另实测下游Receive阻塞至BeginFlush：Stop成功解阻塞，释放graph后DLL可卸载，没有用超时强行释放worker资源。

## 稳定版与未完成事项

只读核对 `run/p6-raw-20261103-final/stable-complete.json`：稳定成品87/87载荷大小/SHA256匹配；
两端仍0.4.1/product_revision2，host PID28040、capture PID12828，均未重启/替换。
远端实际二进制独立备份缺口仍按P5-U4，不用本轮源码补作证明。

- P6-4：双机新尺寸/真实DDA效果、带宽/长稳、真实下游应用重连与完整hashlog归档尚未验收。
- 2..8192是软件参数范围，不是每个组合的显存、吞吐或GPU认证。
- H.264/H.265产品启动仍拒绝。实际MFT编解码原型与后续接线边界见 `docs/proposals/codec-backend-20261103.md`。
- 没有创建便携包、发布zip、注册新DLL或执行OTA；稳定部署继续可用。
