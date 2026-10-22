# PLAN：分派给其他 AI 的实现计划（Phase 2：止血→双机放行）

> 状态基线：T-01~T-05 代码 DONE（2026-10-21，见§6）；但实跑复核发现调用链止血问题，
> 29 份 T0 FAIL 报告堆积（根因见§7）。本 Phase 方向见§8，新任务卡 T-07~T-10 见§9。
> 历史任务卡 T-01~T-06 保留原样作为契约 archaeology，不要改不要删。

> 你（owner）只做：按 §1 装好工具链 → 把 §3 任务卡逐条粘给其他 AI → 收货按验收跑命令 → 双机联调。
> 所有任务卡已写成可直接粘贴的 prompt，自带 skill 入口与验收标准。

## 0. 本机现状（已探明，2026-10-21）

| 项 | 状态 |
|---|---|
| cmake 4.4.4 | ✅ 有 |
| python 3.14.7 | ✅ 有 |
| C++ 编译器(cl) | ✅ 有：MSVC 19.44 + Windows SDK预装（BuildTools，cl不在PATH，cmake VS生成器直连） |
| Npcap 运行时 | ✅ 已装（驱动在 `C:\Program Files\Npcap`） |
| Npcap SDK(`pcap.h` / `wpcap.lib`) | ✅ 已vendor到 thirdparty/npcap-sdk，NPCAP_SDK用户环境变量已设（C++侧LoadLibrary动态加载wpcap.dll，无需链接wpcap.lib） |
| 双机/网线/控制面 IP | 同网口方案（2026-10-21）：PC1 以太网 3 = 2.5G，IP 192.168.2.100；PC2 = 192.168.2.59；直连线待现场插。0x88B5 与 TCP 同卡共存，不去 IP；Jumbo 9014 + 关 EEE/中断合并待做（见 §11）。 |

## 1. P0 你亲自动手（前置，约30分钟）

1. 安装 VS Build Tools（勾 C++ 生成工具 + Windows SDK），验证 `cl` 可用。 ✅ DONE：本机预装 MSVC 19.44（见§0）。
2. 下 Npcap SDK 解到 `C:\npcap-sdk`，设 `NPCAP_SDK=C:\npcap-sdk`（含 `Include/` `Lib/x64/`）。 DONE 2026-10-21: SDK vendored project-local + NPCAP_SDK user env persisted (C-drive write blocked, equivalent path).
3. 控制面 IP 已填：PC1=192.168.2.100，PC2=192.168.2.59（§11 同网口方案，直连线待插），可派 T-06（等 PC2 现场恢复，见 §11）。

## 2. 冻结契约（所有任务共用，AI 不得擅改，改即全线返工）

### 2.1 构建

- `sender/` `receiver/` 各一个 `CMakeLists.txt`，根 `CMakeLists.txt` 统一编排，
  MSVC 优先，`find` Npcap 经 `$ENV{NPCAP_SDK}`（`Include`+`Lib/x64/wpcap.lib`）。
- 产物：`build/vc-sender.exe` `build/vc-receiver.exe`。

### 2.2 CLI（动词级冻结）

```text
vc-sender.exe --adapter <npcap名子串> --mode pattern|file --pattern motion --fps 240
  --frames 0(无限) --ethertype 0x88B5 --mtu 9000 --pacing-us 0 --dst-mac ff:ff:ff:ff:ff:ff
  --session <id> --stats <path> --hashlog <path> [--file <nv12> --loop] [--list-adapters] [--selftest]
vc-receiver.exe --adapter <npcap名子串> --ethertype 0x88B5 --mtu 9000
  --session <id> --stats <path> --hashlog <path> --timeout-s 0 [--expect-frames N] [--selftest]
```

- 分片公式：`payload = mtu - 42`（14 eth + 28 头），`total = ceil(614400/payload)`。
- `--selftest` 无网卡可跑（见各任务验收），退出码 0/非0。
- `--list-adapters` 列 Npcap 网卡名后退出（给 T-06 选卡用）。

### 2.3 Agent↔C++ 对接（文件型 IPC，不搞管道/服务）

- C++ 进程每 ~500ms 原子覆写 `--stats`（写 tmp 后 rename），每完成一帧 append 一行 `--hashlog`。
- agent 轮询读这两个文件，原样供给 `GET /sender|receiver/stats`（字段见 skill `references/agent-api.md`）。
- `sender.hash.jsonl` 行：`{"frame_id":N,"hash":"<16hex>","qpc_ns":T}`；
  `receiver.hash.jsonl` 行：`{"frame_id":N,"hash":"<16hex>"}`。
- 哈希算法 **FNV-1a64**（初值 `14695981039346656037`，prime `1099511628211`，逐字节，
  输出小写 16 hex），对整帧 NV12（Y+UV 共 614400B）发送前/重组后各算一次。
  ctrl 对账即逐行比 `hash`；Python 参考实现由 T-04 在 `ctrl/` 内提供（与 C++ 向量互锁）。

### 2.4 目录落点

```text
build/  (gitignore，产物)
run/    (gitignore，stats+hashlog+报告，agent/ctrl 默认读写这里)
```

## 3. 任务卡（逐条粘给其他 AI）

---

### T-01 sender C++（可与 T-02 并行，P1）

> ✅ DONE 2026-10-21：build/vc-sender.exe --selftest PASS；--list-adapters 列出 13 块网卡；T0 回环发端 240.1fps/16563pps/1.185Gbps。

```text
先读 .agents/skills/virtualcapture/SKILL.md 及其 references（wire-protocol 必读），
再读 proto/vc_wire.h、sender/pattern.h、PLAN.md §2。

任务：在 sender/ 实现 vc-sender.exe（C++17，CMake，Npcap wpcap API）。
链路：pattern.h 生成NV12（默认motion）→ FNV-1a64整帧哈希（PLAN §2.3）→ 按
payload=mtu-42分片 → 以太网裸帧(dst=--dst-mac, type=--ethertype)→ pcap_sendpacket
按fps做QPC节流，包间隔--pacing-us，stats/hashlog按PLAN §2.3落盘。
CLI 严格按 PLAN §2.2。--list-adapters 列卡退出。
--selftest（无网卡可跑）：五档pattern各生成4帧，断言确定性（同fid双跑hash等）
与分片重组往返一致（本地拼包验证总数与尾包payload_len），通过打PASS退出码0。
file模式：--file读裸NV12循环回放（ssd跟不上240Hz则允许降fps但stats如实报）。

验收（本机，有Npcap无 compiler 除外均可跑）：
  build/vc-sender.exe --selftest            # PASS, exit 0
  build/vc-sender.exe --list-adapters       # 列出网卡
禁止：改包头尺寸/字段；用浮点生成像素；私自加压缩。
```

### T-02 receiver C++（可与 T-01 并行，P1）

> ✅ DONE 2026-10-21：build/vc-receiver.exe --selftest PASS（complete=3/dropped=1/recv=276/lost=2/mismatch=1，符合设计期望）；T0 回环在线校验 hash_mismatch=0。

```text
先读 .agents/skills/virtualcapture/SKILL.md 及其 references（wire-protocol 必读），
再读 proto/vc_wire.h、PLAN.md §2。

任务：在 receiver/ 实现 vc-receiver.exe（C++17，CMake，Npcap wpcap API）。
链路：pcap_open（snaplen≥9014，promiscuous，buffer≥128MB）→ 按ethertype+magic过滤
→ 按frame_id重组（保留最近8帧，缺包的帧在新帧到达时判dropped）→ 整帧FNV-1a64
（PLAN §2.3）→ hashlog/stats按PLAN §2.3落盘（含complete/dropped/packets_recv/
packets_lost/pps/bps/gaps尾/trailing、inter_arrival avg/p99最近1200帧环）。
CLI 严格按 PLAN §2.2。整帧号空洞（completed帧号跳跃）同样计dropped并记gaps。
--selftest（无网卡可跑）：内存构造3帧69包并注入丢包/乱序/坏CRC，
断言 complete/dropped/hash_mismatch 与预期一致，通过打PASS退出码0。

验收：
  build/vc-receiver.exe --selftest          # PASS, exit 0
禁止：改包头；跨帧拼包；丢包帧参与hash（缺包帧直接丢，不记hash）。
```

