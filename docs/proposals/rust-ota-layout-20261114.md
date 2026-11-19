# Rust OTA 与运行布局（D27，2026-11-14）

owner要求OTA最高优先级，同时重整目录、继续后端Rust迁移。旧C++/Python、现用部署与历史包保留；Rust控制端继续只运行Raw。
本轮按程序版本、可写状态、更新事务三个用途分离，不移动既有live `staging/`、`prev/` 或运行证据。

## 布局

```text
ROOT/                         固定运行根，升级不变
  VirtualCapture.exe         主机图形入口，直接按current找程序，无CMD/PowerShell
  Start-Host.cmd              旧版兼容诊断入口
  Start-Capture.cmd           被采集端CLI入口
  scripts/                   采集/兼容启动脚本
  app/
    current.json             原子发布的active build指针
    releases/<build_id>/     整个不可变运行版本，含Rust/原生exe、DLL、清单与许可
  data/
    vc-rust-config.json      用户配置
    update-token.json        本机更新令牌，只保护更新接口
    update-peer-token.json   主机保存的采集端配对令牌
    run/                     会话stats/hashlog/日志
  updates/
    incoming/                上传中的临时文件
    staged/<build_id>/       已校验待应用载荷
    history.jsonl            事务与回滚记录
    worker/                  updater副本、job与就绪回执
```

无current指针时采用legacy扁平布局，兼容已有根配置 `ROOT/vc-rust-config.json` 与ROOT/run。
首次成功应用更新后进入managed布局，只复制配置与令牌到data，旧文件和运行证据不移动；
首次升级失败移除指针即可回到legacy。启动路径和运行根分开，版本切换不要求移动正在使用的DLL。
同名配置有冲突时不通过删除文件消除；来源与布局必须明确。
0.2.1增加主机图形入口；0.2.0经OTA升级时，新host在首帧前将缺少的`VirtualCapture.exe`复制到固定ROOT，
已有入口保留。入口只选择当前版本，不驻留、不启动shell；日志与PID回执放入run。

开发源码继续在 `rust/crates/`，发布脚本放 `rust/packaging/`，验证脚本放 `rust/tests/`；
编译产物 `rust/target/`、构建验证 `build/rust-*`、会话证据 `run/`、唯一交付 `dist/` 均被忽略。
旧项目目录暂不重排，避免一次迁移同时破坏数据面与回滚路径。

## 更新流程

1. 一次配对：采集端显式运行 `vc-capture --print-update-token --root <ROOT>` 获取令牌，填入主机更新面板。
   令牌不在health、日志、包或普通配置中输出。此令牌只用于 `/update/*`，不把现有控制API改成通用远控。
2. 主机选择Rust ZIP；本机先校验清单，再发送原始字节与SHA-256；采集端校验路径、大小、数量和逐文件哈希后暂存。
3. 停止采集后应用指定build；后台创建独立helper任务，优雅退出，helper等待原PID结束后切current指针。
4. 新后台以Rust身份、同ROOT和目标build健康响应确认就绪；主机客户端以首帧后的pid/root/build回执确认。
5. 就绪失败恢复旧指针并重启、确认旧版本；恢复失败单独记录，不将其报告为已回滚。保留载荷、历史与配置，重试不覆盖已完成证据。

上传暂存不等于激活成功。UI分别显示本机/采集端active和staged，只有就绪证据确认后才报告应用完成。
更新期间不运行采集；待清理状态必须先完成回收。旧Rust程序没有OTA入口，首次启用该通道仍需要一次bootstrap；
之后同ROOT使用更新面板迭代，不再重复复制整包。新Rust包不能上传到旧Python `/update`。

## 契约与验证

端点、大小上限、Rust函数与兼容规则以 [Rust控制契约](../../.agents/skills/virtualcapture/references/rust-control.md) 为准。
验证使用随机端口与独立ROOT，包含错误令牌、篡改清单/载荷、路径穿越、运行中拒绝激活、重启就绪与失败回滚。
实际PC2与现用虚拟卡不会被隔离测试接管；本机重启验证不代替双机、DDA/WGC或长稳验收。
