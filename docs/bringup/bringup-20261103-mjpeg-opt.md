# MJPEG 缓冲与数据搬运优化（2026-11-03）

## 结论与边界

owner要求“开始优化”。本轮只优化MJPEG内存分配、复制和重复压缩哈希，不降低Q80、不修改DCT/采样/颜色变换、不扩队列、不放宽帧率/码率闸、不删除CRC/逐帧哈希/日志。
Raw/MJPEG后端与外部CLI/API/descriptor、28B包头/48B载荷/SHM/DShow契约不变。当前部署及已有成品未替换，未提交、未部署PC2、未重启常驻服务或关闭用户应用。

同机motion/640²/Loopback/batch1交叉对照：240上限下平均生成至首次DShow交付从1.67～1.69ms降至约1.64ms（两轮均值约低2.2%）；60上限从约1.80ms降至1.73～1.74ms（约低3.7%）。
这是**小幅平均改善，不是大幅提速**；P99区间重叠，进程总CPU没有一致下降，不宣称尾延迟或CPU占用已明显降低。
本轮不重新测DDA或双机实际桌面，不能把合成源结果用于PC2/240Hz桌面或光到光验收，P6-3总体仍IN PROGRESS。

## 实现

- `sender/encoded_sender.h`：MJPEG回调只借用TurboJPEG固定输出，直到本帧封装/发送/审计完成；下一次pump前结束借用。
- `common/access_unit.h`：`pack_into`复用目标容量，只重置48B头、复制实际载荷；返回本次真实计算的FNV-1a64。既有`pack`仍走相同序列化布局，所有codec对账/畸形输入规则不变。
- MJPEG复用封装与以太分片缓冲；sendqueue按实际需要增容、会话退出统一释放，每次flush仍检查完整字节数并清零长度，短发送不回退batch。
- sender日志复用本帧封装的压缩hash，去掉一次JPEG重复扫描；完整源NV12 hash、CRC、接收端压缩hash和解码hash仍逐帧计算，日志仍逐帧flush。
- `common/mjpeg_codec.h`、`receiver/decoder_backend.h`、`receiver/encoded_receiver.h`：增加精确容量的直接解码目标，写入FramePool已acquire且seq为奇数的槽，跳过解码后一次NV12整帧memcpy。
  全部解码与UV合并成功才publish；任何解码错误在reopen时abandon未发布槽。既有私有输出方法保留，不改Raw池/SHM布局。

未实现全链路零拷贝、GPU JPEG、异步审计或新增SIMD UV实现；不拿这些设想当本轮收益。

## 同条件交叉对照

固定：640×640 NV12，pattern/motion/crop，Loopback/batch1，MTU9000/pacing0，Q80/GOP1，双端`turbojpeg:cpu`/hardware0，decoder上限1，最大AU8MiB，最大码率显式800Mbps。
原产品80Mbps默认不变。当前载荷约141Mbps，不能把800Mbps上限写成实际带宽；带宽随内容变化。
准备预测仍为分型recent64 p95+0.1ms，单帧编码在途、网络等待队列最多2AU，帧率最小间隔ceil(1e9/fps)，不追赶。

“旧”是本轮优化前已交付的MJPEG开发构建，不是稳定Raw部署；“新”是最终本轮原生构建。vcam DLL和DShow验证工具全程相同。
每组7200帧约30秒，另各600帧验证60上限；这不是长稳测试。单位ms。

| 顺序 | 版本/上限 | 实发fps | 生成至首次交付avg | P99 | 最大 | 发送/接收/首次交付 |
|---|---|---:|---:|---:|---:|---|
| A1 | 旧/240 | 239.985 | 1.6694 | 1.9523 | 6.1564 | 7200/7200/7200 |
| B1 | 新/240 | 239.984 | 1.6436 | 2.1007 | 3.5472 | 7200/7200/7200 |
| B2 | 新/240 | 239.981 | 1.6422 | 1.9357 | 2.4933 | 7200/7200/7200 |
| A2 | 旧/240 | 239.973 | 1.6900 | 2.1677 | 3.9614 | 7200/7200/7200 |
| C1 | 旧/60 | 59.9994 | 1.8009 | 2.3233 | 2.5158 | 600/600/600 |
| D1 | 新/60 | 59.9992 | 1.7393 | 2.2870 | 2.4978 | 600/600/600 |
| D2 | 新/60 | 59.9993 | 1.7291 | 2.3239 | 2.8768 | 600/600/600 |
| C2 | 旧/60 | 59.9968 | 1.8003 | 2.4302 | 3.5461 | 600/600/600 |

31200帧全部发送/接收/首次交付，反馈环无溢出/未跳帧；对应frame_id的源hash、压缩hash/长度/keyframe、解码像素hash在新旧版本全部一致。
各段由完整未舍入QPC/反馈日志独立重算，包含启动和慢帧，不删异常值；停流重复样本不计首次交付或实发fps。
计时为同机生成开始至vcam调用下游Receive前，不包括显示器或应用后处理，不跨机相减QPC。