### T-03 agent 真数据对接（依赖 T-01/T-02 的 CLI 落地，P2；可与 T-04 并行）

> ✅ DONE 2026-10-21：agent/vc-agent.py 单文件零依赖重写（按§2.2 组装命令行、文件型 IPC、优雅停机）；6s 回环全程 200 且 run/ 文件齐全。 0.2.0 新增 POST /exec + GET /exec/log（白名单远程执行，供自动化）。 0.3.0 落实 POST /update 真通道（zip 原子替换+自重启+rollback，helper 改 VBS）。

```text
先读 .agents/skills/virtualcapture/SKILL.md、references/agent-api.md、PLAN.md §2.3。

任务：升级 agent/vc-agent.py（保持单文件、零第三方依赖、接口不变）：
1. /sender|receiver/start：按PLAN §2.2组装命令行拉起对应exe（cwd=项目根，
   stats/hashlog落 run/<session>.sender|receiver.stats.json + .hash.jsonl），
   记录pid；幂等（先杀旧进程）；返回session_id。
2. /stop：杀进程（先优雅超时2s再强杀），保留文件供对账。
3. /stats：读stats文件原样返回（加 running/uptime）；文件缺失返回 {running:false}。
4. /update：zip原子替换（staging→rename）+ 重启agent自身，返回新version；
   支持 {rollback:true}。
5. /version 从 version.json 读（没有则 "dev"）。

验收（用T-01/T-02的 --selftest exe 也能跑通流程）：
  python agent/vc-agent.py --role sender --port 18081 &
  python ctrl/vc-ctrl.py --sender http://127.0.0.1:18081 --receiver http://127.0.0.1:18081 --seconds 6
  # 全程200，stop后 run/ 下有 stats+hashlog 文件
禁止：改任何端点路径/字段名；引入pip依赖。
```

### T-04 pytest 矩阵 + 对账报告（依赖 skill+PLAN 契约即可开工，P2；真数来了即从 stub 切实测，可与 T-03 并行）

> ✅ DONE 2026-10-21：fnv1a 向量 + test_matrix 6 passed 1 skipped + reconcile；C++/Python 小帧互锁 20/20 相符。

```text
先读 .agents/skills/virtualcapture/SKILL.md、references/test-matrix.md、
references/agent-api.md、ctrl/vc-ctrl.py、ctrl/pattern_ref.py、PLAN.md §2.3。

任务：
1. ctrl/fnv1a.py：FNV-1a64 Python实现 + 自带测试向量（空串/单字节已知值断言）。
2. ctrl/tests/test_matrix.py（pytest）：复用vc-ctrl的get/post，按T0→T1→T2→T4→T5
   编排（T3默认跳过，加 --run-t3 才跑），每用例：比双端/version→arm receiver→
   start sender→轮询→stop→拉stats+hashlog→断言（阈值见test-matrix.md）→ 写
   run/report-<用例>-<时间>.json（含双端version/session/stats摘要/判分）。
3. ctrl/reconcile.py：对指定session比 sender.hash.jsonl vs receiver.hash.jsonl，
   输出 mismatch帧列表（frame_id对照，缺帧单独列）。
4. pattern_ref.py 追加：rings/checker/motion小尺寸实现 + FNV-1a64 over小帧，
   与T-01的C++ --selftest向量互锁（同一fid小帧hash相等，T-01提供向量文件）。

验收：
  python ctrl/pattern_ref.py && pytest ctrl/tests -q    # stub下至少T0结构跑通（判分逻辑对假数据应fail而非crash）
禁止：放宽test-matrix阈值（只能按下限判，不许改判据）；绕过/version比对。
```

### T-05 scripts 三件套（无依赖，现在就能派，P1.5）

> ✅ DONE 2026-10-21：check_mtu/update/run_T1 三件套；-WhatIf 干跑无错；check_mtu 本机实测（用 pwsh 跑）。

```text
先读 部署方案.md、PLAN.md §1。
任务：在 scripts/ 写三个powershell脚本（注释写清每步在干什么）：
1. check_mtu.ps1 -Param(数据网卡名)：打印高级属性（Jumbo/速率/EEE/中断合并），
   临时配169.254.x.x做 ping -f -l 8972 通断粗验，验完删IP还原无IP状态。
2. update.ps1 -Param(版本zip, senderIP, receiverIP)：调双端POST /update推包，
   轮询/version一致才退出码0。
3. run_T1.ps1 -Param(senderIP, receiverIP)：调vc-ctrl跑60s T1并回显判分。
验收：-WhatIf/干跑不报错；check_mtu在本机能打印网卡属性。
禁止：在数据网卡上留IP（check_mtu必须还原）。
```

### T-06 双机联调（依赖 T-01~T-04 完成 + 你的硬件，P3，最后派）

> ⏳ OPEN：等 PC2 硬件。本机预研见 docs/bringup-2026-10-21-p0.md；T0 回环已在本机 PASS。

```text
先读 部署方案.md §3-§7、references/test-matrix.md。
前置（owner已做）：双机数据网卡按部署方案§3配好并截图；控制面IP如下：
  PC1控制面=192.168.2.100  PC2控制面=192.168.2.59  数据网卡名=Controller（双机实测：本机 I226-V 与 PC2 Realtek 各唯一命中）。
任务：出联调执行记录（docs/bringup-<日期>.md）：--list-adapters选卡→check_mtu→
推版本→T0→T1五档→T2 soak→T4 pacing→汇总stats/红线偏差表；任一fail按test-matrix
分诊定位到（网卡/生成瓶颈/抓包丢/版本错位）四类之一并给下一步。
验收：bringup文档 + run/报告齐全，T1 motion判绿。
```

## 4. 派发顺序与并行度（Phase 2 修订，旧顺序作废）

```text
现在可并行：T-07（止血） × T-08（构建+git+治理） × T-09（文档同步）
随后：T-06 双机联调（要 T-07+T-08 完成 + 你的双机成立 + 控制面IP填T-06卡）
最后：T-10 压测收尾（T2/T4 可在回环先跑；T2-10min/T3/T5-dda 要双机）
```

- 合并规则：每任务只碰自己的目录（T-01→`sender/`，T-02→`receiver/`，T-03→`agent/`，
  T-04→`ctrl/`，T-05→`scripts/`；T-07 按子项分目录改，T-08 根目录+git，T-09 只碰 `*.md`+skill），
  `proto/` 仍归你锁定，AI 无权改。
- 收货：每任务按其“验收”命令跑一遍，不过打回（附完整 stdout）。
- 旧顺序（T-01×T-02×T-05 等）已完成归档，见各卡 ✅ 标记。

## 5. 全局禁止（写进每条派发语，AI 违规则打回）

1. 不改 `vc_pkt_hdr_t` 尺寸/字段顺序；加字段先找你批。
2. 像素生成禁用浮点（GPU/CPU/Python 三端 bit-exact 靠整数保证）。
3. 传输链路禁止任何压缩（无损也不行，先保证基线纯净）。
4. 不许用静态帧报性能数据；性能宣称必须注明 pattern + `motion` 成绩。
5. 跨机延迟不许用 QPC 差值汇报；只报 inter-arrival 抖动。

## 6. 验收记录（2026-10-21，T0 loopback 真机）

