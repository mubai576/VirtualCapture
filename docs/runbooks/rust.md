# Rust 运行与本机诊断

构建/完整包见 `build.md`，详细API/生命周期见 `.agents/skills/virtualcapture/references/rust-control.md`。
仅在独立根目录验证；运行开发版不代表授权替换当前双端部署。

## 运行

两台电脑都启动 `VirtualCapture.exe`，首次分别选择本机角色：需要采集画面的电脑选择“采集端”，展示画面的电脑选择“接收端”。选择自动保存到 `vc-startup.json`，下次启动直接进入所选端；接收工作台或空闲采集页点“重新选择角色”可切换。采集页可用 `--capture-bind` 和 `--capture-port` 指定控制 API（默认 `0.0.0.0:5143`）；命令行 `VirtualCapture.exe capture` 仍用于需要控制台的采集/诊断。主机填写采集端 IPv4，非默认端口填写 `IPv4:端口`。

启动角色与采集参数分别保存：扁平目录用 `ROOT/vc-startup.json`，Managed 用 `ROOT/data/vc-startup.json`；回滚到扁平目录时仍优先读取已有 `data/` 偏好。文件内容为 `{"schema":1,"role":"capture"}` 或 `{"schema":1,"role":"receive"}`，不写入 `vc-rust-config.json`。配置损坏时会明确报错；在新包目录可用以下命令重选并保存：

```powershell
.\VirtualCapture.exe --select-role
```

主程序是 GUI 子系统，交互式 shell 直接启动它不一定等待。需要同步前台运行并取得退出码，在已解压的新包目录执行：

```powershell
.\vc-updater.exe launch --root . --role capture
```

源码构建独立启动示例（另选未占用端口及运行目录）：

```powershell
.\rust\target\release\VirtualCapture.exe capture --bind 127.0.0.1 --port 15143 --root .\run\capture-dev --native-dir .\rust\target\release
```

```powershell
.\rust\target\release\VirtualCapture.exe --root .\run\host-dev --native-dir .\rust\target\release
```

前台按 Ctrl+C 正常退出采集后台；GUI停止/正常关窗按精确会话停远端、本机DLL和本次管理的虚拟卡。真实注册操作必须单独授权；测试优先使用下述不注册的隔离工具。

配置独立 `vc-rust-config.json`，不导入旧Python配置。CLI `--root/--native-dir` 优先于环境变量；显式native目录缺文件就失败，不从旧构建补齐。默认DDA/硬件GPU/640²@240/Raw/crop；pattern是显式合成诊断。普通产品枚举状态为Up的物理非虚拟网卡，不按厂商或型号偏置；本机按控制连接路由自动选择，对端多候选须手动选择，失效选择需重选。MTU默认读取双端值，可切手动，不能超过已报告网卡限制；不修改驱动Jumbo。

PyroWave需显式授权开发编码、精确双端 `pyrowave:vulkan` 与Vulkan1.3真实GPU；不支持MJPEG/CPU转换，无自动降级。深蓝画幅只是尺寸比例示意，不是实时预览。

## OTA与证据目录

固定ROOT；无current时为扁平配置/run。有 `app/current.json` 后：

```text
app/releases/<build_id>/       程序与历史版本
app/current.json              当前指针
data/vc-rust-config.json       采集与传输配置
data/vc-startup.json           已确认的桌面启动角色
data/run/                     证据与固定虚拟卡owner证明
updates/staged/ worker/ incoming/
updates/history.jsonl         激活与回滚历史
```

首次激活复制配置，不移动旧 run；失败可恢复原布局。统一版就绪后，固定 ROOT 补齐缺失的 `VirtualCapture.exe`，或替换经旧清单和哈希确认的旧图形启动器。旧 ROOT 更新器和旧启动脚本保留原样；迁移后从 ROOT 用新主程序，采集沿用原 `--bind` / `--port`。更新不再需要配对令牌，连接后即可操作；控制端口 5143 须限制在可信网段。旧令牌文件不读取、不显示、不打包、不删除。

完整Rust ZIP先stage校验，停止且无cleanup_pending才activate；helper等待旧PID退出、复核载荷，按manifest capabilities选择目标启动参数，切current并确认新ROOT/build/就绪，失败恢复旧指针重启。旧D34 unified版本没有新角色参数能力标记，回滚不传 `--start-role`/`--resume-role`；图形采集更新目标需声明 `desktop_capture`，否则在停止旧进程前拒绝。HTTP202不是成功证明。与旧Python OTA不互通；版本校验/SHA256不等于发布者签名。不要手动改staging/prev或owner证明。

