# 主机 GUI 与真实后端整合（2026-10-29）

用户要求开始对接后端整合项目；本轮延续 D17 的主机 UI / 被采集机后台分工。

## 实现

- 主机使用真实 receiver、注册虚拟采集卡，再向 capture 后台下发参数启动真实 sender；停止时先停发端，再停接收并回收虚拟卡。
- 产品启动检查约 1 秒内的原生进程提前退出，返回 500 和限长 stdout；保留既有失败回滚。此检查不等于首帧确认，界面在尚无完整帧时明确等待。
- stats 显示 PID、退出码、错误摘要及日志相对路径，sender 显示实际 mode/pattern，避免被后台保存配置误导。
- 统一隐藏启动脚本支持源码和便携目录；主机与采集后台各有双击入口。启动前校验 5143 已有服务的角色、能力和 root，同目录同角色复用，冲突拒绝。Windows PowerShell 5.1 中默认 Root 在脚本体解析，避免参数默认表达式无法获取 PSScriptRoot。
- 打包载荷与成品冒烟加入采集后台入口、健康角色/目录校验和 UI 404。agent 仍为单文件、零运行依赖；wire、原生判据与产品隐藏默认参数不变。

## 本机真实验证

两份真实 source agent 使用隔离 ROOT：主机 127.0.0.1:18083、后台 127.0.0.1:5143，运行本次构建的原生程序，没有模拟统计。网卡 `\Device\NPF_Loopback`、MTU 9000。

| 检查 | 结果与证据 |
|---|---|
| motion / 240 / batch1 | 26957 帧收发逐帧一致，无缺帧、额外帧和 hash mismatch；SHM 20/20 与 receiver hashlog 一致；`run/gui-integration-20261029/current-pattern-evidence.json` |
| 真实 DDA / GPU / batch1 | 7431 帧收发逐帧一致，无缺帧、额外帧和 hash mismatch；SHM 20/20 一致；`current-dda-evidence.json` |
| DShow 实际读流 | motion 5 秒 1205 帧、DDA 5 秒 1168 帧，FFmpeg copy 至 null，退出 0；`current-pattern-vcam.log`、`current-dda-vcam.log` |
| 自动生命周期 | DDA 开始后枚举及 NV12 640x640@240 格式通过；两轮停止均确认 managed=false、注册路径清空；上述 evidence 与 `current-dda-enumeration.log` |
| 源码启动器 | PowerShell 5.1 启动成功；重复启动 PID 不变；改为 capture 入口返回非零且原 host PID 保留；`source-launcher.json` |
| 真实启动失败 | 带空格的隔离 ROOT 使用 capture 启动器；无效网卡令 native exit 2，接口返回 500、PID/日志与错误摘要；`startup-after.json` |
| 完整总闸 | `python scripts/verify.py --live Loopback`：65 passed / 2 skipped，ALL PASS；`verify-full.log` |
| 浏览器真实界面 | 显示 DDA 实际约 68 fps、真实计数与虚拟卡交付；`real-dda-ui.png` |

所有证据位于 `run/gui-integration-20261029/`。早先 motion 回环还完成 59952 帧全量一致对账，保留 `pattern1-reconcile.json`。batch69 回环曾没有完整帧，已记录；项目既有 Loopback live 约定 batch1，本轮未更改产品 batch69 默认。

本机已有下游程序持有旧 build DLL，清理构建时发生文件占用；将该文件保留为 build/Release/vc-vcam.loaded-*.dll 后完成重新构建，没有结束用户下游程序。下游重开后才能加载新版虚拟卡。

本机 DDA 实际约 68 fps，虚拟卡静默重复帧符合 D12；本轮仅证明真实后台、传输、共享内存和 DShow 流程接通，不声称源采集达到 240 fps。

## 交付与剩余范围

便携包通过 `python scripts/package.py --allow-dirty` 生成，过程自带完整 verify、冻结 host/capture 冒烟与载荷清单；运行日志在 `build/package/package-*.log`，产物入口为 `dist/VirtualCapture/Start-VirtualCapture.cmd` 和 `Start-Capture-Worker.cmd`。

截至 2026-10-29 本轮结束，用户已确认被采集机仍为 192.168.2.59，稍后上线；当时 5143/旧 18080 不可达，未部署或验证远机，本机回环不代替双机结论。这是当日历史边界，不是当前部署状态。

## 后续实际使用与封存（2026-11-02）

owner 确认已完成约三天实际效果测试，目前满意。只读核对显示 PC1 192.168.2.100 为 host，PC2 192.168.2.59 为 capture，两端 :5143 在线、均报 0.4.1 / product_revision=2；当前清单相同，主机 87 项载荷及封存前 299 项源码哈希均与成品清单一致。

已封存主机可恢复成品、两端配置与最新完整 hashlog、当前源码和 Git bundle；未重启、重建、提交或 OTA。远端 PE 文件尚未独立导出或校验实际哈希，既有 dropped=1 观察亦保留。使用满意不回填为连续 72 小时、零丢帧或专项自动化验收通过。详细证据与恢复边界见 `../bringup/bringup-20261102.md`。
