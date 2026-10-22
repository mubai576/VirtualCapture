---
name: virtualcapture
description: Deploy and validate the VirtualCapture 640x640@240 NV12 L2 streaming pipeline (PC1 sender to PC2 receiver over 2.5G direct link). Use when deploying vc-agent, running streaming tests T0-T5, debugging packet loss or hash mismatch, changing the wire protocol or test patterns, or onboarding a new AI/dev onto this project.
---

# VirtualCapture skill

Project root is `../../..` from this skill directory
(`.agents/skills/virtualcapture/` → repo root `VirtualCapture/`).
All paths below are relative to **project root** unless stated otherwise.

## What this project is

PC1 keeps `2K240` normal use; only `640x640@240 NV12` (614400 B/frame, ~1.18 Gbps)
is sent over a 2.5G direct cable via **raw L2 custom EtherType `0x88B5`** (Npcap,
NIC keeps its control IP) to PC2 for reassembly, hash-verified bit-exact, no codec.
Transport is dual-host validated (T-06 closed 2026-10-22). Current phase P4: capture perf,
capture->reassembly latency, PC2 virtual capture card output (DShow). Scope = capture + transport only;
inference side and exclusive-fullscreen hook are out of scope (PLAN D8).

Repo map, commands, rules and multi-AI conventions: **root `AGENTS.md`** (single source; not repeated here).
Progress and open decisions: `PLAN.md`. Specs: `docs/spec/` (requirements / test-plan / deployment,
early docs with dated SUPERSEDED notes). This skill holds the domain knowledge + frozen contracts.

## Contract files

| Path | Truth for |
|---|---|
| `proto/vc_wire.h` | **Wire header — frozen.** `vc_pkt_hdr_t` 28 B. `reserved` is now `pattern`. Do not resize without bumping protocol + updating both ends + this skill. |
| `sender/pattern.h` | Test-source suite (5 patterns, integer-only, seed=`frame_id`). C++/Python bit-exact by construction (no float/sin). Mirror: `ctrl/pattern_ref.py`. |
| `references/wire-protocol.md` | L2 layout, MTU/pacing math, `--batch` sendqueue semantics. |
| `references/agent-api.md` | Agent REST contract (/adapters, /files paging, /update stage + /activate). |
| `references/test-matrix.md` | T0–T5 definitions, D1 gates, triage. |

## Topology (shared-NIC since 2026-10-21: data + control on the same 2.5G port; see docs/history/plan-history.md S11)

- **Data plane**: PC1-PC2 direct 2.5G cable, MTU 9000, EtherType 0x88B5 raw frames via Npcap.
  Raw EtherType and TCP coexist on one NIC, so the NIC keeps its control IP (no IP stripping).
- **Control plane**: the same NICs on 192.168.2.x (PC1=192.168.2.100, PC2=192.168.2.59). All agent/ctrl HTTP goes here.
- Sender agent: `http://<pc1-ctrl-ip>:18080`; receiver agent: `http://<pc2-ctrl-ip>:18080`.

## Test sources (use these, not static frames)

Static frames cheat (DDA dirty-rects skip them, framebuffer compression loves them,
chroma bugs hide in flat colors). Suite in `sender/pattern.h`:

- `counter` — smoke only. `checker` — kills NV12 4:2:0 misalignment.
- `rings` — kills resample/matrix aliasing. `noise` — max entropy, kills hidden compression/cheating.
- `motion` — **default stress source** (scroll bg + 4 bouncing sprites + 256px noise window).

Verification is content-agnostic: sender logs per-frame FNV1a64 → receiver logs
post-reassembly hash → ctrl reconciles. Determinism (seed=`frame_id`) is for
loss-replay debugging only. Online: 4K-sample spot check; offline: full hash reconcile.

## Quickstart for an AI session

```powershell
# 0. Sanity (no hardware): full local gate = python scripts/verify.py ; quick check below
python ctrl/pattern_ref.py            # expect PASS
# 0.1 查卡（--adapter 必填，先 GET /adapters 再跑；adapter 空跑必被 agent/ctrl 拒绝）
# curl http://127.0.0.1:18080/adapters  # -> {text, names[]}，选含 2.5G/Controller 子串
# 1. Control-plane loopback (no second PC needed)
python agent/vc-agent.py --role sender --port 18081 &
python ctrl/vc-ctrl.py --sender http://127.0.0.1:18081 --receiver http://127.0.0.1:18081 --seconds 10 --adapter <Loopback子串>
# 2. Real dual-host T1 (fill in control-plane IPs，双机加 --batch 69 --dst-mac <对端MAC> 单播)
python ctrl/vc-ctrl.py --sender http://192.168.2.100:18080 --receiver http://192.168.2.59:18080 --seconds 60 --adapter <2.5G子串> --batch 69 --dst-mac <peer-MAC>
```

Pass criteria: T0 loopback hash-clean → T1 dual-host 60 s `dropped==0, hash_mismatch==0`
→ T2 10-min soak `pps≈16560±2%, bps≈1.18G, inter-arrival p99<8.3ms`.
Full matrix + thresholds: `references/test-matrix.md`.

## Rules for contributors (AI or human)

1. `proto/vc_wire.h` is frozen at 28 B. Any field change → update sender+receiver+
   `references/wire-protocol.md` + version bump, never silent.
2. New test patterns must be integer-only, deterministic in `frame_id`, with a Python
   mirror in `ctrl/pattern_ref.py` and a self-check assertion.
3. Never validate with static frames alone; `motion` is the minimum bar for perf claims.
4. Cross-host QPC timestamps are not comparable; report inter-arrival jitter, not absolute latency.
5. Version skew fails the run: ctrl must compare `/version` on both agents before T1.
6. Keep this skill + `AGENTS.md` + PLAN §1 in sync with code: if you change topology/API/matrix,
   update the corresponding `references/` file in the same change.

## References

- `references/agent-api.md` — control-agent REST contract (frozen endpoints).
- `references/wire-protocol.md` — L2 frame layout, header fields, MTU/pacing math.
- `references/test-matrix.md` — T0–T5 definitions, pass thresholds, failure triage.


