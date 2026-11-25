# 实时指标显示排查（2026-11-14）

## 结论

owner反馈“接收帧率 采集帧率 链路带宽 没有统计到”。实际API存在数据，不能判定三个指标都未统计：初次与53秒后采样sender.sent_frames和receiver.complete均518，uptime增长且进程running，因此此期间实时增量0fps属实。后续不启停任何进程也观察到计数恢复增长至1330、1948及以上，只读源码预览出现非零收发fps；没有把配置240fps或原生会话平均fps填成实时帧率。仅凭这些计数无法区分WGC源静止与暂时采集停滞，不宣称根因已经确定或240fps验收通过。

## 修复

- 带宽原值约1–2Mbps，原GUI固定Gbps保留2位导致0.00；改为bps/Kbps/Mbps/Gbps自适应单位。
- 保留原生bps语义：发送优先、缺失时用接收，为会话平均而非实时带宽，不以其推断瞬时传输仍在进行。
- 标注实际Raw/MJPEG/PyroWave，不再固定写未压缩NV12。
- 零增量说明为“本次采样无新增采集帧/完整帧”，非零说明实时增量，未建立基线/离线/停止仍为未知。

## 验证与边界

- `python -m pytest ctrl/tests/test_gui_client.py ctrl/tests/test_gui.py -q`：32 passed。
- 新增单位边界、零值/缺失/无效发送值回退接收、会话平均语义、停滞计数与零/非零提示回归。
- 18144隔离源码预览仅代理正式5143的只读GET和不写配置的stream/plan，禁止启停/配置POST；初次启动wrapper导入错误已修正。早期禁止全部POST导致plan 403，仅属验证wrapper限制，改为允许固定只读plan后重启，最终控制台无错误。
- 浏览器实际观察收发fps均1、完整帧1948、带宽1.48Mbps；随后截图计数2056、0fps及1.51Mbps，反映不同采样区间，不混作同一快照。
- 原始状态在`run/stats-debug/status.json`、`status-later.json`；只读验证脚本在该目录，不入git。
- 未改原生程序/API/wire/性能判据，未启停真实采集，未重启正式host、未打包部署/commit/push；正式5143仍为旧GUI。完整总闸由同checkout的OTA修复会话统一执行，完成前不记ALL PASS。
