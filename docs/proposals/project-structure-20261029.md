# 2026-10-29 项目结构整理

本次按 owner「整理与优化项目结构」执行。目录地图仍以根 `AGENTS.md` 为唯一真相源；本页记录迁移理由与验收证据。

## 整理内容

| 原位置 / 问题 | 整理结果 |
|---|---|
| `sender/`、`receiver/` 各自持有相同的 `crc32.h`、`fnv1a.h` | 合并到 `common/`，通过 `vc-common` INTERFACE target 共用；算法原样迁移 |
| 发收端重复 Npcap SDK 探测 | 收敛到 `cmake/Npcap.cmake`，保留环境变量 → vendored SDK → 无 SDK 的原有顺序 |
| 多个 CMake 重复编译规则，辅助工具输出写死在源码目录 `build/` | `cmake/VirtualCaptureTargets.cmake` 统一 C++17、MSVC UTF-8 与输出目录；所有产物跟随 `-B` |
| 根目录散落 `monitor/`、`testwin/`、`vcam_verify/` | 迁入 `tools/`，保持构建目标名与既有 Release 产物路径 |
| receiver / vcam 隐式依赖 sender 的测试源头文件 | `vc-pattern` INTERFACE target 显式声明依赖，`sender/pattern.h` 仍为真相源 |
| 工具入口、输出路径不全 | 更新 `AGENTS.md`、skill、PLAN §1 与 build runbook，新增 `docs/runbooks/development-tools.md` |

保留 `proto/vc_wire.h`、`sender/pattern.h`、`vcam/vcam_shm.h` 的契约位置。
包头、算法、测试向量、CLI/API、SHM 布局与验收判据没有改变；`vcam_shm.h` 仅更新镜像文件位置注释。
历史文档保留当时路径；运行证据、release、staging、prev 均未整理或删除。

## 构建兼容性

- 发收端依旧产出 `build/Release/vc-{sender,receiver}.exe`，并复制到 `build/vc-{sender,receiver}.exe`。
- vcam、monitor、monitor-diag、testwin 仍在 `build/Release/`。
- vcam 验收工具仍在 `build/vcam_verify/Release/`。
- 根 CMake 编排所有组件；使用自定义 `-B` 时各产物跟随该构建目录。
- 原有 Python / PowerShell 运维入口保持原位，避免破坏 agent `/exec` 白名单与部署脚本。

## 验证（2026-10-29，本机）

1. `python scripts/verify.py`：**ALL PASS**；7 个原生目标构建成功，发收端 selftest、Python 测试源与 20 个跨语言向量对账通过；pytest **28 passed, 3 skipped**。跳过项为两个未指定网卡的 live 用例与默认禁用的 T3。
2. 清除当前命令进程中的 `NPCAP_SDK` 后，运行 `cmake -S . -B build/structure-clean` 与 `cmake --build build/structure-clean --config Release`：**PASS**；成功使用仓库 vendored SDK，7 个目标均输出到该全新构建目录，排除旧缓存与写死默认输出路径的影响。
3. 构建仍报告验收工具中 `wchar_t` → `char` 的 C4244 警告，位置 `tools/vcam_verify/vcam_verify.cpp:58`；对应源码原样迁移，未在此次结构整理中改动字符转换逻辑。
4. 完整性核对：共用头文件与原实现一致；迁移工具除路径注释外代码一致；两套构建的 7 个目标产物齐全、发收端根目录兼容副本与 Release 文件逐字节一致；已有 GUI 文件和 PLAN GUI 段落保留，暂存区仍为空。

完整命令输出：`run/structure-20261029/verify.log`、`run/structure-20261029/clean-build.log`；完整性核对：`run/structure-20261029/integrity.log`（均为 gitignore，按运行证据规则保留）。
未执行双机性能测试、发布、OTA 或 DLL 注册；这些操作不是目录整理的验收结果。

## 并发修改保护

开始时已有 GUI 修改：`agent/vc-agent.py`、skill `references/agent-api.md`、`ctrl/tests/test_gui.py`、
`docs/proposals/p5-gui.md`、`docs/runbooks/gui.md`、`PLAN.md`。前五个文件通过 SHA-256 核对保持原样；
PLAN 仅定点同步结构路径与新增本次完成记录，保留已有 GUI 契约与 D16 决策。
初始清单与 PLAN 副本保留在 `run/structure-20261029/`。未暂存或提交任何文件。