```text
build/vc-sender.exe --selftest            # PASS, exit 0 (VEC full+small 5x4)
build/vc-receiver.exe --selftest          # PASS, exit 0 (3/1/276/2/1 as designed)
python ctrl/pattern_ref.py                # PASS + interlock C++ 20/20 match
pytest ctrl/tests -q                      # 6 passed, 1 skipped (T3 opt-in)
vc-ctrl T0 motion 240Hzx10s adapter=Loopback
  -> sent=2400 complete=2400 dropped=0 hash_mismatch=0
  -> inter_arrival_avg=4.159ms pps~16560 bps~1.18Gbps  # PASS
vc-ctrl T1 motion/noise 6s                # PASS, exit 0
reconcile sender vs receiver hashlog      # 2900/2900 zero-mismatch
T0/T1/noise 连续多次 PASS
T1 五档回环 counter/checker/rings PASS（motion/noise 早前 PASS）；T4 pacing 0/50/200us -> 240/191/64fps 均零丢，默认 pacing 0；T3 mtu1500 回环 101k pps 零丢 PASS（反例必须真网卡重验，p99 7.1 接近红线）；T2 soak motion 240Hzx10min loopback
file 模式端到端 3 帧 PASS（sender hash 与 Python 参考逐字节一致，reconcile 3/3 零错）。soak 基线已入库 docs/baseline-T2-2026-10-21.md。: sent=144000 complete=144000 dropped=0 lost=0 mm=0 pps=16557 bps=1.185G p99=5.2, 14.4万帧全量对账零错；avg=5.12 因尾部单次~115ms 毛刺超 4.166+-0.3（回环串发篬 4，双机重验）。harness: ctrl 等发端发完定帧数再停（停止边缘丢帧消除）+ receiver timeout seconds+120（pacing 拖慢场景）。loopback 收尾偶发 ~1s 嗯声（3 次，只影响 avg 不丢帧，双机验证。；review 修复 sender file 模式停止逻辑 + /exec 错误 timeout 返 400 + 判分加 p99<8.3ms -> 0.3.1
```

T-02 selftest 中 dropped=1/验证后台控制台控制台 lost=2 为设计预期（内存注入丢/重/坏CRC），不是真实丢包。
T0 首轮曾现过 p99=20.8ms 单次毛刺（同机争用），重跑后 p99=4.8ms 判绿。

---

## 7. 状态审计（2026-10-21 实跑复核，§6 的另一面）

`run/report-T*.json` 共 32 份：T1 PASS×2、T0 PASS×1、**T0 FAIL×29**。
PASS 的 3 份全部带 `--adapter Loopback`（motion 2400 帧 p99=4.8ms / noise 1440 帧，零错）；
FAIL 的 29 份全部**没传 adapter**：sender 自动选卡选中发不出去的网卡，
`send_errors == sent_packets`（100% 全错），receiver 收包为 0，且 sender **exit=0**。

根因链（已逐段验证）：

1. `vc-ctrl.py --adapter` 默认为空，空即不透传（`if adapter:`），agent 再把空值滤掉，
   exe 走自动选卡 —— 全链没有任何一处 fail-fast，裸跑完才出 FAIL 报告。
2. sender 自动选卡失败/全错发送时 exit 仍为 0（已实测 `errors=345/345, exit=0`），错误被静默。
3. pytest 只覆盖向量/阈值/结构（全绿），**没有 live 路径用例**，29 连 FAIL 无自动拦截。
4. `run/` 下 ~200 个 session 残留文件，无保留策略；仓库**尚无 git**（只有 zips+version.json）。
5. SKILL.md（“sender/receiver 未实现”）、部署方案§9（scripts TODO）、本 PLAN §0（工具链）
   与现实脱节 —— §0 称 MSVC/NPCAP_SDK 就绪，但复核 shell 下 `cl`/NPCAP_SDK 均不可见
   （可能只存在于另一 AI 会话环境），T-08 须验证可重复构建。

结论：§6 的 DONE 有效（代码能力已证），但**调用链缺止血 + 构建可重复性未证 + 知识未同步**。
先做 T-07~T-09，再谈 T-06 双机。

## 8. 方向（Phase 2：先止血，再放行）

北极星：**双机 T1 motion 判绿**。之后 T2 soak→T4 pacing→T3 归档→T5 dda 摸底；
虚拟采集卡、独占全屏 Hook 另立项，不进本 Phase。

原则：

1. 止血优先（T-07）：任何“全错发送”“无卡可用”必须当场非零退出+人话报错，
   不许再产出“跑完才知道是空跑”的报告。
2. 可重复+可回滚（T-08）：当前环境重编一次 + `git init` + run 治理。编不出来=前面全是沙上建塔。
3. 知识同步（T-09）：skill/部署方案/PLAN 与代码一致，接手 AI 不读旧结论。
4. 双机放行（T-06）：T-07+T-08 完成后，owner 配好数据网卡（去IP/Jumbo9014/关EEE）再派。
5. 压测收尾（T-10）：T2/T4 可在回环先跑出基线；10min soak、T3、T5-dda 要双机。

## 9. 新任务卡（逐条粘给其他 AI）

---

### T-07 调用链止血（碰 sender/vc_sender.cpp、agent/vc-agent.py、ctrl/vc-ctrl.py、ctrl/tests/）

> 背景必读：PLAN §7。29 份 FAIL 全是“没传 --adapter → 自动选错卡 → 全错发送 → exit 0”。
>
> ```text
> 先读 .agents/skills/virtualcapture/SKILL.md、PLAN §2/§7。
> 三处改动，一起交付：
> 1. sender（vc_sender.cpp）：--adapter 缺省时逐卡试发（按 pcap_findalldevs 顺序，
>    首包 send 成功即锁定）；无可用卡/全部打开失败 → stderr 人话（列出试过的卡+错误）
>    exit 2；live 发送中成功包为 0（send_errors == sent_packets 且 sent_frames > 0）
>    → exit 3。--selftest/--list-adapters 行为不变。
> 2. agent（vc-agent.py）：新增 GET /adapters（调 exe --list-adapters，原样返回文本+解析后的
>    名字数组）；/sender/start 与 /receiver/start 若缺 adapter 参数 → 400 并在 message 里
>    附上本机 /adapters 列表，不许拉起空跑进程。
> 3. ctrl（vc-ctrl.py + tests/test_matrix.py）：--adapter 改为必填（缺省报错退出码 2，
>    提示先调 GET /adapters 查卡）；新增 test_T0_live_loopback：读环境变量 VC_ADAPTER
>    （未设则 pytest.skip），跑 motion 240Hz×2s 小帧数，断言 verdict PASS 且 send_errors==0；
>    新增 test_T1_live_loopback 同理（frames=480，断言 dropped==0 and hash_mismatch==0）。
> 验收：
>   build/vc-sender.exe --mode pattern --frames 5 --session probe ...（无adapter，本机回环卡可发）# 按新逻辑走试发
>   VC_ADAPTER=Loopback pytest ctrl/tests -q -k "live or vectors or determinism"  # 全绿
>   python ctrl/vc-ctrl.py --sender ... --receiver ... --seconds 5   # 不带--adapter 必须秒退码2，不产生报告
> 禁止：改包头；给 live 测试加 sleep 蒙混；放宽 test-matrix.md 判据。
> ```

### T-08 可重复构建 + git + run 治理（碰根 CMakeLists、.gitignore，不碰业务代码）

> ```text
> 先读 PLAN §0/§7.5（工具链存疑：复核 shell 无 cl、NPCAP_SDK 未设，但 build/ 有 MSVC 产物且现存 clang）。
> 任务：
> 1. 在当前环境从零重编：删 build/ 重进 cmake configure + build（clang 或 MSVC 均可，
>    如实记录用了哪条链），产物 selftest 双 PASS 才算过；把确切命令写进 docs/build.md。
> 2. 第三方链固化：thirdparty/ninja + npcap-sdk 已 vendor，要求 CMake 在 NPCAP_SDK 未设时
>    自动回退到 thirdparty/npcap-sdk（含 headers 检查 + 明确 message），不依赖任何环境变量。
> 3. git init + 首 commit（含 .gitignore：build/ run/ __pycache__/ .pytest_cache/ thirdparty/ninja-win.zip），
>    输出 `git log --oneline` 截图式文本；写 run/ 保留策略进 docs/build.md
>    （session 文件只留最近 N=5 轮 + 所有 PASS 报告，其余进 run/archive/，pytest/report 默认不落盘 session 明细）。
> 4. 执行一次治理（按新策略归档 run/），`find run -type f | wc -l` 前后对比写进交付说明。
> 验收：rm -rf build 后重编 selftest 双 PASS；git log 可见；run 文件数降到 <40。
> 禁止：提交二进制产物进 git（exe/zip/log 全部 ignore）；删 PASS 报告。
> ```

