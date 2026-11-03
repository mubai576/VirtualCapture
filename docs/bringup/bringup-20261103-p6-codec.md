# P6-3 H.264/H.265 全链路开发验证（2026-11-03）

## 结论与边界

本轮已打通真实 Windows MF 编码 → 变长 L2 分片 → 有界重组/解码 → NV12 SHM v3 → 真实 DShow 取帧，
并接入 agent 会话、双端后端检查、GUI 显式选择和诊断指标。不是只增加下拉框或候选枚举。
Raw 独立路径和历史判据保留，`proto/vc_wire.h` 无改动；没有新增依赖 DLL、注册虚拟卡、部署 PC2 或替换稳定目录。

**P6-3 仍为 IN PROGRESS**：部署 PC2 的实际编码后端、物理双机链路、编码 DDA 实景、长稳、画质/色彩、
60/240fps 吞吐与实际延迟尚未验收。以下均为本机功能验证，不能宣传为双机或正式上线通过。
GUI 默认为 Raw，编码默认为只保存草案；明确启用开发模式并选择两端 MFT 身份/类别后才允许启动。
协商不支持或运行失败时明确报错，不换 codec、设备、尺寸、帧率、batch 或解码队列深度。

## 实现

- `common/access_unit.h`：48B VCAU 载荷内封装、压缩字节 FNV、Annex-B IDR/参数集验证、EOS、有界重组及负例自测；外层包头仍28B。
- `common/codec_session.h` / `common/mf_codec.h`：显式 CLSID 或 `name:精确名称`、硬件类别、低延迟/CBR/GOP/强制IDR、实际媒体类型协商；失败不回退。
- `sender/encoded_sender.h`：每次最多1帧编码在途，动态分片，真实发送帧率上限、无追赶补发、一秒最大载荷滑窗与 AU 上限；每帧分别记录源像素与压缩字节哈希。
- `receiver/encoded_receiver.h`：Npcap 与独立解码线程分离，最多2个待解码 AU，显式解码在途上限；断档/溢出/错误后重置并等完整IDR；精确aperture/stride复制、EOS/drain、真实SHM/DShow输出。
- `agent/vc-agent.py`：候选枚举、只读 `/stream/codec-check`、双端会话严格回显、GUI 后端/类别/码率/IDR/在途上限；Raw与有损传输完整性统计分开。

完整 CLI、载荷布局、API、队列和日志契约见 `.agents/skills/virtualcapture/references/encoded-stream.md`。

## 环境与后端

本机 Windows、MSVC 19.44、Npcap Loopback；所有正向链路用 `motion / batch1 / Loopback / MTU9000`。
编码器由本机 RTX4070 的 NVIDIA MFT 实际运行，不能代替 PC2 后端测试。

| codec | 编码器，硬件类别 | 解码器，系统/软件枚举类别 |
|---|---|---|
| H.264 | `{60F44560-5A20-4857-BFEF-D29773CB8040}` | `{62CE7E72-4C71-4D20-B15D-452831A87D9D}` |
| HEVC | `{966F107C-8EA2-425D-B822-E4A71BEF01D7}` | `name:HEVCVideoExtension` |

HEVCVideoExtension 是本机已安装组件，不假定其他机器存在，也不自动安装。
“系统/软件枚举类别”不等于已证明没有内部硬件加速；本轮不作此宣称。

## 最终原生矩阵

两种 codec 各测 `416×416 / 640×640 / 1280×720 / 1920×1080 / 2560×1440 / 418×282`，
每组60帧、发送上限30fps、请求20Mbps、最大80Mbps、GOP15、解码在途上限2。
这是发送上限验证，不是每档实时30fps吞吐承诺。所有样本均检查发送间隔不小于33,333,334ns和一秒载荷预算。

| 项目 | 结果 | 完整证据 |
|---|---|---|
| H.264 六档 | 360/360压缩哈希一致、360/360解码；每档DShow观察60种真实解码哈希 | `run/p6-codec-transport-20261103-061526-h264-ebe074/matrix.json` |
| HEVC 六档 | 360/360压缩哈希一致、360/360解码；每档DShow观察60种真实解码哈希 | `run/p6-codec-transport-20261103-061526-hevc-142b9f/matrix.json` |
| H.264 产品编排 | 418×282/fill，112/112传输解码一致，88个DShow样本 | `run/p6-product-20261103-061622-h264-e3441a/result.json` |
| HEVC 产品编排 | 418×282/fill，111/111传输解码一致，89个DShow样本 | `run/p6-product-20261103-061624-hevc-13db41/result.json` |
| H.264 1Mbps滑窗 | 60/60一致，峰值124812B/秒（含封装，低于125000B）；实际19.298fps，不伪称30fps | `run/p6-codec-transport-20261103-061905-h264-e785eb/matrix.json` |

矩阵为原生实帧；产品测试通过私有ROOT、随机HTTP端口、私有映射执行真实agent/native/DShow，
仅注册/注销函数被替换。因此不是注册表/下游应用真实设备枚举验收，不占用5143，也不更改运行目录配置。
DShow 的 sample 总数含末帧停顿重复；另报 distinct hashes，不以重复样本制造新画面 FPS。
有损前后的 source_hash 与 decoded_hash 不要求相等；检查的是压缩字节完整性及解码输出到DShow一致。