240档audit_hash平均由0.514～0.519ms变为0.459～0.462ms，约减少0.05～0.06ms/帧；该段位于发送后，**不能直接从当前帧首次交付延迟扣除**。
发送至接收发布平均约0.73→0.70ms；这是整段，不将其全部称为纯JPEG解码。
进程CPU用GetProcessTimes在子进程退出后读取，包含启动/等待/精旋/尾部；原始值在summary中，未观察到跨两轮一致的总CPU改善。删除少量工作后仍有帧率闸精旋，不能把wall-clock阶段缩短当作同等CPU节省。

## 正确性与专项

`python scripts/verify.py --live Loopback`实际退出0：**403 passed / 2 skipped，ALL PASS**，包含新增的两项无网卡原生故障注入。
专项脚本串行执行8项，全部退出0：

| 项目 | 验证结果 |
|---|---|
| MJPEG丢帧恢复 | 90@60/GOP1，丢20，89帧完整传输/解码，从21恢复，reset1/discard0 |
| Raw尺寸矩阵/产品贯通 | 七组尺寸/帧率原始像素全量对账；HTTP产品121/121收发，DShow92样本 |
| MJPEG六档矩阵 | 416²/640²/720p/1080p/1440p/418×282，各60帧@30，共360帧完整 |
| Q60/Q90 | 各60帧，源/压缩hash/长度与优化前相应质量结果逐帧一致，没有隐式改质量 |
| 最大80Mbps限流 | 640²/480帧@240上限，实际144.785fps，最高一秒载荷9999698B；与旧版800Mbps对照前480帧的源/压缩hash/长度完全一致，只等待不降质 |
| MJPEG产品贯通 | 418×282@30/fill/Q86，121/121收发、DShow91样本，实际stats确认86；注册替身不改注册表 |

Raw矩阵与产品、Q60与Q90各计两项，共8项。

### 缓冲故障注入（不需要真实Npcap）

`ctrl/tests/test_mjpeg_buffers.py`用MSVC编译实际sender/receiver源码，只替换Npcap函数指针，不替换编码、分片、重组、解码、工作线程或帧池逻辑：

- sender：交替平坦/噪声NV12造成JPEG大/小帧，验证batch1/69、sendqueue两次以上按需增容而非每帧分配、EOS复用，以及分配/入队/短发送/sendpacket失败清理；实际JPEG字节、CRC、源/压缩日志hash与独立参考完全一致，成功或失败均无队列泄漏。
- receiver：给合法CRC/整体hash封装放入截断熵流的JPEG，连续故障数量超过帧池槽数，再发送好JPEG。只发布前后两帧且解码像素完全一致，错误帧不发布，未发布槽能反复abandon，不因坏帧耗尽池。
- `--au-selftest`新增复用目标、污染头/保留字段清零、容量稳定和hash一致检查；`--mjpeg-selftest`覆盖直接目标与私有输出完全一致、前后保护区、容量/空指针拒绝。

## 构建与交付

| 文件 | 旧SHA-256 | 新SHA-256 |
|---|---|---|
| vc-sender.exe | `481c3ac93bfcc4c7091a843c8962c32b4e14497b2ae6beeb055c13717ffc33bd` | `826b6fab8fbc0419386c287003ceb5f52ef30f40f0b1546e2a218c1aba287982` |
| vc-receiver.exe | `05aa5583e5818f2530a801a44ea9ae47f30af303acc23d9feca3401fc892f9ba` | `db3bce1e6de9dfbc361283b565ae12d96dfd3144b3812c9e20f518f2a2382b43` |

共同DLL `d636edadef416b7dff35e6907980b0d0af89440bbf2b9e4f5b2ce2d76ad26f4f`，验证工具`dafde556d3247cffef92c6a1754d09e9e5995a80407487290b50f45e58a13e0d`。
原生产物与A/B candidate逐字节一致。保留先前dirty工作区与旧包，不把此前未提交功能算作本轮新增，不commit/OTA。

预定独立开发包`dist/VirtualCapture-P6-MJPEG-Opt-20261103`，使用现有一键GUI打包脚本；是否成功与最终逐项复核以证据根`delivery.json`为准：

```powershell
python scripts/package.py --allow-dirty --output dist/VirtualCapture-P6-MJPEG-Opt-20261103
```

依赖、许可证及GUI质量配置仍按`../runbooks/mjpeg.md`。成品不是自动上线；两端5143不切换，PC2并存/正式切换仍按D20与`../runbooks/p6-cutover.md`确认。

## 证据

根`run/mjpeg-opt-20261103-173622/`，指针`run/mjpeg-opt-current.txt`。二进制/运行证据不入git。

- `baseline/`、`source-before.json`、`baseline.diff`、`baseline-status.txt`：开工完整源码与Git状态，保留原有dirty改动。
- `old/`、`old-hashes.json`、`candidate/`：本轮旧/新实际被测程序。
- `measure.py`、`latency-summary.json`、`latency/`、`ab.log`：八组31200帧参数/原始hashlog/QPC反馈/GPU快照/进程CPU。
- `audit.py`、`independent-audit.json`、`audit.log`：独立关联/分位重算与新旧像素/压缩输出一致性。
- `regressions.py`、`native-regressions.json`、`regression-audit.json`：8项串行专项与限流/质量独立对账。
- `native-buffer-test.log`、`verify-live.log`：直接缓冲/故障测试及403/2总闸。
- `delivery.json`：独立包、源码/载荷/被测哈希、旧包及常驻agent保护核对，成功交付后写入。
