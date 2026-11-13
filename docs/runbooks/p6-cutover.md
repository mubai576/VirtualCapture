# P6 双机隔离验证与切换门禁

状态：待owner确认，不是已执行记录。不使用历史`pc2-cutover.md`的18080/角色/计划任务步骤。
P6开发、编译、打包不等于授权替换稳定服务；正式GUI仍固定5143，备用端口仅用于低层诊断。

## 当前前置状态（2026-11-03只读快照）

| 项目 | PC1主机 | PC2被采集机 |
|---|---|---|
| IP/产品端口 | 192.168.2.100:5143 | 192.168.2.59:5143 |
| 角色 | host/receive | capture |
| ROOT | dist/VirtualCapture-P6-20261103 | C:\Users\s1973\Documents\VirtualCapture |
| 能力 | P6-2，stream_revision=2，仅Raw | 已用稳定后台，无stream_revision，/stream/capabilities为404 |
| 采集状态 | running=false | running=false |

同为version0.4.1不代表可协商编码。具体PID、网卡GUID、MAC与原始应答见
`run/p6-followup-20261103-094946-3a6e20/host.json|capture.json`，每次执行前重新核对，不沿用旧PID操作。
PC1 I226-V当前数值收/发链路速率2.5G；PC2旧API没有速率数值，不以网卡名称替代实际链路速率。

## 阶段A：只读预检（已执行）

只GET两端`/health`、`/version`、`/adapters`、`/stream/capabilities`、两槽stats。
发现任一已有会话运行、角色/ROOT/能力不符，停止推进；不能为通过测试自动停会话或升级。
保留两个稳定目录、封存快照、已有hashlog、计划任务与watchdog，不调用update/activate。

## 阶段B：独立目录/备用诊断端口（须先批准）

1. 先在PC2实际导出稳定二进制、配置、注册与启动方式清单，补P5-U4远端字节备份缺口；核对大小和SHA256。
2. 将已验证P6便携目录**复制**到全新明确路径，不覆盖Documents稳定目录；校验完整manifest。
   不复制开发机配置、run/或注册状态。不能把便携目录整体压缩为OTA包上传。
3. 在当前登录桌面用户会话、已确认空闲且可达的备用诊断端口启动capture，隐藏控制台并记录精确PID/ROOT。
   不创建SYSTEM任务、不改现有启动器/5143服务/防火墙/网卡；端口不可达先报告，不自动改系统设置。
4. 只读检查该实例原生codec_revision、PC2 encoder候选与显式codec-check，PC1独立实例检查decoder。
   枚举/类型协商不当作实帧通过。选用身份与硬件类别必须记录，不复用PC1 NVIDIA身份当PC2结论。
5. 后续低层诊断显式访问备用端口，使用私有ROOT、SHM和独立流source MAC；receiver不注册DShow。
   产品GUI不能填写备用对端端口，不改固定5143规则以绕过这一点。

## 阶段C：小规模物理链路（须批准B及本阶段）

- 开跑前再次确认稳定两槽停止、实际两端能力/版本、2.5G网口GUID/MAC、MTU和链路速率；不重设网卡。
- 先独立Raw验证，再按需验证MJPEG（H.264/H.265 已按 D23 移除）；先小尺寸/低fps后扩大。每组明确尺寸、变换、
  后端类别、码率、GOP、AU/队列上限、motion、物理网卡和batch；不将本机Loopback batch1结论外推为物理batch69通过。
- 先motion判功能，再真实DDA检查ROI确有变化；有损传输hash与解码到SHM验证分开，不套Raw像素相等门。
- 失败完整保留两端stats/日志，不能降尺寸/fps/更换codec/batch/队列后继续沿用原组PASS。
- 结束立即分页取回远端完整sender/hashlog；本机receiver transport/decoded证据也保留。
  跨机不相减QPC，不相加分段p99冒充全程；长稳与画质/色彩标准仍须独立确认。
- 只关闭本轮创建且核实ROOT/PID的诊断实例；稳定5143服务保持原样，不用按进程名全局结束。

## 阶段D：正式GUI/虚拟卡切换（另须批准维护窗口）

只有B/C通过且owner确认后才安排：保存最新配置和设备注册路径，关闭下游设备使用者，核实稳定服务的
精确PID/ROOT与watchdog关系；停止旧服务、切换新ROOT至5143、验证单监听与GUI/原生能力。
接收/注册在同一用户HKCU，DShow按真实新会话格式重开；新旧SHM版本/尺寸不能共用活动映射。
这会影响实际使用，不在“继续开发”授权内。

回滚必须使用保留的旧ROOT、配置和注册路径，不混用新旧EXE/DLL。先停本轮新实例及其子进程，
关闭新下游读端，再按原启动方式恢复旧版与原设备格式；不删除旧目录、staging/prev或测试证据。
无法确认无人值守可恢复时不得停旧agent；agent更新/重启最后安排。