## 丢包恢复与明确拒绝

用 `--test-drop-frame 20` 只丢第20帧seq0；GOP15，下一个完整IDR为frame30。
两种codec均验证30..59全部恢复，20..29不发布依赖坏画面；不用Raw零丢帧判据伪装成无损通过。

| 项目 | 真实结果 | 证据 |
|---|---|---|
| H.264 缺片 | 60输入，59压缩完整，50解码，1次reset，9帧discard | `run/p6-codec-transport-20261103-061614-h264-bacb4e/matrix.json` |
| HEVC 缺片 | 60输入，59压缩完整，49解码，1次reset，10帧discard；另丢解码器缓存旧帧 | `run/p6-codec-transport-20261103-061616-hevc-431b8c/matrix.json` |
| HEVC 在途上限1 | 5秒明确失败：`explicit decode-in-flight limit`；不自动改成2 | `run/p6-codec-final-20261103/negative-49a6fa/result.json` |
| AU上限1024B | 明确失败：`explicit max-au-bytes` | 同上 |
| H.264 2×2 | 后端媒体类型拒绝，退出1、HRESULT `0xc00d6d76`；不缩放 | 同上 |
| Loopback batch69 | 短发送明确失败；不自动改成batch1 | 同上 |

HEVC 本机解码器常保留1帧等待后续输入，静态源最后一帧可能延迟到下个输入或EOS/drain。
GUI 显示pending，不能把它包装成与Raw相同延迟。周期IDR按输入帧数计，源静止时没有恢复墙钟上限。

### 本机批量发送失败保留

初始 `batch69 / 1Mbps` 诊断：发送60帧，接收仅54个完整AU/30个解码，不能记为PASS。
缺少的6帧恰为多片AU；证据在 `run/p6-codec-transport-20261103-060654-h264-392b1e/`。
随后确认 Npcap Loopback `pcap_sendqueue_transmit` 返回0，队列len为51337；
编码分支现严格比较返回字节数与队列len，错误退出而不再报成功，复现证据在 `run/p6-codec-transport-20261103-061048-h264-024fed/`。
Raw 历史发送实现和产品 batch69 默认值不改；本机正向专项明确指定batch1，不作为物理网卡batch69结论。

## 本地总闸与交付

`python scripts/verify.py`：构建、发收端自测、pattern_ref、pytest **ALL PASS，186 passed / 4 skipped**，
日志 `run/p6-codec-final-20261103/verify.log`。跳过项不是双机通过。
另跑真实Raw回环七组1665帧，逐帧收发哈希零错、发送节奏符合上限，证据 `run/p6-raw-20261103-061808/matrix.json`；
Raw产品编排121/121一致，证据 `run/p6-product-20261103-061913-raw-1e2b35/result.json`。

最终链路所用二进制SHA-256（打包来源可再核对清单）：

| 文件 | SHA-256 |
|---|---|
| vc-sender.exe | `cdd499c9082428edbe80713d395a2bc637d965686061eee620d33bc9bbf80f4c` |
| vc-receiver.exe | `f714af4247e9a5f1dcdfa0c179ea9b500d8e08a1962497e24d4c772132ab65da` |
| vc-vcam.dll | `d636edadef416b7dff35e6907980b0d0af89440bbf2b9e4f5b2ce2d76ad26f4f` |

GUI 脚本已通过Node状态/表单测试，覆盖显式授权/后端、草案保存、隐藏字段、错误状态及旧预算覆盖保护。
本轮桌面UI自动化辅助连接三次均报 native pipe unavailable，未绕过安全策略或冒充视觉检查通过；
实际桌面/手机宽度布局视觉复测及编码DDA实景留待后续。成品HTTP/GUI由打包脚本执行隔离冒烟。

便携交付使用原有脚本，独立目录 `dist/VirtualCapture-P6-Codec-20261103`，不覆盖稳定包或P6-2包：

```powershell
python scripts/package.py --allow-dirty --output dist/VirtualCapture-P6-Codec-20261103
```

`--allow-dirty` 仅包含本轮开发改动；来源/载荷哈希以该目录的 `package-manifest.json` 为准。
不因打包而视为PC2部署、注册或双机验收完成。当前版本元数据仍为0.4.1，不能仅凭版本字符串判断P6能力。

## 复现入口

```powershell
python scripts/p6_codec_verify.py --codec h264 `
  --encoder '{60F44560-5A20-4857-BFEF-D29773CB8040}' `
  --decoder '{62CE7E72-4C71-4D20-B15D-452831A87D9D}' --matrix
python scripts/p6_codec_verify.py --codec hevc `
  --encoder '{966F107C-8EA2-425D-B822-E4A71BEF01D7}' `
  --decoder 'name:HEVCVideoExtension' --matrix
```

上述后端身份只对应本机验证环境，不是所有机器的默认值。`--loss`为明确注入负例；
`--bitrate 1 --max-bitrate 1`可验证硬滑窗限码率；`--dda`必须有持续变化桌面，不用静态重复帧证明新鲜度。
下一步为 owner 安排双机切换后核验PC2 encoder/PC1 decoder，先做小规模真实桌面，再做画质/色彩、延迟/吞吐和长稳。
