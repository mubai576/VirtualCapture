# PC2 割接 runbook：0.3.7 → 0.3.8（并存验证，先立新后杀旧）

执行人：在 PC2 旁的人（需管理员 pwsh）。控制机（PC1）配合验证每一步门禁，不替杀进程。
前置：release/vc-0.3.8.zip 已拷到 PC2 并解压（如桌面 vc038-pkg）；包内 11 文件、version.json 0.3.8。

## Step 0 侦察（不动手）

管理员 pwsh 执行，输出贴给控制机：
schtasks /query /tn vc-agent；schtasks /query /tn vc-agent-watchdog
Get-NetTCPConnection -LocalPort 18080 -State Listen（记 OwningProcess，即老 PID）
确认老 exe 目录（PLAN 记载 Documents\VirtualCapture，核实）与 18081 空闲。
门禁：老 0.3.7 存活、无会话在跑（/health running:false），18081 无监听。
回滚：无任何改动。

## Step 1 并存安装 0.3.8（解压目录下执行）

pwsh -ExecutionPolicy Bypass -File install-pc2.ps1 -Role receiver -Port 18081 -InstallDir C:\vc38 -TaskName vc-agent-38
门禁（控制机查）：GET 192.168.2.59:18081/health 报 0.3.8 且 updater ps-ok；18080 仍是 0.3.7。
回滚：删任务 vc-agent-38（及 watchdog）、删防火墙同名规则、删 C:\vc38，老端零触碰。

## Step 2 验证新实例

PC2 本机：C:\vc38\vc-receiver.exe --selftest（内存自检，PASS 即行）；可选回环小流量（--adapter Loopback --batch 1）。
门禁：selftest PASS；控制机 /adapters 可见网卡（只读，不拉会话）。
回滚：同 Step 1。

## Step 3 先禁旧任务，再杀旧进程

顺序不可反：先禁 `vc-agent` 与 `vc-agent-watchdog` 计划任务（若存在；schtasks /Change /TN <名> /DISABLE），再按 18080 端口持有者精确 PID taskkill /F。
门禁（控制机查）：18080 无监听；18081 的 0.3.8 不受影响。
回滚：老文件全在原目录未动，手工重起老 exe 即回（端口 18080 空着）。

## Step 4 热更新实证（控制机执行，PC2 不用人）

PC1 执行：python ctrl/run_remote.py --agent http://192.168.2.59:18081 --update release/vc-0.3.8.zip --force
同版本重推：stage 不重启、activate 原地 execv 接管、轮询确认 version 仍 0.3.8 且 PID 已换、单监听。
门禁：命令 exit 0；不做 rollback（新装 prev 为空，属预期行为，本地已验证）。
回滚：不适用（版本未变；真有问题回 Step 3 的回滚）。

## Step 5 还原端口到 18080

PC2：按 18081 端口持有者精确 PID 停新实例；删临时任务与临时防火墙规则；重跑安装脚本收尾：
pwsh -ExecutionPolicy Bypass -File install-pc2.ps1 -Role receiver -Port 18080 -InstallDir C:\vc38 -TaskName vc-agent
门禁（控制机查）：18080 报 0.3.8 单监听；18081 无监听。老 Documents 目录保留到你点头再删。
终态：C:\vc38 为 live 目录；C:\vc 从未被使用；旧目录原样保留可回滚。
