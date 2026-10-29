# 2026-10-29 统一编译与目录打包

按 owner 要求，用一个脚本完成编译并将所有运行交付物集中到一个目录。
owner 已明确同意本次包含当前工作区全部功能（含尚未提交的 GUI）；没有提交 Git 或执行 OTA。

## 入口与成品

```powershell
python scripts/package.py --allow-dirty
```

干净工作区可省略 `--allow-dirty`；`--output` 可指定其他成品目录。相对路径按仓库根解析。
默认成品：`dist/VirtualCapture/`；本次保留 `version.json` 的 `0.4.1`，清单以 `dirty: true` 标识工作区快照。
版本发布号没有另作决策。

成品包含冻结 agent / GUI、7 个原生目标、8 个 MSVC runtime DLL、启动入口、顶层运行脚本、Python 控制工具、
Markdown 文档与清单，共 **80 个文件（含 manifest），12,472,767 字节**。
无需将散落在 `build/Release/`、`build/vcam_verify/Release/`、PyInstaller 目录中的文件手动拼包。
目标机器的 Npcap 驱动安装仍是前置；可选 Python 控制工具依赖 Python，TUI 另需 rich。

实现入口：`scripts/package.py`、`cmake/Install.cmake`、`packaging/vc-agent.spec`。
程序路径辅助：`ctrl/runtime_paths.py`、`scripts/runtime-paths.ps1`，同时兼容仓库构建、OTA build 与便携目录。
使用说明：`docs/runbooks/build.md`、`docs/runbooks/package-quickstart.md`。

## 验证证据

从仓库外 `C:\Users\s1973` 调用：

```powershell
python D:/Project/REA/VirtualCapture/scripts/package.py --allow-dirty
```

结果：

- `scripts/verify.py` **ALL PASS**；7 个原生目标构建成功，发收端自检、Python 对账通过；pytest **42 passed, 3 skipped**。3 个跳过为两个未指定网卡的 live 用例和默认禁用的 T3。
- 新增 14 项打包回归检查：输出目录保护、非受管目录拒绝覆盖、文件过滤、缺失文件拒绝、清单哈希、旧包归档、发布失败回滚、便携/OTA 二进制定位。
- PyInstaller 冻结成功；将二进制与运行库复制到临时目录后，发收端自检、agent `--help`、`/health`、`/ui`、`/vcam/status` 文件定位均通过。
- 发现无 BOM 中文脚本在 Windows PowerShell 5.1 下解析失败；成品统一为 UTF-8 BOM / CRLF 后，全部 `.ps1` 解析与便携目录程序查找通过。没有执行安装、网卡修改或虚拟卡注册。
- 重复实际打包成功，上一份成品整体归档到 `dist/VirtualCapture.archive/VirtualCapture-20261029-101301-7769c9c0/`。
- 最终目录与清单严格一致，79 个载荷文件逐项 SHA-256 核对通过；没有机器配置、run/staging/prev、编译目录或测试缓存；临时测试 agent 已退出。

完整日志：`build/package/package-20261029-101124.log`。
最终完整性核对：`build/package/final-integrity.log`。
成品 manifest SHA-256：`2f194c7db86cc2fa03f6e325d6b9132e4f26a207b4573821ba50f5fac051964c`。
这些产物和日志均由 gitignore 排除；本记录在成品验证后写入，不修改已经验过哈希的目录。

本次是本机编译、打包和隔离冒烟验证，未做新的双机性能测试；完整目录也不等同于 OTA 白名单 zip。
