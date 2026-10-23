# Agent API（冻结端点，dual-PC 通用）

Base: `http://<ctrl-ip>:18080`. 全部 JSON. `start` 幂等（先停旧会话）。
每个会话带 `session_id`，双端 stats 用它对齐。

| 方法 | 路径 | 入参 | 出参 |
|---|---|---|---|
| GET | `/health` | — | `{ok, version, role, running, updater, pid, staged}` |
| GET | `/version` | — | `{version[, git_sha]}` |
| GET | `/adapters` | — | `{text, names[], exe}`（调 exe --list-adapters，不拉起进程） |
| POST | `/update` | 二进制 zip → `{staged, version}`（只 stage，不重启）；`{rollback:true}` → 经 activate 回滚 prev |
| POST | `/activate` | `{version}` 或 `{rollback:true}` | `{activated, version, rollback, restart}`（`restart=execv`，原地接管，无 detached 进程） |
| POST | `/sender/start` | `{mode, pattern, fps, frames, ethertype, mtu, pacing_us, adapter*, dst_mac, ...}`（*adapter 必填，缺省 400 + adapters 列表，不拉起进程） | `{session_id, args}` |
| POST | `/sender/stop` | — | `{session_id, stopped}` |
| GET | `/sender/stats` | — | `{session_id, running, sent_frames, sent_packets, pps, bps, fps_actual, send_errors, pattern, gpu_ms, gpu_dispatch_ms, gpu_dispatch_samples, cpu_pct}`（P4-1：gpu_ms=wall-clock；gpu_dispatch_ms=D3D11 timestamp 真 GPU 拷贝时间，0 样本时为 0） |
| POST | `/receiver/start` | `{adapter*, ethertype, mtu, expect_frames, timeout_s}`（*adapter 必填，缺省 400 + adapters 列表，不拉起进程） | `{session_id, args}` |
| POST | `/receiver/stop` | — | `{session_id, stopped}` |
| GET | `/receiver/stats` | — | `{session_id, running, complete, dropped, packets_recv, packets_lost, pps, bps, gaps, hash_mismatch, inter_arrival_avg_ms, inter_arrival_p99_ms}` |
| GET | `/receiver/report` | -- | {version, sender, receiver} (both-side stats snapshot) |
| POST | `/pcap/start` | `{seconds}` |
| POST | `/exec` | `{argv:[...], timeout_s}` (allowlist, see below) | `{exec_id, exit_code, stdout, stderr, timed_out, duration_s}` |
| GET | `/exec/log?lines=N` | - | `{entries:[{ts, client, argv, exit_code, timed_out, duration_s}]}` | 失败留证抓包（限时，避免 9K 包撑爆盘） |
| GET | `/files?path=<rel>[&offset=N][&limit=M]` | relative path + byte range | {path, size, offset, truncated, next_offset, content} (under ROOT; every response capped 1MB; default head 256KB; bad offset/limit 400; follow next_offset until null) |

Impl: `agent/vc-agent.py` single-file zero-dep (same code both PCs; --role is label-only). C++ file IPC (atomic stats rewrite + hashlog append); agent starts/stops/passes through. Also: GET /files reads any file under ROOT with optional paging (0.4.0). Adapter rule (P1-1): `--adapter` 必填；agent 缺 adapter 直接 400 并附本机 adapters 列表，不拉起进程；C++ 空 adapter 按 pcap_findalldevs 顺序逐卡试发（首包成功锁定，全败 exit 2）；显式坏 adapter exit 2 并列卡；live 发送 sent_frames>0 但成功包为 0 则 exit 3。

## /exec policy (added 0.2.0, allowlist-bound, NOT a general shell)

- `python` only runs script files under agent ROOT (no `-c`); frozen exe falls back to PATH python.
- `pwsh/powershell` only as `-NoProfile -ExecutionPolicy Bypass -File <script-under-ROOT/scripts> [args]` (no `-Command`).
- Bare names resolve under ROOT/`build`/`scripts` only; absolute paths outside ROOT denied; `..` denied.
- Canned diagnostics: `ipconfig [/all]`, `ping` (max 8 args, `-t` denied).
- `timeout_s` default 30, max 300; stdout/stderr truncated at 128KB each; cwd=ROOT; shell never used.
- Every call appended to `run/exec.log` (ts/client/argv/exit/duration; outputs stay inline-only).
- Server is threaded (`ThreadingHTTPServer`), so a running /exec does not block /health|stats.

## /update + /activate channel (P3，自 0.4.0；detached-helper 已删除）

- `POST /update` 只做 stage：raw zip bytes（`Content-Type: application/zip`，max 100MB）
  校验 → 解压到 `staging/mirror-<ver>` → 全树快照当前文件到 `prev/mirror` → 应答 `{staged:true, version}`，不重启、不碰 live 文件。
- `POST /activate {version}`（缺省用已 stage 版本）或 `{rollback:true}`（用 prev）：拷贝适用条目到 live 位 →
  应答 `{activated, version, rollback, restart}`（`restart=execv`）→ 原地 `os.execv` 接管（同 argv；冻结模式切 `version.json` exe 指针到版本化 exe 并 GC 旧版）。
- 交接期 `health/version` 短暂不可达，轮询确认；`run/updates.log` 记录每次 stage/apply/rollback。
- Zip allowlist：顶层 `vc-agent.exe|vc-agent.py|vc-sender.exe|vc-receiver.exe|version.json` 加 `scripts/*.ps1`；`version.json` 必带；zip-slip（`..`/absolute）400。
- stage 包不含自更新条目时（只换 sender/receiver exe）直接 live 应用并返回 `{updated, version, restart:none}`（先停 sender/receiver 会话）。
- 自更新条目（`vc-agent.exe|vc-agent.py`）若落不到 ROOT 下，整包 400 拒绝（不静默跳过）；冻结模式 ROOT 默认为 exe 所在目录，生产布局不受影响。
- 回滚用 `prev/mirror` 走同一 activate 路径；空备份 → 409；stage 版本不符 → 409；staged agent.py 编译失败 → 500；中途失败一律 500 JSON（不断连接），轮询 `/version` 为准。
- 调用方：`scripts/update.ps1` 与 `ctrl/run_remote.py --update/--rollback` 做 stage→activate→poll `/version`；
  `health.updater` 非 ps-ok 拒绝（该处走 install-pc2.ps1）。同版本重复 stage 保留更旧的 prev 备份。
- Windows 下 CPython execv 表现为子进程接管 + 父进程退出（非同 PID）；以 version 翻转 + 单监听 + updates.log 链为准，不以 PID 不变论。同版本冻结重装时目标即运行中镜像，apply 自动改用时间戳唯一文件名并更新指针（老文件下次启动 GC）。


