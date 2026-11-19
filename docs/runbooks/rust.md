# Rust 运行与本机诊断

构建/完整包见 `build.md`，详细API/生命周期见 `.agents/skills/virtualcapture/references/rust-control.md`。
仅在独立根目录验证；运行开发版不代表授权替换当前双端部署。

## 运行

便携包：主机双击 `VirtualCapture.exe`，被采集机双击 `Start-Capture.cmd`（无参数默认15143）。主机输入 `IPv4:15143`；省略端口和直接运行vc-capture默认仍为5143。主机无本机REST，无浏览器UI。

源码构建独立启动示例（先自行选择不与已用服务冲突的运行目录/端口）：

```powershell
.\rust\target\release\vc-capture.exe --bind 127.0.0.1 --port 15143 --root .\run\capture-dev --native-dir .\rust\target\release
```

```powershell
.\rust\target\release\vc-host.exe --root .\run\host-dev --native-dir .\rust\target\release
```

Ctrl+C正常退出采集后台；GUI停止/正常关窗按精确会话停远端、本机DLL和本次管理的虚拟卡。真实注册操作必须单独授权；测试优先使用下述不注册的隔离工具。

配置独立 `vc-rust-config.json`，不导入旧Python配置。CLI `--root/--native-dir` 优先于环境变量；显式native目录缺文件就失败，不从旧构建补齐。默认DDA/硬件GPU/640²@240/Raw/crop；pattern是显式合成诊断。普通产品只允许已识别物理网卡，默认优先唯一I226；多I226须选定，失效选择需重选。MTU默认读取双端值，可切手动，不能超过已报告网卡限制；不修改驱动Jumbo。

PyroWave需显式授权开发编码、精确双端 `pyrowave:vulkan` 与Vulkan1.3真实GPU；不支持MJPEG/CPU转换，无自动降级。深蓝画幅只是尺寸比例示意，不是实时预览。

## OTA与证据目录

固定ROOT；无current时为扁平配置/run。有 `app/current.json` 后：

```text
app/releases/<build_id>/       程序与历史版本
app/current.json              当前指针
data/vc-rust-config.json       配置
data/run/                     证据与固定虚拟卡owner证明
updates/staged/ worker/ incoming/
updates/history.jsonl         激活与回滚历史
```

首次激活只复制配置/配对凭据，不移动旧run；失败可回原布局。令牌仅通过用户明确操作配对，不进health/普通配置/日志。现有根令牌不读取、显示、打包或删除。

完整Rust ZIP先stage校验，停止且无cleanup_pending才activate；helper等待旧PID退出、复核载荷、切current并确认新ROOT/build/就绪，失败恢复旧指针重启。HTTP202不是成功证明。与旧Python OTA不互通；版本校验/SHA256不等于发布者签名。不要手动改staging/prev或owner证明。

## 本机专项（均不替代双机验收）

先 `python scripts/verify.py`；以下按所需前提显式执行，证据写gitignored run/。

| 脚本（`python -I rust/tests/<脚本>`） | 验证内容 |
|---|---|
| `transport_vectors.py` | 独立Python oracle、冻结向量、Rust pcap回放，不开网卡 |
| `raw_stream_smoke.py` | 双Rust CLI、motion/640²@60/120帧/Loopback/batch1、全量参考哈希与私有SHM |
| `raw_stream_perf.py --batch 1` | motion/640²@240/2400帧/Loopback，同机cadence/延迟与独立CPU门 |
| `dll_stream_smoke.py --package rust/target/release` | 真实DLL同进程ABI、Raw收发/完整hash/SHM、stop/destroy |
| `dll_stream_smoke.py --package rust/target/release --codec pyrowave` | 显式PyroWave压缩传输/解码证据，需同目录原生库 |
| `source_dll_smoke.py --dll rust/target/release/vc_stream.dll --allow-dda-unavailable` | 私有Tk动态窗口、WGC硬件GPU/缩放/关闭；DDA无输出记BLOCKED，不换源 |
| `vcam_dll_smoke.py` | Rust隔离验证器真实加载COM DLL、SHM/重复/阻塞Stop/Disconnect七场景，不注册 |
| `adapter_smoke.py --package rust/target/release` | CLI完整枚举与产品物理列表、非显式Loopback拒绝；不收发物理网卡 |
| `control_smoke.py` | 随机端口/独立ROOT，REST+真实DLL Raw回环、精确停止与证据保护，不注册 |
| `gui_smoke.py --package rust/target/release` | 真实GPUI交互、HTTP错误与正常退出，非空白截图硬门 |
| `ota_smoke.py --package <完整包>` | 私有ROOT/端口、capture/helper stage/activate/rollback |
| `host_update_smoke.py --package <完整包>` | 私有HostController/更新助手/真实GPUI重开 |

仅dll/source等支持的脚本可用 `--self-check`；具体参数以该脚本 `--help` 为准。图形native程序不通过浏览器预览验证。

## 故障边界

- pending不是释放；DLL stop/destroy未确认不得卸载，也不能改配置/OTA/注册。PyroWave每操作5秒看门狗只标粘性错误，可能挂起的对象留至进程退出。
- 虚拟卡只允许匹配全局owner与本根精确schema2证明的恢复；其他根、其他存活宿主、旧注册无owner、路径/进程状态未知均拒绝。不要手动绕过保护。
- 无DXGI输出只把DDA实采记BLOCKED；不能用WGC或pattern给DDA补PASS。GUI黑屏就是FAIL。
- 旧双机自动矩阵工具已移除，Rust替代尚缺；门限不变。实际物理NIC/双机/长稳/画质/完整产品DShow及便携门仍须独立证据。
- 历史本机结果在 `docs/bringup/bringup-20261119-rust-r3-local.md`；其中旧MJPEG数据仅属当时版本，不是当前支持。