### T-09 文档/skill 同步（只碰 *.md + .agents/，不碰代码）

> ```text
> 先读 PLAN §6/§7、本次审计结论。任务：按现实修正三处过期知识：
> 1. .agents/skills/virtualcapture/SKILL.md：repo-map 去掉“未实现”字样，写明 sender/receiver/agent/pytest/scripts
>    均已落地 + T-07 新增的 /adapters 与 --adapter 必填；Quickstart 加“先 GET /adapters 查卡”。
> 2. 部署方案.md §9：scripts 三件套标 DONE（含 install-pc2 超额），TODO 只留“真双机验证”。
> 3. PLAN.md §0：按 T-08 实测结果重写工具链现状（哪条编译链可用写死命令，不要“可能有”）。
> 验收：grep -rn "未实现\|TODO.*script\|无 compiler" .agents/ 部署方案.md PLAN.md --include=*.md | grep -v 历史/审计/ archaeology → 无命中（历史段落除外）。
> 禁止：改任何阈值/契约数字；顺手改代码。
> ```

### T-10 压测收尾（依赖 T-06 双机成立；T2/T4 短时档可在回环先跑基线）

> ```text
> 先读 references/test-matrix.md、PLAN §8。任务（双机上跑，回环基线先行）：
> 1. T2 soak：motion 240Hz×10min，达 pps≈16560±2%、p99<8.3ms，基线入库（红线±收紧建议）。
> 2. T4 pacing：0/50/200us 三档对照，定默认 pacing（dropped vs CPU 表格）。
> 3. T3 mtu1500 反例跑一次归档（预期 pps≈101k 且丢帧，证伪用）。
> 4. T5 dda 摸底前置：sender 加 --mode dda 最小实现（DDA 抓无边框桌面→居中裁640→NV12，
>    无DDA环境/独占全屏直接报错退出，不许静默发黑帧；hash不判，只记 gpu_ms/pps/CPU）。
>    先交付 dda 自检（--selftest 扩展：有屏 PASS、无屏 SKIP），T5 联调记入 bringup。
> 验收：run/ 下 T2/T4/T3 报告齐全 + bringup 续写；dda 无屏机器上 --selftest 为 SKIP 不是 FAIL。
> 禁止：拿回环数据冒充双机结论（报告必须注明 adapter/双机 IP）；独占全屏 Hook（另立项，不许在本卡顺手做）。
> ```

update.ps1 真传实测 PASS（读 zip 期望版本→双 POST→轮询一致 exit 0，0.3.1→0.9.9-updtest）。update POST 加 try/catch（双推失败不中断轮询）。
0.3.2: same-version re-apply keeps older prev (true rollback target) + test; exe+zip repacked.
T-06 probe: PC2 192.168.2.59 agent 0.3.2 UP, versions match local; PC2 NICs: Realtek 2.5GbE (no IP, absent from ipconfig = likely disabled), Meta tunnel, control NIC .59; local eth3 Up 2.5G with .100 (=control uplink, cable goes somewhere live). BLOCKED on-site: enable Realtek, plug direct cable, decide control path, strip eth3 IP + Jumbo9014 + EEE off.


## 10. 更新通道事故与修复（2026-10-21，0.3.4）
现象：0.3.3 推送后 PC2（192.168.2.59）agent 未恢复，18080 持续拒绝。
根因：PC2 运行 0.3.2，其重启 helper 依赖 VBS（wscript）；PC2 的 wscript 不可用，helper 未执行而 agent 已退出，变砖。
修复（0.3.4）：helper 改 PS 数组传参（$args 保持参数边界），启动后轮询 /health、失败自动回滚。
/health 新增 updater 字段（ps-ok/ps-missing）；update.ps1 与 run_remote --update 推包前预检，非 ps-ok 直接拒推。
验证：冻结版 exe 在隔离 root 端到端走通 helper 日志 healthy，/health updater=ps-ok。
铁律：updater 非 ok 的机器一律走 install-pc2.ps1 手工通道，不许 POST /update。PC2 当前已死，必须现场手动恢复（见 §11）。
版本：release/vc-0.3.4.zip（9 文件：scripts×5 + 双 exe + version.json）。在 PC2 复活确认前，不再推任何包。

## 11. 同网口双机联调（2026-10-21 决策）
结论：数据+控制跑同一网口可行。数据面是 EtherType 0x88B5 裸帧（Npcap 直收，无需 IP），控制面是同网卡 IP 上的 TCP 18080，两者不冲突，数据网卡保留 IP。
地址：PC1 以太网 3 = 192.168.2.100，PC2 = 192.168.2.59，同网段，直连网线（待现场插）。
网卡准备（双机各做）：Jumbo 9014、关 EEE、关中断合并；先用 scripts/netinfo.ps1 截图留档。
PC2 现场恢复：1) 看 C:\vc\run\helper.log（如有）；2) 手动起 C:\vc\vc-agent.exe --role receiver --port 18080；3) 验 /health 与 /version；4) 非 0.3.4 则重跑 install-pc2.ps1（0.3.4 包）。
联调（PC1 上）：先 python ctrl/bringup.py --sender http://192.168.2.100:18080 --receiver http://192.168.2.59:18080 --adapter <卡名> --dry-run，再全量；T2 soak 可加 --skip-soak。
远控：python ctrl/run_remote.py --agent http://192.168.2.59:18080 -- <白名单命令>；推包加 --update（带预检门）；审计加 --log。/exec 白名单见 agent 源码 _resolve_argv。


## 12. 双机首飞（2026-10-21 夜，0.3.5，同网口 Controller）
链路结论：通。T0 + T1 五档全部 complete==sent、dropped=0、lost=0、hash_mismatch=0，p99 全部 <8.3ms；T0 全量对账 2400/2400 零错。
原始判分 FAIL 的唯一原因是 receiver inter_arrival_avg（T0=8.0，其余约13.4，noise=4.99 正常）：已用 2 秒采样的时间序列证明，稳态 avg 全程约 4.95，巨额 spike 只出现在发端停止后的收尾 drain（最后约 75 帧用约 2 秒排空），而终态 120 样本环几乎全由 drain 段构成。
T-04 后续：判分应看法①稳态采样（发端停止前）或②全量离线均值，终态环形 avg 不得作为门限；receiver 代码本身无 bug（已读源码确认环形逻辑）。
发端瓶颈：同机同 exe 回环 240fps 达标，双机实网只跑 190-202fps，差值在 pcap_sendpacket 发送侧背压（回环是 hypervisor 即时吸收）。T-10 材料：先定 drain/pacing 对照，再谈优化。
偶发：checker 首跑发端在 1713 帧停滞（hashlog 跨度 8.5 秒，stdout 空，疑似崩溃/卡死），重跑 14400 全过，未复现。记为 transient，T-06 留意，若再现则取 loopback 复现 + 事件日志。
现场备注：PC2 agent 实际跑在 Documents\VirtualCapture（手动启动），role 标签为 sender（纯显示，不拦功能）；建议下次用 install-pc2 落到 C:\vc 并正名为 receiver。OTA 0.3.5 文件已生效（helper.log 证实拷贝完成），重启仍需手工，OTA 重设计记 0.4.0。


## 13. T4/T3/T2 双机归档（2026-10-21 夜，0.3.5，单播 Controller）
T4 三档（motion 3600 帧）：p0/p50/p200 全部 complete==sent、dropped=0、lost=0、mm=0；p99=6.18/10.49/20.31，随包间隔物理拉长。默认 pacing=0 维持。
T3 mtu1500 真网卡：2400/2400 零丢零错，pps 28k。T-10 原预期（101k 且丢帧）未复现：发端被每包 syscall 开销压到 69fps，pps 起不来所以不丢。结论修正为慢而不丢；要快而丢的证伪需先解决发端包速率。
T2 soak（单播）：144000/144000，dropped=0、lost=0、mm=0，包级 9936000/9936000 精确一致，p99=5.985，稳态 avg 约 5.0。终态 avg=13.161 仍是 §12 的 drain 环 artifact，不判。
广播楔子定论：广播 soak 在 70059 帧（约 350 秒）发端 hung（零错、进程活、stdout 空），单播一口气跑完 144000。结论：交换机广播风暴抑制。联调默认发单播（--dst-mac 对端 MAC，vc-ctrl 已支持，冒号格式）。直连线到位后可重测广播。


