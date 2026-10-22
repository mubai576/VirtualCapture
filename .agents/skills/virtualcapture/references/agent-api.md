# Agent API（冻结端点，dual-PC 通用）

Base: `http://<ctrl-ip>:18080`. 全部 JSON. `start` 幂等（先停旧会话）。
每个会话带 `session_id`，双端 stats 用它对齐。

| 方法 | 路径 | 入参 | 出参 |
|---|---|---|---|
| GET | `/health` | — | `{ok, version, role, running, updater}` |
| GET | `/version` | — | `{version[, git_sha]}` |
| GET | `/adapters` | — | `{text, names[], exe}`（调 exe --list-adapters，不拉起进程） |
| POST | `/update` | 二进制 zip 或 `{rollback:true}` | `{updated, version}`（原子替换+重启） |
| POST | `/sender/start` | `{mode, pattern, fps, frames, ethertype, mtu, pacing_us, adapter*, dst_mac, ...}`（*adapter 必填，缺省 400 + adapters 列表，不拉起进程） | `{session_id, args}` |
| POST | `/sender/stop` | — | `{session_id, stopped}` |
| GET | `/sender/stats` | — | `{session_id, running, sent_frames, sent_packets, pps, bps, fps_actual, send_errors, pattern, gpu_ms, cpu_pct}` |
| POST | `/receiver/start` | `{adapter*, ethertype, mtu, expect_frames, timeout_s}`（*adapter 必填，缺省 400 + adapters 列表，不拉起进程） | `{session_id, args}` |
| POST | `/receiver/stop` | — | `{session_id, stopped}` |
| GET | `/receiver/stats` | — | `{session_id, running, complete, dropped, packets_recv, packets_lost, pps, bps, gaps, hash_mismatch, inter_arrival_avg_ms, inter_arrival_p99_ms}` |
| GET | `/receiver/report` | -- | {version, sender, receiver} (both-side stats snapshot) |
| POST | `/pcap/start` | `{seconds}` |
| POST | `/exec` | `{argv:[...], timeout_s}` (allowlist, see below) | `{exec_id, exit_code, stdout, stderr, timed_out, duration_s}` |
| GET | `/exec/log?lines=N` | - | `{entries:[{ts, client, argv, exit_code, timed_out, duration_s}]}` | 失败留证抓包（限时，避免 9K 包撑爆盘） |
| GET | `/files?path=<rel>` | relative path | {path, size, truncated, content} (under ROOT; files >4MB refused, content capped at 256KB, check truncated) |

Impl: `agent/vc-agent.py` single-file zero-dep (same code both PCs; --role is label-only). C++ file IPC (atomic stats rewrite + hashlog append); agent starts/stops/passes through. Also: GET /files?path=<rel> reads any file under ROOT (files >4MB refused, content capped at 256KB, check truncated). Adapter rule (P1-1): `--adapter` 必填；agent 缺 adapter 直接 400 并附本机 adapters 列表，不拉起进程；C++ 空 adapter 按 pcap_findalldevs 顺序逐卡试发（首包成功锁定，全败 exit 2）；显式坏 adapter exit 2 并列卡；live 发送 sent_frames>0 但成功包为 0 则 exit 3。

## /exec policy (added 0.2.0, allowlist-bound, NOT a general shell)

- `python` only runs script files under agent ROOT (no `-c`); frozen exe falls back to PATH python.
- `pwsh/powershell` only as `-NoProfile -ExecutionPolicy Bypass -File <script-under-ROOT/scripts> [args]` (no `-Command`).
- Bare names resolve under ROOT/`build`/`scripts` only; absolute paths outside ROOT denied; `..` denied.
- Canned diagnostics: `ipconfig [/all]`, `ping` (max 8 args, `-t` denied).
- `timeout_s` default 30, max 300; stdout/stderr truncated at 128KB each; cwd=ROOT; shell never used.
- Every call appended to `run/exec.log` (ts/client/argv/exit/duration; outputs stay inline-only).
- Server is threaded (`ThreadingHTTPServer`), so a running /exec does not block /health|stats.

## /update channel (real since 0.3.0; was stub before)

- `POST /update` with raw zip bytes (`Content-Type: application/zip`, max 100MB)
  or JSON `{rollback:true}`.
- Zip allowlist: top-level `vc-agent.exe|vc-agent.py|vc-sender.exe|vc-receiver.exe|version.json`
  plus `scripts/*.ps1`; `version.json` required; zip-slip (`..`/absolute) rejected 400.
- Flow: validate -> extract to staging mirror of the live tree -> snapshot current files
  to `prev/mirror` -> spawn detached PowerShell helper -> respond `{updated, version}` -> agent exits.
  Helper waits for agent PID, `robocopy` mirror over live dirs, restarts agent with identical
  argv, deletes itself. `run/updates.log` records every apply/rollback. Helper passes exe argv as array, polls /health, auto-rolls-back on failure. /health.updater reports ps-ok/ps-missing; update.ps1 and run_remote --update refuse non-ps-ok targets (use install-pc2.ps1 there). Same-version re-apply keeps the older prev backup.
- Rollback replays `prev/mirror` the same way; empty backup -> 409.
- Only entries applicable to the running layout are applied (frozen exe vs script);
  nothing under `run/` is ever replaced. Poll `/version` to confirm (agent is briefly down).


