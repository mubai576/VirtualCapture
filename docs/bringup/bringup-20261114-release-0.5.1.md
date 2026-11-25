# 0.5.1 配置与虚拟卡格式更新（2026-11-14）

## 结论与未完成项

owner 要求“直接打包更新吧”。本轮完成开发、打包、本机主机更新与PC2 OTA切换。PC2曾离线；owner恢复后，本轮定位并补齐build旁缺失运行库，双端0.5.1健康、原生能力、网卡枚举、远端窗口枚举及当前配置启动前协商均通过（can_start=true）。两端未由本轮启动采集；真实业务客户端/双机吞吐/长稳/画质未验收，不能把启动前通过记为实际流验收。未commit/push。

## 实现

- GUI显式DDA/WGC/测试图案、显示器编号；WGC窗口从被采集机枚举并刷新，不要求手输标题。主机POST `/capture/windows`代理对端GET同路径，不使用主机窗口。标题非法/子串歧义禁选，原生启动重查唯一性。
- 移除目标码率、固定GOP、不可选后端类别控件；保留实际最大码率/在途上限/质量/后端/开发授权。
- 虚拟卡客户端协商NV12（默认）或实际MJPG；尺寸/fps仍为会话固定。MJPG复用libjpeg-turbo CPU/SIMD、质量80、420，每样本独立JPEG；按最坏容量分配、按实际长度交付。内部SHM不变，D12不变，MJPG另有CPU/有损开销，不承诺240fps、不套Raw像素相等判据。

## 验证与包

- 打包总闸：328 passed / 6 skipped；构建、双selftest、pattern_ref全部PASS。
- 冻结成品host健康/UI/vcam定位及capture健康/UI404烟测通过。
- 110项成品文件SHA256逐项通过；来源HEAD bb7a6ff8f0559a9191107325409461fd482ea2a7 + 本轮dirty源码；未混入其他会话文件。
- `VC_WGC_LIVE=1`私有窗口CPU/GPU resize/close：3 passed（本机功能，不是性能）。
- 双格式18组隔离样本测试含2x2、418x282、640x640，NV12精确平面、MJPG真实解码色度误差检查、阻塞Receive/持样本Disconnect/停止均通过。静态功能向量不是motion性能验收。
- 隔离GUI验证真实采集worker窗口枚举、选择/保存/重载、无效选项移除、375px无横溢出；没有改稳定配置。
- 日志：`build/package-0.5.1-resume.log`及其中打包日志；中断旧日志`build/package-0.5.1.log`不记成功。
- 成品目录`dist/VirtualCapture-0.5.1/`。

| 归档（release/，不入git） | SHA256 |
|---|---|
| VirtualCapture-0.5.1-windows-x64.zip | 37c87484c574a9a67b87ded0d073208cfe3e34f4373feefea4e6777e190b1f99 |
| vc-0.5.1.zip | 3717d97311f2a7caef180db911d3209dda3802bb01f652f966c3d5f717f6735d |

OTA仅agent/native/PyroWave DLL/version及scripts，额外release-inspect.ps1用于本轮备份/哈希核验；完整便携zip不含本机配置/run。

## 部署与备份

- 本机原5143服务在继续会话时已停止；确认无本机发收/agent进程后覆盖已备份的`dist/VirtualCapture`受管载荷，配置字节不变；使用preview启动正式host。健康PID15740、version0.5.1、receive，110项载荷哈希通过。
- 本机完整旧目录：`archive/predeploy-0.5.1-20261114-001949/host/`，备份清单同目录`backup-manifest.json`。
- PC2 ROOT仍为`C:\Users\s1973\Documents\VirtualCapture`，原capture0.5.0停止状态。先通过固定allowlist部署备份/核验脚本，保留旧版本号；脚本最初有PowerShell大小写变量冲突，报错后修复，仅诊断脚本，无应用配置改动。
- PC2独立完整恢复备份：`C:\Users\s1973\Documents\VirtualCapture\archive\predeploy-0.5.1-20261114-002316`。OTA自身`prev/mirror`也保留，但其旧原生备份不全（旧payload在ROOT，OTA槽位在build），恢复应使用独立备份，不假设prev包含整套旧原生。
- 已完整分页拉回两次sender hashlog（s-5ef1b8b3、s-12d92b94）及stats/stdout到本机archive/capture-evidence；/files为文本接口，CRLF重新编码导致本机字节数与远端不同，不称其原始字节哈希副本；记录内容完整。
- PC2 `/update` staged0.5.1，activate连接在execv时重置，随后/version确认0.5.1。健康PID14300、capture；实际agent为vc-agent-0_5_1.exe。核验6项agent/native/DLL均与包SHA256一致。
- PC2 OTA新原生实际位于build，PyroWave DLL位于ROOT。后续能力探测拒绝/离线可能涉及DLL搜索或进程/启动器生命周期，**尚未确认根因**。
- 主机窗口枚举代理曾200、6个远端窗口；未本轮点击product/start。浏览器另有POST product/start返回409（缺采集后台能力），随后PC2 5143拒绝连接；这是失败，不记为通过。
- 页面配置随后由本轮之外改为WGC/640x640/PyroWave240，未覆盖；初始保留配置DDA/416x416不再代表最终用户选择。最终快照在`run/release-0.5.1-preflight/final-snapshot.json`。
- 所有预检/OTA/远端哈希原始结果在`run/release-0.5.1-preflight/`。未修改网卡/防火墙/任务/watchdog，未全局按进程名杀进程。

