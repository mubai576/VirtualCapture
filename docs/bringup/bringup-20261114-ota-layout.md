# OTA 原生目录与运行库修复（2026-11-14）

## 范围与原因

owner批准“修复 OTA”。只修源码、启动脚本、回归与契约；不改版本、不重打包、不部署、不触碰现有双端服务/配置/注册表，不commit/push。GUI实时统计由另一会话并发处理，共用agent文件，以下仅归属OTA段。

0.5.1恢复证据见[发布记录](bringup-20261114-release-0.5.1.md)：原有ROOT原生被OTA固定槽位放到build，PyroWave与MSVC运行库却仍留ROOT；build/sender退出0xC0000135，ROOT同版正常。旧备份还因固定build槽位遗漏ROOT原生。这不是编码能力本身缺失。

## 修复

- `_update_slots()`沿用各native实际查找路径，包含ROOT便携布局和build/Release等开发混合布局；空冻结布局默认ROOT，空源码布局默认build，不新建更高优先级影子exe。
- OTA白名单加入msvcp140.dll、vcruntime140.dll、vcruntime140_1.dll，与libpyrowave-shared-1.dll组成四项app-local运行库。包内某DLL部署到各受管native目录；sender目录是主槽，其余副本用内部`_runtime/`键记录，上传不能使用该私有路径。
- 每个目录的原有DLL独立备份；activate复制失败和rollback均恢复旧字节或原先不存在。ROOT残留运行库在native选中build时也保留独立备份，不假装两位置同一文件。
- 上传native之前检查其目录中每项运行库已经存在，或本包提供。缺失400且未改prev/live；不靠ROOT作为cwd补偿DLL搜索。runtime-only包仍允许只换某项DLL。
- 非agent自更新的立即应用路径新增事务快照和失败恢复；恢复失败保留唯一快照。此路径仍在有运行会话时409拒绝，不改变既有stage/activate端点。
- 冻结agent非自更新包保留version.json的活动exe指针。start-agent.ps1安全读取同目录版本化镜像，非法/缺失指针拒绝，不悄悄启动旧vc-agent.exe。

## 验证

- 定向回归：`python -m pytest ctrl/tests/test_ota_layout.py ctrl/tests/test_ota_launcher.py ctrl/tests/test_ota_runtime_recovery.py ctrl/tests/test_stream_native.py -q`，**49 passed**。
- 包含ROOT/build/build-Release/混合目录的stage不碰live、activate、rollback；缺依赖拒绝；自更新和非自更新复制失败注入；runtime-only多目录及活动指针；私有快照路径拒绝。
- Windows PowerShell 5.1实际执行启动器，HTTP与Start-Process用私有测试替身，不打开5143、不启动服务。默认/版本化/唯一重装镜像选择与路径穿越/绝对路径/缺失镜像拒绝共6组通过。
- 真实便携成品native+DLL复制到私有根，stage/activate/rollback后实际执行sender/receiver `--capabilities`，两轮退出0并支持Raw；无build影子exe。使用既有0.5.1二进制仅验证Python布局操作及Windows加载器，不当作新冻结agent已发布。
- 最终完整总闸`python scripts/verify.py`：**348 passed / 6 skipped（177.89s）**，cmake配置/构建、sender/receiver自检、pattern_ref、pytest均PASS，ALL PASS；日志`build/verify-ota-20261114-final.log`。首轮347 passed / 6 skipped记录在`build/verify-ota-20261114.log`，最终重跑包含新增legacy-prev回归及同checkout并发GUI统计改动。6项跳过不等于双机/专项验收。

## 升级与未验证边界

- 旧agent先agent-only+version+启动脚本包，接管到新agent后再上传完整native+四DLL包。第一次请求仍由旧代码执行，直接给旧agent新完整包不会自动获得新白名单/布局。源码修复没有改变已发布0.5.1归档。
- 旧agent生成的prev若遗漏ROOT原生，不能凭空恢复历史字节；保留独立部署备份，不能宣称旧prev已修复或完整。新快照写内部`_runtime-layout.json`标记；旧prev无标记时不删除当时未纳管的MSVC DLL或额外副本，避免因未知历史缺失破坏现有运行库。新增legacy prev回归。
- 本轮没有实际双机OTA、冻结新agent发布、业务客户端、性能/长稳/画质验收。修复不涉及wire/编码模式或测试判据。
- 启动指针修复针对当前产品便携入口start-agent.ps1；历史install-pc2.ps1创建的SYSTEM任务仍直接运行固定exe，不属于本轮当前产品桌面入口验证，既有历史任务不会被本轮自动改写。
