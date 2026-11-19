# 历史：PC2 网卡调优（2026-10-23）

> 仅保留当时的操作、回滚参数与测量记录，不是当前Rust部署步骤或本轮性能结论。下文设备名/角色/旧脚本仅在当时有效；现版本默认只读NlMtu，不执行调优。任何物理网卡或PC2操作均须单独授权。

> 操作人：AI 会话，2026-10-23  
> 验证：batch=69 DDA 双机实测，60s soak

## 背景

P4-2 延迟实测发现 rseg_max 偶发 >1000ms 尖刺，rseg_p99 约 8.3ms，全程 p99 约 12ms。
根因：PC2 I226-V 默认开启 FlowControl，接收突发时发 PAUSE 帧暂停整条链路；ITR 节流延迟中断响应。

## 调优步骤（PC2 本机执行，需管理员）

```powershell
$nic = '以太网 3'   # I226-V；先用 Get-NetAdapter 确认

# 1. 关闭流控（关键：消除 PAUSE 帧尖刺）
Set-NetAdapterAdvancedProperty -Name $nic -RegistryKeyword '*FlowControl' -RegistryValue 0

# 2. 关闭中断节流（降低包延迟，更快通知 CPU）
Set-NetAdapterAdvancedProperty -Name $nic -RegistryKeyword 'ITR' -RegistryValue 0

# 3. 确认 EEE 和中断合并已关
Set-NetAdapterAdvancedProperty -Name $nic -RegistryKeyword '*EEE' -RegistryValue 0
Set-NetAdapterAdvancedProperty -Name $nic -RegistryKeyword '*InterruptModeration' -RegistryValue 0

# 验证
Get-NetAdapterAdvancedProperty -Name $nic |
  Where-Object { $_.RegistryKeyword -in ('*FlowControl','ITR','*EEE','*InterruptModeration') } |
  Select-Object RegistryKeyword, RegistryValue, DisplayValue
```

## 回滚

```powershell
$nic = '以太网 3'
Set-NetAdapterAdvancedProperty -Name $nic -RegistryKeyword '*FlowControl' -RegistryValue 3  # Rx+Tx
Set-NetAdapterAdvancedProperty -Name $nic -RegistryKeyword 'ITR'          -RegistryValue 64
```

## 效果（batch=69，DDA 双机，2026-10-23 实测）

| 指标 | 优化前 | 优化后 | 变化 |
|---|---|---|---|
| fps | 196 | 207 | +5% |
| pc1_seg_p99 | 3.71ms | 3.14ms | -15% |
| rseg_avg | 4.70ms | 3.93ms | -16% |
| rseg_p99 | 8.28ms | 6.70ms | **-19%** |
| rseg_max | 1015ms | 35ms | **-97%** |
| 全程 p99 | 11.99ms | 9.83ms | -18% |
| hash_mismatch | 0 | 0 | — |

rseg_max 从偶发 ~1s 降到 ~35ms，是 FlowControl 关闭的直接效果。

## 注意

- 设置在重启后持久（注册表），无需每次配置。
- Jumbo Packet (9014) 和 EEE 在部署阶段已配置（scripts/set_nic.ps1），无需重复。
- 如需对 PC1 做同等配置，适配器名称为 Realtek PCIe 2.5GbE，关键字相同。