## 阻塞与恢复

初次PC2 agent离线时，HTTP控制通道与SMB管理共享不可用。提供`release/Recover-Capture-0.5.1.cmd`供PC2登录桌面恢复。owner随后确认“已运行，继续核验”；后续以实际API与程序结果判定，不以脚本运行确认代替健康检查。

### 恢复后核验（2026-11-14）

- 双端健康：PC1 host PID15740、PC2 capture PID19388，均0.5.1，ROOT正确、running=false。远端实际进程为ROOT/vc-agent.exe，SHA256与0.5.1包一致；不存在版本化exe与recover日志。故不能声称该恢复脚本的版本化启动分支成功，实际启动方式及此前离线原因仍未确认。
- 恢复后仍有原生失败：build/vc-sender.exe `--capabilities`退出3221225781（0xC0000135，DLL未找到），ROOT同名同版sender退出0且完整声明Raw/MJPEG/PyroWave。inspect确认四项运行库仅在ROOT，build无对应文件。
- 通过现有/update允许的scripts槽位部署固定诊断脚本`native-runtime-inspect.ps1`，无self更新/重启、无配置改动。仅在健康为同ROOT、idle capture0.5.1时允许补缺：ROOT的libpyrowave-shared-1.dll、msvcp140.dll、vcruntime140.dll、vcruntime140_1.dll复制到build；遇已存在不同哈希拒绝覆盖。最终四项两位置SHA256逐项相同。最初诊断JSON因PowerShell扩展属性递归被截断，修正后`runtime-final.json` exit0/truncated=false，截断输出不作完整证据。
- 修复后PC2 `/stream/capabilities` errors=[]、variable_resolution=true、codec_revision/mjpeg_revision/pyrowave_revision均1，Raw/MJPEG/PyroWave完整；`/adapters`恢复200，识别Realtek PCIe 2.5GbE（192.168.2.59、2.5Gbps）。PC1 I226-V同为2.5Gbps。
- 主机POST `/capture/windows`代理返回200、3项真实远端窗口；未选取或改变窗口配置。
- POST `/stream/plan` refresh=true：当前WGC显示器0、640x640@240、crop、PyroWave Vulkan、80Mbps上限、在途1，返回implementation.supported=true、reasons=[]、can_start=true、network.status=fits。包含两端实际codec-check初始化协商，但hardware_verified=false，不能当作真实画面/性能验证。
- 最终host配置与本轮恢复前JSON对象精确相同；两端仍未启动sender/receiver。主机虚拟卡尚未由本轮注册（配置为启动接收时自动注册），不声称真实业务客户端已经验证。
- 完整证据在`run/release-0.5.1-preflight/recovery/`：hashes.json、sender-build.json、sender-root.json、runtime-final.json、capture-capabilities.json、capture-adapters.json、host-plan.json、windows.json、final.json。
- **剩余边界**：本轮仅修复当前PC2文件布局，未把OTA槽位/依赖布局问题修入产品源码、未重打发布zip；原归档SHA256保持不变。后续干净机器执行旧OTA路径仍可能重现；不把单机补DLL当成OTA机制已修复。实际双机流、客户端、长稳、画质与此前agent离线原因均未验收。