## 14. T-04 判分跟进：只加指标、不动门限（vc-ctrl，已测）
report 新增 steady_state（发端停止前最后一采：发端帧数/收端 complete/avg/p99）与 sender_cadence（本地发端 hashlog qpc 跨度真 cadence）；verdict_t1 门限一字未动（单测锁定：双机 13.5avg 照样 FAIL）。
pytest 15/15（roundtrip 两兄弟因沙箱杀 detached helper 仍红，逻辑已由前台 healthy 实证）。vc-ctrl 另加 --dst-mac（冒号 MAC）。


## 15. T-06 文档与 run 治理（2026-10-21 夜）
docs/bringup-2026-10-21-dual.md：12 份双机报告编译，cadence_ms 列由发端 hashlog qpc 回填（含 pacing 物理验证：p50=8.53、p200=19.07；T3=14.51）。
run 治理：29 份中午无 adapter FAIL 报告（逐份验 verdict=FAIL，125233 PASS 保留）+179 个无引用 session 文件进 run/archive.d/。另修复一次 Move 误把报告改名为 archive 文件的事故（内容已抢救改名归档）。


## 16. T5 dda 发端最小实现（sender/dda.h，MSVC 一次编过）
DDA 抓桌面→居中裁 640→NV12，全整数 BT.601，无浮点；无输出/独占全屏/异格式/过小桌面一律显式报错退出，无静默黑帧。收端 pattern=255 自动跳过像素 verify（hash 照常对）。
自检：converter 白/红逐字节精确 PASS；DDA 采集 PASS（本机 2560x1440，全黑因锁屏/无会话画面，管线无恙）。loopback 端到端 60/60 零错。stats 增 gpu_ms/cpu_pct（运行+终态，实测 gpu 约 1.4ms/帧）。vc-ctrl 加 --mode 透传，T5 联调就绪。


## 17. 0.3.6 staged + dda 双机冒烟（2026-10-21 夜）
release/vc-0.3.6.zip 已打好（10 文件，vc-sender 为 dda 版），未推 PC2，等确认。本地 version.json 为保版本门暂留 0.3.5，随推包时一起正名。
运行时发现并修复：静态桌面 AcquireNextFrame 500ms 超时曾致命退出，现超时复用上一帧（新 fid 照常 hash/发），init 期失败仍致命。T5-dda-smoke 双机 150/150 零丢零错，gpu 约 3ms/帧；240Hz 红线不适用于 dda（10fps cadence 约 100ms），需独立门限。


## 18. skill 同步（agent-api 纠偏 + test-matrix 双机附录）
agent-api.md：/health 加 updater；/sender/start 补 adapter/dst_mac/file/loop 与 dda；stats 按真实字段；/receiver/report 按真实结构；删 stub 段；helper 改 PS 描述 + 预检门 + 同版保旧 prev；新增 /files 行。
test-matrix.md：尾部加 Dual-NIC notes（稳态判分法、200fps 天花板、广播楔子、T3 修正、dda 独立门限），判据数字一字未动。


## 19. OTA 0.4.0 design proposal (detached helper must die; AWAITS USER ACK, no code yet)
Evidence: 0.3.3 VBS brick (wscript) + 0.3.5 PS helper dead twice with first log line unwritten (sandbox + PC2 SYSTEM task both kill the tree within ~1s of parent os._exit). Foreground runs of the same helper go healthy. Conclusion: any design depending on outliving the parent is unreliable here.
Proposal A (recommended): split stage/activate. POST /update only stages (validate+extract+prev snapshot, NO restart) and returns staged version. Activation is explicit: POST /activate or install-pc2.
- script-mode agent: py_compile staged vc-agent.py, then in-process os.execv (no child, no powershell, no Bypass, no hidden window). Socket outage ~2-5s; caller polls /version (update.ps1 already does).
- frozen exe: versioned side-by-side (vc-agent-<ver>.exe, pointer in version.json) + execv to the new file (old image unlocked by exec, no overwrite race). Boot GC deletes non-current exes. Sender/receiver children are stopped before staging (fixes silent locked-exe overwrite).
- rollback = activate prev pointer (re-exec). No detached process exists anywhere in the flow, so the sandbox-kill class disappears and the roundtrip pytest becomes deterministic.
Proposal B (fallback): keep helper but launch via one-shot schtasks (survives parent, observable, still powershell). Weaker; only if A is rejected.
Needs ACK: new POST /activate endpoint; versioned exe filenames; GC semantics; updater field stays as capability report. No code until ACK.


## 20. Open items ledger (2026-10-21 night; nothing below is mine to decide alone)
1. 0.3.6 push to PC2: staged, needs user GO (receiver unchanged; install-channel recommended over OTA). Local label held at 0.3.5 meanwhile. [STATUS 2026-10-22: CLOSED as moot per S22/S34.]
2. T-06 formal close: delivery green, frozen avg gates red. Changing redlines needs user nod (T-10 recalibration). Bringup doc exists.
3. OTA 0.4.0: proposal in S19, needs ACK before any code.
4. PC2 on-site: move agent to C:\vc, role receiver, confirm direct cable, retest broadcast, watch checker-transient. [STATUS 2026-10-22: AMENDED per S33 - C:\vc move STRUCK; true remainder in S34.]


## 21. Q2/Q3/Q4 answers landed (file-mode verdict + restart-free OTA + watchdog + keep-Documents)
Q2: file mode (zero generation cost) on real NIC = 209fps vs motion 200fps (run/fileprobe.*: sender 2400f/pps14404, receiver complete 2400/dropped 0/mm 0/avg 5.156). Bottleneck is send path (~13us/pkt Npcap-NDIS-I226 round-trip), not generation. T-10 direction: batched submission (PacketSendPackets/send queue).
Q3: agent now applies exe/script-only updates LIVE with no restart (409 if sessions active); restart path stops children first (locked-exe fix) + creates missing parent dirs (fresh-layout crash fix). pytest test_update_no_restart_keeps_pid green. Watchdog task (5min probe, port+quiet-window guards) added to install-pc2, both branches tested.
0.3.7 staged (agent restart-free + watchdog + dda, 11 files), NOT pushed. Local label 0.3.7 so dual gates pause until PC2 installs 0.3.7 via install channel. 0.3.6 push moot (PC2 needs nothing from it).
Q4: single copy in Documents kept per user; C:\vc removal + task repoint is on-site manual (port-based firewall needs no change).


## 22. 0.3.7 installed on PC2 via install channel (manual); versions aligned 0.3.7/0.3.7
watchdog.ps1 verified present on PC2 (latest, with schtasks rc check). Role still sender-label per user (cosmetic only). 0.3.6 OTA push moot/superseded. Dual version gates open again.

