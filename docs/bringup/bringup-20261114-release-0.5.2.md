# 0.5.2 WGC 1ms / GUI / OTA更新（2026-11-14）

owner要求“打包部署”。本次来源HEAD bb7a6ff及本会话已有dirty改动（含同会话委托完成的OTA修复），不提交。发布内容：WGC固定1ms并读回、无接口明确拒绝；GUI指标自适应单位/非JSON错误提示/固定后端说明/PyroWave每帧预算；OTA实际native路径/四项运行库副本/事务恢复及版本化启动指针；保留0.5.1已实现的WGC窗口枚举与DShow NV12/MJPG协商。

部署前双端0.5.1，PC1 host/receive ROOT=dist/VirtualCapture，PC2 capture ROOT=Documents/VirtualCapture，running=false。保留两端配置、完整旧载荷及运行证据，不改编码/尺寸/fps/显示模式/网卡，不自动开始采集。PC1当前Windows不支持IGraphicsCaptureSession5，更新后其WGC启动会明确拒绝；PC2私有实采已确认1ms读回。无240独立画面/长稳/画质新验收。

打包使用独立dist/VirtualCapture-0.5.2，避免打包脚本直接覆盖运行目录。PC2先agent-only新版本切换，再通过新agent更新完整native与四DLL；不以旧agent能自动处理新布局为前提。独立备份为回滚依据，不手删staging/prev。执行结果如下。

## 成品验证与归档

- `python scripts/package.py --allow-dirty --output dist/VirtualCapture-0.5.2`完成；build/双selftest/pattern_ref全绿，pytest 350 passed / 6 skipped（177.37s）；PC1不支持IGraphicsCaptureSession5，WGC硬件测试跳过，不等于PC1支持1ms。
- PyInstaller成品host健康/UI/vcam定位、capture健康/UI404、PowerShell5.1解析/便携路径检查通过；116项载荷逐项SHA256校验通过（14.4MiB目录）。来源HEAD bb7a6ff及manifest记录的dirty源码，构建期间源码快照未变化。
- 日志`build/package-0.5.2.log`及`build/package/package-20261114-014407.log`；目录`dist/VirtualCapture-0.5.2/`。源码之后仅追加部署证据/PLAN，不冒充与打包时文档字节相同。

| release/归档 | SHA256 |
|---|---|
| VirtualCapture-0.5.2-windows-x64.zip | 7ba38c15bf931fc481fe66f6d62390d4ccb33e7b6c4dd6b122f842f6ed8a5727 |
| vc-0.5.2-agent.zip | 116793d515a7b98ae8dec345a1c1cf772261891b0dcd4329164835c7b72b0987 |
| vc-0.5.2-native.zip | d654e21635b73b525cd0093bdfe507ee1c706f0c975249dcdd676110bbdea3cc |
| vc-0.5.2.zip | 63ee9407dbdda372048735389c7a7a504fd8b2af0b9323552c00d9063d849cef |

完整portable已发送owner；旧agent升级必须先agent-only、后native，不能直接用总OTA包绕过旧白名单。

## 备份与部署

- 主机完整旧目录：`archive/predeploy-0.5.2-20261114-014526/host/`，143文件独立哈希清单。PC2备份：`C:\Users\s1973\Documents\VirtualCapture\archive\predeploy-0.5.2-20261114-014521`，排除archive自身、保留ROOT其他全部载荷/配置/运行数据/build/scripts/prev/staging，备份内有manifest。不手删live OTA机关。
- PC2原agent PID19388，session1；通过旧agent上传agent-only，activate时连接重置后新0.5.2 PID7176接管，版本化`vc-agent-0_5_2.exe`哈希与新包一致。确认后新agent接受native+四DLL更新，restart=none；实际build中四native及ROOT/build中四DLL逐项新包哈希一致，版本指针保留。两步完整日志在`run/release-0.5.2/deploy-capture.log`。
- 主机先preview_stop原0.5.1服务，检查5143无监听/该ROOT无agent进程，再复制受管载荷。首次复制遇WinError32：AimMagic_1.1.16 PID18236持有旧vc-vcam.dll；检查该DLL与新包字节相同，改为跳过相同文件，未关闭下游。完成后116项全部新包哈希通过，vc-config.json字节不变，旧run与未管理文件保留。
- 主机通过preview启动正式`dist/VirtualCapture/vc-agent.exe --role host --root ...`，健康0.5.2 PID21784、receive；PC2健康0.5.2 PID7176、capture。两端原ROOT不变、running=false、/config对象与更新前一致。主机无旧agent监听，不改任务/watchdog/网卡/显示模式，不注册虚拟卡、不开始业务流。

## 更新后核验

- 两端/stream/capabilities native.errors=[]，/adapters正常。
- 正式主机POST /stream/plan refresh=true：当前WGC显示器0、640²@240/crop/PyroWave、80Mbps、inflight1，implementation.supported=true、reasons=[]、can_start=true、network=fits。此为初始化/预算协商，不是240吞吐或真实客户端验收。
- PC2执行实际选中`build/vc-sender.exe --selftest`，exit0、未截断，WGC: PASS 2560x1440中心640 NV12；WGC init必须设置并读回1ms后才能成功，所以部署后实际WGC启动通过。无网络会话、无hashlog新流产生。
- 正式5143 GUI显示两端0.5.2；配置页固定PyroWave Vulkan后端、两个旧输入为hidden、每帧预算41,666B/约14.7:1，符合保留配置。console无错误，后续network无失败请求。初次预检并发浏览器轮询曾因客户端断开记录ConnectionAbortedError，后续API和GUI均正常；不将其隐去或认作部署失败。
- 原始健康/配置/备份/进程/hash/自检/协商证据：`run/release-0.5.2/`。未自动开始采集，修改后的双机吞吐/240独立画面、虚拟卡真实客户端、长稳与画质仍未验收；没有提交或push。

