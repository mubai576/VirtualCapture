# P4-4 PC2 出口验收 runbook（不含推理端，PLAN D8）

验收判据（PLAN P4-4 卡）：通用 DShow 客户端枚举到设备并协商 NV12 640x640@240；
出口帧与发端逐字节一致（hash 对账）；FIFO 堆积为 0。

## 前置
- PC2 agent 已换代到含 `--shm` 透传 + `vc-vcam.dll` OTA 白名单的版本（见 agent-api.md）；
  发版号由 owner 定，zip 内必须含 `vc-vcam.dll`（见 docs/runbooks/build.md）。
- PC1 发端正常（motion，注明 pattern/batch/adapter；性能结论只认 motion）。

## 步骤
1. OTA 推包 → `/activate` → 轮询 `/version` 确认翻转（参考 scripts/update.ps1）。
2. PC2 注册虚拟卡（与稍后跑 DShow 客户端**同一用户**执行，HKCU 注册）：
   `regsvr32 <ROOT>\vc-vcam.dll`（注销用 `/u`）。
3. 枚举确认：GraphStudioNext/GraphEdit 能看到 "VirtualCapture 640x640 NV12"；
   pin 协商确认为 NV12 640x640、AvgTimePerFrame=41666。
   注意：本机 WinGet 版 ffmpeg 打不开虚拟机（对 OBS 同样失败，client 侧 quirk），
   **不用该 ffmpeg 版本判流**；用 GraphStudioNext 或 SampleGrabber 式 client。
4. 经 agent 起 PC2 receiver：`/receiver/start {"adapter": "<2.5G子串>", "shm": "Local\VirtualCaptureVcamFrame", ...}`；
   PC1 起 sender（双机 T1 参数：`--batch 69 --dst-mac <PC2-MAC>` 单播）。
5. 客户端开卡收流一段时间，逐帧记 FNV-1a64（可复用本机验证过的 SampleGrabber 思路）；
   同时按 AGENTS.md 硬规则立刻拉回 PC2 receiver hashlog（`/files` 分页）。
6. 对账（集合口径，(frame_id 通道不经过 DShow)）：
   outlet_hash_set ⊆ sender_hash_set 且无外来 hash；发端覆盖的每一帧都至少被出口交付一次
   （读端 240Hz 快于写端，天然多采样；torn-read 会以 motion 帧形式显式出现，见 bringup-20261024-vcam-outlet）。
   参考命令形如：`python -c "import json; s={...sender...}; o={...outlet...}; assert o<=s and not (o-s)"`。
7. FIFO 检查：receiver stats `dropped==0` 且 `trailing==0`（`complete==sent`、`hash_mismatch==0` 同 T1 判据）。

## 通过标准
- 枚举+协商一次性通过；对账零外来、覆盖完整；FIFO 零堆积；全程 stats/hashlog 按 AGENTS.md 归档。
- 任一失败：先按 test-matrix 失败分诊查链路/MTU/版本错位，再怀疑 vcam（先复现本机回环对照）。
