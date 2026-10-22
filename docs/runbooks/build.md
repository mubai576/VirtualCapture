# Build 可重复构建

## 工具链
cmake 4.4.4
MSVC 14.44.35207
Windows SDK 10.0.26100.0
VS 生成器 Visual Studio 17 2022
python 3.14.7
PyInstaller 6.22.3
Npcap SDK fallback thirdparty/npcap-sdk

## 从零构建
cmake -S . -B build
cmake --build build --config Release
NPCAP_SDK 未设时自动回退 不依赖环境变量

## 自测
一键：`python scripts/verify.py`（build + 双 selftest + pattern_ref + pytest；`--no-build` 跳过编译，`--live Loopback` 加跑回环 live）。
产物在 build/Release/（VS 生成器）；release 打包取该目录的 exe。
build/vc-sender.exe --selftest PASS
build/vc-receiver.exe --selftest PASS
P1-1 验收 exit 2 列卡已验证

## agent 打包
python -m PyInstaller --onefile --name vc-agent agent/vc-agent.py
产物 dist/vc-agent.exe

## release zip
含 vc-agent.exe vc-sender.exe vc-receiver.exe version.json scripts ps1
用 Compress-Archive 打 release/vc-<ver>.zip（当前 0.4.0）

## run 治理策略
保留所有 report json
保留被报告引用的 session 文件
保留最近 5 轮 其余进 run/archive.d
本次 310 进 59 出 归档含活网验证报告
## 仓库治理
pkg034x pkg035 bak 移到 archive
scripts py 移到 scripts/analysis 保留不删
archive gitignore 不入库
