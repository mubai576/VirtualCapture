# 0.5.0 提交、打包与部署记录（2026-11-13）

## 结论

owner 本轮要求“提交 打包 部署”（D25）。修复与版本提交为 `62e37cd56e46eba9c79d6ff1324f637e7bb7f9e0`，
分支 `release/p6-review-fixes-0.5.0`；未 push。完整便携包已构建并验证，本机主机已启动 0.5.0。
**本轮未执行PC2部署**：只读预检发现其5143实例是旧版 host/receive 而不是 capture，按切换门禁停止远端推进；最终并发检查发现PC2已由本轮之外更新为0.5.0 capture并运行PyroWave，见下文。
没有对PC2调用update/activate/exec、写配置、结束进程或修改注册。本轮不构成双机、长稳、画质验收。

## 包与来源

- `python scripts/package.py` 成功；源码 clean，manifest revision 为上述提交。
- 构建、双selftest、pattern_ref、pytest通过：299 passed / 6 skipped（默认非live总闸）。
- 冻结成品host健康检查/UI/虚拟卡路径及capture健康检查/UI 404通过。
- 便携成品108个manifest文件逐一复核SHA256通过，约13.5 MiB。
- 打包日志：`build/release-0.5.0-package.log`、`build/package/package-20261113-225551.log`。
- 打包后OTA专项：11 passed / 1 skipped。跳过的是旧测试寻找`dist/vc-agent.exe`，实际新产物位于
  `dist/VirtualCapture/vc-agent.exe`；不冒充冻结OTA往返通过。成品启动烟测已独立通过。
- 打包前本机回环、真实WGC及编码/DShow证据见 [review修正记录](bringup-20261113-review-fixes.md)。
  该记录中的“未提交/未打包/未部署”描述当时状态，由本文记录后续执行。

| 文件（均在release/，不入git） | SHA256 |
|---|---|
| VirtualCapture-0.5.0-windows-x64.zip | d9b0f3c1aa39d373c76adbbf10e5feebe1184df2fbb3e4d1233f719dab3e5d97 |
| vc-0.5.0.zip | c154ad7bdf5350e086a0772af1b4808d5c72df654e19911d659f953dc05ff198 |
| vc-0.5.0-agent-bootstrap.zip | ae3a7525f9c706d67068a278e8b3a5f92c06021123068ac2aa4b1f99c4f6f6a4 |

完整便携zip在恢复本机配置前生成，不含主机配置/run。OTA zip严格仅允许顶层agent/native/运行DLL/version及scripts/*.ps1，
不将完整便携目录当OTA。bootstrap只含agent.exe/version，供旧allowlist先升级；本轮两种OTA均未发往PC2。

## 本机切换与回滚点

- 打包前配置、版本、manifest和173个旧文件的大小/SHA256清单：`archive/predeploy-0.5.0-20261113-225617/`。
- 上一份完整成品由打包器归档到 `dist/VirtualCapture.archive/VirtualCapture-20261113-225838-7b0ed3ad/`。
- 新ROOT：`D:\Project\REA\VirtualCapture\dist\VirtualCapture`；恢复原主机vc-config.json，
  保持peer=192.168.2.59:5143、原网卡、DDA/GPU、Raw 640×640@240、batch69、MTU9000。
- 使用本地preview启动配置运行冻结agent：host/receive、5143；核验时PID15532。
  `/health` version=0.5.0、product_revision=2、stream_revision=2、ROOT匹配，两槽均停止。
- `/stream/capabilities`显示Raw/MJPEG/PyroWave、四种整数变换、SHM v3，原生errors为空。
  codec候选不是实帧验收；未自动启用有损编码。
- GUI正常显示0.5.0以及“请更新被采集机后台”；浏览器无console error。未点击开始采集。
- 本机虚拟卡原先无注册，切换后仍无注册；本轮未注册/注销，不干扰下游。
- 精确API快照：`run/deploy-0.5.0-final-snapshot.json`。
- 回滚须先停止本轮host及其子进程/下游，再使用完整旧ROOT与旧配置，不能混搭二进制。
  当前host由本地preview进程管理；没有新增开机自启、任务或watchdog。

## 最终并发变化（停止进一步部署操作）

初始部署快照后，核验配置字节时发现当前配置已不再等于保存的Raw配置；失败断言保留，不能记为配置持续不变。
再次只读检查发现codec=pyrowave、codec_experimental=true、双端backend=pyrowave:vulkan、decode_in_flight=1；
本机receiver会话`s-29370e86`、PID28220正在运行，观察时complete=285。
这些配置保存和采集启动不是本轮执行的，不能归因于本轮，也不据此宣称本轮双机部署成功。
没有覆盖现配置或停止活动会话。最新只读证据为`run/deploy-0.5.0-latest-host.json`。
因此上文Raw/两槽停止/无注册是初始核验时状态，不代表最终活动状态；旧Raw配置和旧ROOT仍完整保留。

## PC2阻塞与保留边界

PC2最初5143拒绝连接，稍后恢复；后续多次只读结果一致：

- IP192.168.2.59，ROOT=`C:\Users\s1973\Documents\VirtualCapture`。
- version=0.4.1、role=host、product_role=receive、product_revision=2，缺stream_revision，capabilities=404。
- PID7468，两槽均停止；配置peer指向自身且网卡GUID属于本机主机配置，不符合capture部署。
- 真实PC2 Realtek网卡GUID `{1C8C6B1E-3112-4C00-BCD0-F77E37E66198}`，MAC `00-E2-69-A9-F0-0A`。
- 虚拟卡注册指向旧ROOT下build/vc-vcam.dll，与当前ROOT/vc-vcam.dll不匹配；未修改它。
- 原始预检：`run/deploy-pc2-ready-20261113-225711.json`，最终快照同上。

按 [P6切换门禁](../runbooks/p6-cutover.md) 的角色/恢复通道检查，远端在此停止。
仅OTA会沿用原host启动argv，不能修正为capture；未确认独立恢复/重启方式时不得结束旧agent。
远端完整字节备份、角色纠正、agent-first升级、运行时部署及双端协商核验均未由本轮执行。

最终并发只读检查（`run/deploy-0.5.0-latest-pc2.json`）发现PC2已为version0.5.0、role/product_role=capture、
stream_revision=2、PID6008、ROOT不变；sender会话`s-12d92b94`、PID7764、PyroWave 640²@240配置、
实际DDA源，观察时sent_frames=322、send_errors=0。不能把配置240fps或静态桌面DDA统计当motion吞吐验收。
这些远端改变不是本轮执行；仅核实当前健康/角色/会话，没有校验远端二进制manifest或执行回滚测试。