## 23. Batch-submit comparison 1/3/23/69 + send_errors fix (2026-10-21 night)
Setup: 2400 motion frames x6 runs, mtu9000 (69 pkt/frame), fps240, pacing 0, unicast Controller, dual 0.3.7. Tags q1/q1b/q3/q3b/q23/q69. Table:
| batch | fps_tx | pps_tx | send_err | rx complete | dropped | lost | mm | cadence_ms | rx_avg | rx_p99 |
| 1 | 208.0 | 14355 | 0 | 2400/2400 | 0 | 0 | 0 | 4.807 | 13.219 | 6.048 |
| 1b | 203.4 | 14038 | 0 | 2400/2400 | 0 | 0 | 0 | 4.915 | 13.272 | 5.764 |
| 3 | 230.3 | 15889 | 0 | 2400/2400 | 0 | 0 | 0 | 4.342 | 4.336 | 5.578 |
| 3b | 239.5 | 16519 | 0 | 2400/2400 | 0 | 0 | 0 | 4.177 | 12.613 | 5.497 |
| 23 | 239.8 | 16550 | 0 | 2400/2400 | 0 | 0 | 0 | 4.169 | 12.565 | 6.130 |
| 69 | 240.0 | 16553 | 0 | 2400/2400 | 0 | 0 | 0 | 4.168 | 4.174 | 6.697 |
Read: batch1 baseline ~205fps; batch3 +12%; batch23/69 hit 240fps target (cadence 4.168ms = 1/240). Diminishing after 23 (23 pkt ~ 1/3 frame; 69 = full frame). Correctness 100% every run; q69 reconciled 2400/2400 zero mismatch via /files hash fetch.
Bug found mid-way: sendqueue_transmit success return is positive (not 0); old !=0 check faked 165600 errors on 100%-delivered b3 run. Fixed 3 sites to <0 (2 transmit + 1 sendpacket), rebuilt, selftest green, q-runs all 0 errors. Stats-only fix, wire/proto untouched.
Caveat: rx_avg bimodal (4.2 vs 13.2, avg>p99 => single ~1s stall inside receiver 120-window in q1/q1b/q23/q3b (4 of 6)). Sender hashlog gaps clean (max 6.4ms), so stall is PC2-side. Receiver hashlog carries no timestamps, exact slot unknown. Follow-up 2026-10-22: PC2 receiver stdout logs carry no per-frame timing (session/done lines only, checked s-a051af7e/s-484962f4) - stall placement is a dead-end without the hashlog-qpc change. Follow-up proposal: add qpc_ns to receiver hashlog lines (small, needs nod). q3b repeat (batch=3, 2400f) 2026-10-22: rx avg 12.613 STALLED where q3 was clean 4.336 - batch-correlation hypothesis REJECTED; sender gaps clean (max 4.9ms) so stall definitively PC2-side; 4/6 runs stalled (random per run); sender fps itself varies 230->240 at same batch (OS noise); q3b reconciled 2400/2400. Rate math: 4/6 runs stalled in ~12s windows excludes slow-period causes (5-min watchdog would hit P~0.04/run, P(>=4/6)~0.00005; 60s-period excluded at ~2%) - remaining candidates are fast/random PC2-side load (background tasks/defender) or receiver processing spikes; discriminating needs on-PC2 observation or S30 timestamps.
Housekeeping: staged release/vc-0.3.7.zip still holds pre-fix sender (stats-only diff); re-stage or not is user call. PC2 receiver binary unaffected. Helpers kept: scripts/batch_table.py, scripts/gap_any.py (superset of gapcheck, merged), scripts/fetch_all_rxhash.py (covers all five tags incl. q69; fetch_rxhash merged in, q69 re-fetch byte-identical). Run files: run/q*.sender.stats.json + .hash.jsonl, run/q*.receiver.stats.json, run/q*.receiver.hash.jsonl (all six) + re-fetched T0/T3/T4 receiver hashlogs. brn.py hardened 2026-10-22 (auto-saves receiver stats + SUMMARY line, CLI unchanged; 60-frame batch69 probe green: 60/60, 240fps).

## 24. Raw-transport design note: zero codec delay by construction (2026-10-22)
Verified by repo-wide grep (2026-10-22): no H.264/H.265, NVENC/AMF/QSV, x264/openh264, avcodec anywhere; only byte-string .decode() hits. Chain carries raw NV12 pixels end to end (640x640, hash-verified bit-exact, q69 reconcile 2400/2400).
So: no encoder/decoder exists => codec latency is structurally zero, not merely unmeasured. Real latencies present: generation/capture (pattern ~0, DDA gpu 1.4-3ms), send path (~13us/pkt single, amortized by batch), wire + reassembly (frame cadence bound, 4.168ms at 240fps).
Tradeoff paid the other way: bandwidth. 240fps/640x640/mtu9000 batch69 run measured ~1.18 Gbps. That is why the link budget demands 2.5G direct + mtu9000 (bandwidth-for-latency swap).
If resolution outgrows the link (1080p/4K), a codec must be introduced and codec delay appears for the first time: needs its own selection/rate-control/wire-format task (proto/ moves), NOT a continuation of T-10 batch work. No action now; recorded so batch numbers are never misread as containing codec cost.

## 25. pytest re-verify after sender send_errors fix (2026-10-22)
Main suite (2 roundtrip deselected): 15 passed, 1 skipped in 20.58s. Skip = T3 real-NIC (needs --run-t3, covered by dual runs). Logs: run/pytest-main.log. Binary selftests re-run 2026-10-22 on current tree: sender PASS (DDA converter exact), receiver PASS (3/1/276/2/1 vectors), pattern_ref PASS (tiers+determinism). 01:16 re-run after comment-only edit: deterministic parts PASS (converter exact, vectors identical), DDA capture FAIL (AcquireNextFrame timeout, output idle at night - environmental, selftest demands live desktop unlike sender timeout-reuse); rebuilt exe same size (85504) but different hash (MSVC timestamps; comment cannot change codegen) - functional identity via converter PASS + identical vectors, no regression. Suite impact 01:22: pytest main re-run vs rebuilt binary gives 14 passed + 1 FAILED (test_interlock_cpp_vectors, was green at 00:12): it shells exe --selftest requiring rc==0, and the idle-night DDA-capture FAIL trips it. Vectors/converter all fine - environmental red, not a regression. Design note: this is the exact case for the recorded T5 direction (dda selftest: live output PASS, no output SKIP-not-FAIL); implementing SKIP needs a nod (exe + test change). Log: run/pytest-main2.log.
Roundtrip pair: 1 passed (same_version_keeps_prev), 1 failed (roundtrip_and_rollback) in 132s. Failure is the known sandbox-kill flake, now at a new spot: update half PASSED this time (0.9.9-test serving, prev mirror + scripts/ok.ps1 asserted), rollback restart never came back in 60s (ver empty). Same detached-helper class as S19, unrelated to sender change (agent/scripts untouched). Log: run/pytest-roundtrip.log. Local agent unaffected (health ok, 0.3.7). Post-hoc forensics 2026-10-22: no helper/temp traces survive anywhere (test tmpdirs cleaned, no repo helper artifacts) - consistent with S19 tree-kill, nothing further recoverable.

## 26. 0.3.7 zip custody check + skill batch sync (2026-10-22)
Zip audit (scripts/zipdiff.py): release/vc-0.3.7.zip holds 11 files. vs current tree: vc-receiver.exe identical, version.json identical, scripts/ in-zip byte-verified identical (7/7, scripts/zipdiff.py (zipscripts merged into zipdiff; single custody script), 2026-10-22; new helpers are outside zip allowlist). vc-agent.exe timestamp-chained (source 21:58 -> frozen build 22:22 -> zip entry 22:22). agent source untouched since stage (agent/vc-agent.py 21:58 < zip 22:25). The ONLY stale file is vc-sender.exe (zip 067f4d5c.. vs built 1e3707f2.. (rebuilt 01:16 after comment-only edit: df5cc0cd.., size still 85504; MSVC embeds timestamps so hashes differ on any rebuild - functional identity via converter PASS + identical vectors), the send_errors <0 stats fix). So re-stage = swap one file, nothing else; still user call (dry-run proven on temp copy via scripts/restage_dryrun.py: 10 other entries byte-identical, sender == current build; in-place vs r1 filename TBD at nod). Also removed stale build/agent-dist/vc-agent.exe (15:32 frozen exe predating restart-free/watchdog, 10MB trap, zero references in docs/skill/scripts/PLAN; install channel uses the zip, never agent-dist). Stale 15:32 PyInstaller workdir build/agent-build/ (13.6MB, unreferenced, regenerable from build/vc-agent.spec) also removed via shutil (recursive Remove-Item is policy-blocked); fresh 22:22 build/vc-agent/ kept as zip provenance. PC2 needs nothing (receiver identical). 2026-10-22 drift check: PC2 scripts/watchdog.ps1 + version.json byte-identical to local (one-off fetch, script removed). PC2 exes live at ROOT, not build/ (install layout): vc-receiver.exe 77312B + vc-sender.exe 85504B, sizes match local byte counts. Exact-hash channel does not exist (/files text-decodes binary; /exec allowlist has no hasher); binary identity rests on size + all dual runs pattern-verify mm=0.
Skill sync per rule 6 (no thresholds touched): references/wire-protocol.md += Batch submit section (semantics, S23 numbers, positive-return gotcha, wire untouched); SKILL.md sender row points at it. test-matrix.md unchanged.