## 本机专项（均不替代双机验收）

先 `python scripts/verify.py`；以下按所需前提显式执行，证据写gitignored run/。

| 脚本（`python -I rust/tests/<脚本>`） | 验证内容 |
|---|---|
| `transport_vectors.py` | 独立Python oracle、冻结向量、Rust pcap回放，不开网卡 |
| `raw_stream_smoke.py` | 双Rust CLI、motion/640²@60/120帧/Loopback/batch1、全量参考哈希与私有SHM |
| `raw_stream_perf.py --batch 1` | motion/640²@240/2400帧/Loopback，同机cadence/延迟与独立CPU门 |
| `dll_stream_smoke.py --package rust/target/release` | 真实DLL同进程ABI、Raw收发/完整hash/SHM、stop/destroy |
| `dll_stream_smoke.py --package rust/target/release --codec pyrowave` | 显式PyroWave压缩传输/解码证据，需同目录原生库 |
| `source_dll_smoke.py --dll rust/target/release/vc_stream.dll --allow-dda-unavailable` | 私有Tk动态窗口、DDA/WGC硬件GPU/缩放/关闭；无限运行至少10帧后精确 stop+destroy 共用2秒期限。DDA仅确认无输出可记BLOCKED；0帧超时仍FAIL，不换源 |
| `vcam_dll_smoke.py` | Rust隔离验证器真实加载COM DLL、SHM/重复/阻塞Stop/Disconnect七场景，不注册 |
| `adapter_smoke.py --package rust/target/release` | CLI完整枚举与产品物理列表、非显式Loopback拒绝；不收发物理网卡 |
| `control_smoke.py` | 随机端口/独立ROOT，REST+真实DLL Raw回环、精确停止与证据保护，不注册 |
| `gui_smoke.py --package rust/target/release` | 首次真实点击选端、偏好保存/重启、两端重选、接收工作台交互；前台及非空白截图硬门 |
| `ota_smoke.py --package <完整包>` | 私有ROOT/端口、capture/helper stage/activate/rollback |
| `host_update_smoke.py --package <完整包>` | 私有HostController/更新助手/真实接收页重开，Managed ROOT 保存角色及显式重选转发 |
| `host_update_smoke.py --bin rust/target/release --native rust/target/release --capture-desktop` | 图形采集 OTA 后恢复窗口及原 bind/port；仍只使用私有 ROOT 与回环控制 HTTP |

仅dll/source等支持的脚本可用 `--self-check`；具体参数以该脚本 `--help` 为准。图形native程序不通过浏览器预览验证。

## 故障边界

- pending不是释放；DLL stop/destroy未确认不得卸载，也不能改配置/OTA/注册。DDA/WGC 的 NV12 回读使用非阻塞 Map、两平面共用1秒期限并响应停止，超时为真实失败、无CPU/WARP降级；不保证任意驱动调用或 teardown 可被中断。PyroWave每操作5秒看门狗只标粘性错误，可能挂起的对象留至进程退出。
- 开始时先只读预检归属/能力，再等 receiver 的真实 SHM 几何就绪后注册虚拟卡；接着启动远端 sender。下游已打开的 graph 不随输出尺寸自动换档；修改尺寸须停止并重开设备。只有 ready 不等于首帧或画质验收，主机深色尺寸框仍不是实时预览。
- 虚拟卡停止只回收证明属于本根的注册；启动在全局 CLSID 锁内确认无存活宿主占用后，可自动移除旧版、其他目录、损坏或无全局 owner 的残留注册并重注册当前 DLL；其他存活宿主或注册表/进程状态未知仍拒绝。不要手动绕过保护。
- 无DXGI输出只把DDA实采记BLOCKED；不能用WGC或pattern给DDA补PASS。GUI黑屏就是FAIL。
- 旧双机自动矩阵工具已移除，Rust替代尚缺；门限不变。实际物理NIC/双机/长稳/画质/完整产品DShow及便携门仍须独立证据。
- 历史本机结果在 `docs/bringup/bringup-20261119-rust-r3-local.md`；其中旧MJPEG数据仅属当时版本，不是当前支持。
