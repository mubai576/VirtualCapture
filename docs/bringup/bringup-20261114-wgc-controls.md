# 停止入口、固定后端与 WGC 约59fps排查（2026-11-14）

## 停止与证据保全

owner反馈停止采集Unexpected token '<'。浏览器网络实证：localhost:18144/product/stop三次403，返回只读验证wrapper的HTML；不是正式agent停止失败。18144为本会话留下的源码只读预览，未明确隔离控制按钮造成误导，已关闭。通过正式127.0.0.1:5143/product/stop返回ok=true，capture/receive两步均200；随后running均false，vcam.managed=false，注册已回收。未杀任意其他进程。

停止后立刻分页拉回远端s-1e258c87.sender完整hashlog/stats/stdout；hashlog接口size与保存UTF8均4,657,619 B，共17,014帧。注意/files本质是文本接口，不以其证明任意二进制字节保真。主机s-bd353ccc.receiver的hash/transport hash/stats/stdout均复制保留。证据位于`run/stats-debug/stopped-evidence/`，停止API结果`run/stats-debug/stop-result.json`、`stopped-status.json`。

## 实现

- GUI api先读响应文本再解析JSON，非JSON明确报告路径/HTTP状态，并提示检查正式:5143地址；不直接展示Unexpected token错误。
- 移除两个后端可编辑输入；显示固定后端说明。UI保存按显式codec派生turbojpeg:cpu或pyrowave:vulkan，API/CLI仍严格核对，无自动换codec/降尺寸/降fps。开发授权保留，Raw不改隐藏配置。
- 最大码率在PyroWave下标为“压缩预算 / 最大发送码率”，只读显示原生公式`min(maxMbps*1e6//8//fps,max_au_bytes-48,NV12帧字节数)`和相对原帧预算比。640²@240/80Mbps为41,666B约14.7:1，160Mbps为83,333B约7.4:1；不是实测压缩比。当前固定上游pyrowave_rate_control仅maximum_bitstream_size，没有JPEG quality参数，不新增假旋钮。
- fps说明明确DDA/WGC源更新/DWM限制，目标为上限，不制造重复帧补足。

## 59fps证据与未定位边界

这次真实会话：WGC显示器0、2560x1440源、640²@240 crop/PyroWave Vulkan/GPU转换、batch69、MTU9000、80Mbps；PC2 Realtek 2.5GbE发送→PC1 I226-V接收。不是motion测试源（stats中的pattern=motion为保留参数，实际mode=wgc）；不作为性能验收。

- 17,014帧，连续采集调用起点间隔（0<delta<100ms）中位16.9329ms，p10=15.7035ms、p90=17.9618ms，主峰17/16ms；>=100ms间隔292次。该时间是调用开始，不是WGC SystemRelativeTime，不能当精确display present计数。
- 每帧encode_wait中位0.4924ms、max2.6686ms；全会话rate_wait总5.404ms；不支持编码耗时或80Mbps限流把该会话卡成59fps的解释。encode计时不代表完整端到端成本。
- Win32_VideoController报告RTX5070Ti当前2560x1440/180Hz；另有GameViewer Virtual Display Adapter（刷新率null）。此信息未精确关联被WGC选中的HMONITOR，不能据此认定该capture源一定180Hz，也不能简单认定显示器60Hz。未改显示配置。
- 原生无显式60fps限制，WGC使用CreateFreeThreaded帧池、仅消费系统供给的新帧。仍需同一持续变化画面的WGC Raw / WGC PyroWave / DDA对照及精确输出/帧到达诊断定位；本轮未重新启动采集或擅自改源，不声称瓶颈已修复。

## 验证与部署边界

- GUI专项34 passed（最终固定后端序列化修改后重跑仍34通过）。完整`python scripts/verify.py`：350 passed / 6 skipped，build+sender/receiver selftest+pattern_ref ALL PASS；`build/verify-ui-controls-20261114.log`。全套pytest发生在最终固定后端序列化修改后，专项再覆盖该修改。
- 18145隔离预览加醒目只读标记，拒绝变更接口并返回JSON；浏览器无控制台错误，验证后端input hidden及80→160Mbps预算41,666→83,333B。预算修改只是预览草稿，未保存正式配置。截图工具证明新界面。
- 验证完关闭18145，正式预览导航回127.0.0.1:5143/ui；没有留下可误用的验证服务。
- 新源码未冻结、未打包/部署、未commit/push；正式0.5.1仍旧GUI。OTA机制修复证据另见bringup-20261114-ota-layout.md。实际59fps根因与修改后真实用户端验收未完成。