## 27. Full frame-level reconcile of all batch runs (2026-10-22)
Fetched the four remaining PC2 receiver hashlogs via /files (run/<sid>.receiver.hash.jsonl, each 2400 lines, none truncated; helper scripts/fetch_all_rxhash.py) and reconciled against local sender hashlogs. With q69 from S23, all six batch runs are now frame-exact:
q1/q1b/q3/q3b/q23/q69: sender 2400, receiver 2400, missing 0, extra 0, hash_mismatch 0.
Batch-submit correctness is therefore proven at bit level for every档, not only via receiver pattern-verify (mm=0) but via independent sender-vs-receiver hash join. Run files: run/q*.receiver.hash.jsonl (6 pairs total with .sender.hash.jsonl).

## 28. T0 stub FAIL explained + adapter rule in skill (2026-10-22)
The 0:09 background full-suite run left run/report-T0-20261022-001157.json: sender 480 frames out, send_errors=33120 (all), receiver 0 packets, verdict FAIL. Read against test source: _run_stub_case launches agents with NO adapter, and test_T0_stub_structure_fails_cleanly ASSERTS FAIL. So this FAIL is the designed outcome, not a regression (my 0:12 main run passed it for the same reason).
Root mechanism verified in sender/vc_sender.cpp: empty --adapter auto-picks the FIRST Npcap device (here a WAN Miniport that cannot send) - every sendpacket returns -1, hence errors==packets and a deaf receiver. Side benefit: this is live proof the <0 send_errors accounting reports true failures correctly (q1 with a real adapter shows 0 errors on the same code path).
Doc: adapter rule added to references/agent-api.md (always pass explicit adapter substring; loopback-without-adapter FAIL is baked into the T0 stub test). Cleanup: removed one-off scripts/q1b_cadence.py (value 4.915 already in S23 table). Run hygiene: 6 unreferenced T0-stub session files (s-4ce62f69/s-49c88cd7, incl. the 0-byte deaf-receiver hashlog) moved to run/archive.d/; the cited T0 report stays in run/. Unreferenced handoff-era 60-frame probe files (probeb.*, batch unknown) also archived. Two more same-shape stub FAIL reports (000914/001145) + their 12 session files archived; cited 001157 report stays. Midday bench-*/solo240* probes (8 files, zero references anywhere) archived. Early file-mode trial filetest* (5 files incl. 1.8MB nv12, superseded by cited fileprobe) archived. Afternoon dual layer (14:40-15:47, S13-era) left as governed evidence. pytest-main2 stub pair (012158/012210 + 12 sessions, same shape) archived the same way.

## 29. Rollback-chain audit: prev is partial by design + staging leaks (2026-10-22, read-only)
Live prev/mirror holds ONLY version.json (0.3.6) while the tree runs 0.3.7. Code-read (agent/vc-agent.py handle_update_binary/handle_rollback): prev snapshots just the files the incoming zip touches (pairs loop with isfile guard), wiped first unless same-version re-apply. So a rollback today restores version.json=0.3.6 but leaves 0.3.7 binaries in place = version/binary skew by design, not by accident. Any update touching a subset (e.g. scripts-only) narrows the next rollback to that subset.
Second: staging/mirror-<ver> dirs are never deleted after success (helper copies out, no cleanup; rmtree only on same-name reuse). mirror-0.3.3 (agent/+version.json) and mirror-0.9.9-norestart (scripts/+version.json) are test residue accumulating forever - disk leak plus confusion about which mirror is live.
Both feed the pending OTA 0.4.0 decision (S19): proposal A already covers full-tree prev snapshot + staging GC semantics. No code touched here (would collide with the awaited redesign); recorded as analysis input. Do NOT hand-delete staging/prev (live rollback machinery).

## 30. Receiver-hashlog-qpc change spec (awaiting nod, NO code yet)
Purpose: locate the single ~1s PC2-side stall behind the bimodal rx_avg (S23 caveat). Verified the exact edit site: receiver/vc_receiver.cpp:408, the only hashlog write:
  hashlog << "{\"frame_id\":" << bk.first << ",\"hash\":\"" << bk.second << "\"}\n";
Spec (2-line-class change): append ,"qpc_ns": + qpc_ns_now() at write time (completion-observed; microseconds after true completion, negligible for ms-scale stall hunting). Same key name the sender hashlog already uses.
Compat, all checked: reconcile.py reads only frame_id/hash (extra keys ignored, old tool reads new files); no agent change (exe writes the file directly); no wire/proto change; no threshold change. After nod: MSVC rebuild + --selftest + next dual run yields timestamped receiver hashlogs, gap analysis runs the same script as sender side. Contract note 2026-10-22: S2.3 freezes the receiver hashlog line as {frame_id,hash} - this change amends that line (additive field, reconcile-compatible); amendment rides with the nod, S2.3 untouched until then.

## 31. OTA-A readiness mapping: Q3 already covered ~30% of proposal A (2026-10-22, read-only)
S19 was written BEFORE Q3 landed; re-mapped current code (endpoint dispatch + _update_slots + handle_update_binary/rollback) against proposal A so the nod is priced correctly:
ALREADY DONE by Q3: children stopped before staging (409 gate + kill_slot; locked-exe fix), missing parent dirs created, exe/script-only updates applied LIVE with no restart, updater capability field in /health. Rollback-via-/update-JSON path exists and is test-exercised.
STILL MISSING (the actual A work): no /activate endpoint (dispatch confirmed: /update only); no staged-but-inactive state (current paths are live-apply-now or helper-restart-now); no in-process os.execv (agent never re-execs; all restarts go through the fragile helper); no versioned side-by-side exes + pointer + boot GC (single vc-agent.exe overwritten post-exit); prev snapshot is per-update-file-set, not full-tree (S29) so rollback can skew.
Net: proposal A is now smaller than S19 priced it - the remaining core is stage/activate split + execv + versioned filenames + GC + full-tree prev. No code touched (still awaiting ACK); this mapping is decision input only.

## 32. T-06 gate scorecard on all dual evidence (decision input, thresholds UNCHANGED)
Frozen gates (test-matrix): avg 4.166+-0.3ms, p99 < 8.3ms. Scored against recorded dual runs (bringup 12 evening runs + S23 six batch runs):
p99 gate: HOLDS everywhere. Evening max 6.987 (T1-checker-rerun), batch max 6.697 (q69). Margin to 8.3 is >=1.3ms on every run. No recalibration needed on this axis.
avg gate: FAILS essentially everything on real NIC. Evening: 11/12 runs at 12.5-27.5 (only T1-noise 4.99 even sniffs the band, still outside 3.866-4.466). Batch: 4.17-4.34 on clean runs, ~13 on stalled ones. Two distinct contaminants: (a) terminal drain artifact (S12, excluded by steady-state S14 method), (b) single ~1s PC2-side stalls inside the 120-window (S23, unlocatable without S30); sender side exonerated across full history (evening T0/T1-noise/T3/T2-soak sender gaps incl. 144k-frame soak: zero gaps over 20ms, scripts/gap_any.py).
What recalibration could use (options, NOT a decision): sender-side cadence (true emission pace: 4.168 batch69 -> 4.915 batch1b, stall-immune by construction, 2400-sample window) as the pace gate; keep p99 as the jitter gate; restrict receiver-avg to steady-state windows or drop it. Numbers above are all already in PLAN/bringup; no new runs, no threshold edits.

## 33. PC2 on-site item reconciled: C:\vc move is STALE, Q4 (Documents) won (2026-10-22, read-only)
Conflict found while pre-baking the on-site checklist: S20.4 says "move agent to C:\vc" but S21/Q4 (later) says "single copy in Documents kept". Winner is Q4 - PC2 live layout proves it: exes at ROOT (vc-receiver.exe/vc-sender.exe found via /files at top level, build/ 404), i.e. flat manual copy, no C:\vc structure. So "move to C:\vc" is SUPERSEDED, do not execute it.
True remaining on-site list: (a) confirm direct cable, (b) retest broadcast (needs cable first; run via vc-ctrl default broadcast dst), (c) "watch checker-transient" - term appears ONLY in the S20 line itself, no procedure exists anywhere; needs definition before anyone can watch it, (d) NEW question surfaced: install-pc2 registers the scheduled task + watchdog for InstallDir (default C:\vc), but the live agent is a manual Documents run (user: "manual restart") - task/watchdog coverage for THIS copy is unverified and unverifiable remotely (/exec allowlist has no schtasks). No files touched; checklist not written (would invent (c), misdirect on C:\vc). Same staleness class found in 部署方案.md (dual-NIC S2, C:\vc paths, .101 peer, old update flow): dated SUPERSEDED-NOTE header added pointing at skill/S11/Q4/S21, doc kept as archaeology. Same treatment applied to 需求文档.md (no-IP clause void, budget numbers stand) and 自动化测试方案.md S1 (dual-NIC rationale void, matrix untouched) - all three early docs now carry dated notes. Stale .101 peer IP in examples fixed to .59 (SKILL.md, 2 script comments, 3 ctrl docstrings; examples only) plus one body example in the deployment doc.

## 34. Open-items consolidated status (2026-10-22; the single nod list)
1. 0.3.6 push (S20.1): CLOSED as moot (S22; superseded by 0.3.7, receiver unchanged).
2. T-06 close (S20.2): AWAITS NOD on redlines. Input ready: S32 scorecard (p99 holds, avg unworkable, recal options without decision).
3. OTA 0.4.0 (S20.3/S19): AWAITS ACK on proposal A. Inputs ready: S29 (prev partial + staging leak), S31 (Q3 covered ~30%, remaining core listed). S19 text itself predates Q3 - read S31 first.
4. PC2 on-site (S20.4): AMENDED by S33. "Move to C:\vc" STRUCK (Q4/Documents won). Left: direct cable, broadcast retest, checker-transient UNDEFINED (needs definition), task/watchdog coverage for manual Documents run UNVERIFIED.
5. Re-stage 0.3.7 (S23/S26): AWAITS NOD (in-place vs r1 filename). Dry-run proven, one file swap.
6. Receiver hashlog qpc (S23/S30): AWAITS NOD. 2-line spec + compat checked, no code.
7. DDA selftest SKIP-not-FAIL (S25): AWAITS NOD (exe + test change). Suite red at night on idle desktop; deterministic parts proven unaffected.

## 35. Bringup-doc audit: 12/12 reports match the table (2026-10-22, scripts/bringup_audit.py)
Every row of docs/bringup-2026-10-21-dual.md re-verified against its run/report file (verdict + sent + complete + pps, +-1 rounding): 12/12 OK, zero mismatches. The headline dual deliverable is file-backed end to end. (T5-dda-smoke report also present, outside the 12.) Follow-up: T0-193245 re-reconciled live (sender s-7555da01 + PC2 s-19ec19e2 fetched, 2400/2400 zero mismatch, file kept as run/s-19ec19e2.receiver.hash.jsonl). T1-noise full fetch impossible: /files effective content cap is 256KB (not the documented 4MB; skill fixed), its 621KB hashlog truncated at 5984 lines - partial copy deleted to avoid confusion; T1-noise stands on in-run mm=0/complete=14400. Extended: T3 + all three T4 runs re-reconciled live (2400/2400 and 3x3600/3600, zero mismatch; files kept as run/s-f5b14ff5/s-b7ce4413/s-1a2cf705/s-f53e0f57.receiver.hash.jsonl). 5 of 12 bringup runs now have offline hash-join proof; the 7 big-frame runs (T1x6+T2) stand on in-run verify only: 2026-10-22 census (scripts/big7_presence.py) shows their PC2 receiver hashlogs are 404-gone (PC2 run/ did not survive reinstall), offline join permanently impossible; all 7 sender hashlogs present locally. Lesson: fetch receiver evidence promptly (as done for q-series + 5 small runs).

## 36. Whole-history report sweep: 43 reports, delivery spotless (2026-10-22, scripts/report_sweep.py kept)
Swept every run/report-*.json (44 files incl. Tloopback: verdict/sent/complete/dropped/mm). hash_mismatch=0 in ALL 43 across the full project history. dropped=0 everywhere except two ancient T1s (143650 drop=1, motion-153647 3596/1) plus one pre-trailing-accounting T4 (143940 comp=1023/1440, settled archaeology). Every other FAIL in history is the avg-gate artifact. This upgrades S32 from 17 runs to full-history coverage: the p99-holds / avg-fails split is not a sampling effect. Noon PASS pair (T0-125233, T1-125255/125311) intact per S15. Session-file cross-check (scripts/session_presence.py): all 88 hashlogs cited by reports resolve in run/ or archive.d, zero missing. PLAN structural lint (scripts/plan_lint.py): sections 0-39 contiguous, no duplicates, all S-refs resolve.

## 37. PC2 helper.log: physical proof of the tree-kill class (2026-10-22, scripts/pc2_logs.py kept)
Fetched PC2 run/helper.log (74 bytes, total): "helper-start wait 13376" + "copying", then SILENCE - no starting/healthy/UNHEALTHY. This is the 18:38 0.3.3 update (PC2 updates.log confirms 0.3.2->0.3.3->0.3.5->0.3.6 OTA chain): the helper died mid-robocopy, ~1s in. Worse: the 19:23 (0.3.5) and 21:34 (0.3.6) updates logged action=update (helper path) yet left ZERO additional helper.log lines - killed before line one. Both S19 kill modes now have PC2-side exhibits in a single 74-byte file. Proposal A rationale upgraded from inference to observation. PC2 exec.log shows only our own .100 diagnostics (ipconfig/list-adapters/netinfo/set_nic), no foreign activity; 0.3.7 manual install unlogged as expected.

## 38. Loopback smoke on current (comment-rebuilt) binary (2026-10-22, scripts/loopback_smoke.py kept)
480 motion frames over Npcap loopback adapter via live local agent, current df5cc0cd binary: sent 480, complete 480, dropped 0, lost 0, mm 0, send_errors 0. Steady-state receiver sample mid-run: avg 4.172/p99 4.834 (clean); terminal avg 12.526 (drain artifact) so frozen verdict FAILS as designed. Live re-exhibit of the S32 split on demand, plus end-to-end proof of the rebuilt binary that selftest-capture cannot give at night. Report: run/report-Tloopback-20261022-011928.json. Its 6 session files archived to run/archive.d/ (report stays, stats embedded). Backlog idea (not implemented): suite still has no passing live test (T0-stub asserts FAIL); a guarded loopback-PASS test (skip without Npcap/binary) is possible follow-up - unasked, flake risk, left for user call. Dual re-probe on df5cc0cd (60f batch69 via brn.py): 60/60 clean, 240.0fps/pps16553 (== q69 exactly); probe files removed.

## 39. Frozen-CLI drift inventory: S2.2 missing --mode dda + --batch (2026-10-22, read-only)
S2.2 pins sender CLI as "--mode pattern|file" with no --batch flag. Both postdate it: --mode dda (S16/S17, T5) and --batch N (S23, T-10). sender --help (current binary) shows: --mode pattern|file|dda, --batch 1, --dst-mac, --session/--stats/--hashlog, --list-adapters, --selftest. Like the S30 contract note: S2.2 stays untouched (frozen); the two missing flags ride with a future amendment nod. No behavior question - binaries and skill already document both.
