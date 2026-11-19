"""Measure two Release vc-stream processes on the exact Npcap Loopback adapter.

Run with `python -I rust/tests/raw_stream_perf.py`. This measures 2400 Raw NV12
motion frames (640x640@240, MTU 9000, batch 1 by default) with private SHM v3.
Pass --batch 69 for an explicit batching comparison; there is no fallback.
Return 0 when frame integrity and requested cadence/latency targets pass; the
report records T0 CPU gates separately. This is not physical-link acceptance.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
import uuid

# The documented -I invocation excludes the script directory from sys.path.
sys.path.insert(0, str(Path(__file__).resolve().parent))
import raw_stream_smoke as smoke  # noqa: E402

FRAMES = 2400
FPS = 240
WIDTH = HEIGHT = 640
MTU = 9000
DEFAULT_BATCH = 1
LIMIT_NS = 10_000_000


def summarize_ns(samples):
    values = sorted(samples)
    count = len(values)
    return {
        "samples": count,
        "mean_ms": sum(values) / count / 1e6,
        "p50_ms": values[(50 * count + 99) // 100 - 1] / 1e6,
        "p99_ms": values[(99 * count + 99) // 100 - 1] / 1e6,
        "max_ms": values[-1] / 1e6,
        "gte_10ms": sum(value >= LIMIT_NS for value in values),
    }


def timing(tx, rx):
    source_start, source_ready, first_packet, completed = [], [], [], []
    for fid in range(len(tx)):
        start = tx[fid].get("qpc_ns")
        ready = tx[fid].get("send_qpc_ns")
        first = rx[fid].get("first_packet_qpc_ns")
        done = rx[fid].get("qpc_ns")
        smoke.require(all(smoke.is_int(value) and value > 0
                          for value in (start, ready, first, done))
                      and start <= ready <= first <= done,
                      f"frame {fid}: invalid same-host QPC order: {start}, {ready}, {first}, {done}")
        source_start.append(start)
        source_ready.append(ready)
        first_packet.append(first)
        completed.append(done)

    def cadence(times, role):
        intervals = [right - left for left, right in zip(times, times[1:])]
        smoke.require(intervals and min(intervals) > 0, f"{role}: non-increasing frame times")
        return summarize_ns(intervals), len(intervals) * 1e9 / sum(intervals)

    tx_intervals, tx_fps = cadence(source_start, "TX frame start")
    ready_intervals, ready_fps = cadence(source_ready, "TX frame ready")
    rx_intervals, rx_fps = cadence(completed, "RX completion")
    latencies = [done - ready for ready, done in zip(source_ready, completed)]
    latency = summarize_ns(latencies)
    return {
        "source_ready_to_rx_ms": latency,
        "source_start_to_rx_ms": summarize_ns(d - s for s, d in zip(source_start, completed)),
        "generation_ms": summarize_ns(r - s for s, r in zip(source_start, source_ready)),
        "source_ready_to_first_packet_ms": summarize_ns(f - r for r, f in zip(source_ready, first_packet)),
        "reassembly_ms": summarize_ns(d - f for f, d in zip(first_packet, completed)),
        "tx_interval_ms": tx_intervals,
        "tx_ready_interval_ms": ready_intervals,
        "rx_interval_ms": rx_intervals,
        "tx_steady_fps": tx_fps,
        "tx_ready_steady_fps": ready_fps,
        "rx_steady_fps": rx_fps,
        "worst_frame_id": max(range(len(latencies)), key=latencies.__getitem__),
        "latency_p99_lt_10ms": latency["p99_ms"] < 10.0,
        "all_frames_lt_10ms": latency["gte_10ms"] == 0,
        "sender_cadence_gate": abs(tx_intervals["mean_ms"] - (1000 / FPS)) <= 0.3,
        "requested_performance_pass": (
            abs(tx_intervals["mean_ms"] - (1000 / FPS)) <= 0.3
            and latency["p99_ms"] < 10.0
        ),
    }


def measure(exe, run, token, ref_samples, batch, report):
    cfg = smoke.build_config(FRAMES, FPS, WIDTH, HEIGHT, MTU, batch)
    session = "rperf-" + token
    mac = smoke.make_local_mac()
    shm = "Local\\VirtualCaptureRawPerf." + token
    report.update(config=cfg, session_id=session, source_mac=mac, shm_name=shm)
    smoke.require(exe.is_file(), f"Release vc-stream not found: {exe}")
    with exe.open("rb") as binary:
        report["binary_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()

    ready_path = run / "rx-ready.json"
    common = ["--adapter", smoke.LOOPBACK, "--session", session, "--source-mac", mac,
              "--width", str(WIDTH), "--height", str(HEIGHT), "--fps", str(FPS), "--mtu", str(MTU)]
    rx_cmd = [exe, "receive", *common, "--timeout-s", str(cfg["receiver_timeout_s"]),
              "--expect-frames", str(FRAMES), "--ready", ready_path, "--shm", shm,
              "--stats", run / "rx-stats.json", "--hashlog", run / "rx-hash.jsonl"]
    tx_cmd = [exe, "send", *common, "--frames", str(FRAMES), "--pattern", "motion",
              "--batch", str(batch), "--dst-mac", smoke.DEST_MAC,
              "--stats", run / "tx-stats.json", "--hashlog", run / "tx-hash.jsonl"]
    report["commands"] = {"rx": list(map(str, rx_cmd)), "tx": list(map(str, tx_cmd))}
    rx = smoke.Child("rx", rx_cmd, run)
    smoke.wait_for_path(rx, ready_path, cfg["ready_timeout_s"], "RX")
    ready = smoke.read_json(ready_path, "RX ready")
    report["ready"] = ready
    smoke.validate_ready(ready, session, "RX ready")
    smoke.require(ready.get("geometry") == {"width": WIDTH, "height": HEIGHT, "fps": FPS}
                  and ready.get("mtu") == MTU and ready.get("shm_version") == 3,
                  f"incorrect receiver configuration: {ready}")

    view = smoke.open_shm_view(shm, rx, cfg["ready_timeout_s"], "RX")
    try:
        tx = smoke.Child("tx", tx_cmd, run)
        smoke.wait_children([rx, tx], cfg["stream_timeout_s"], "Raw performance")
        report["exit_codes"] = {"tx": tx.proc.returncode, "rx": rx.proc.returncode}
        tx_stats = smoke.read_json(run / "tx-stats.json", "TX stats")
        rx_stats = smoke.read_json(run / "rx-stats.json", "RX stats")
        report.update(tx_stats=tx_stats, rx_stats=rx_stats)
        smoke.validate_tx_stats(tx_stats, FRAMES, cfg["total_packets"], session, "TX stats")
        smoke.validate_rx_stats(rx_stats, FRAMES, session, True, "RX stats")
        smoke.require(tx_stats.get("pattern") == "motion" and tx_stats.get("fps_target") == FPS
                      and tx_stats.get("batch") == batch and tx_stats.get("pacing_us") == 0,
                      f"incorrect sender configuration: {tx_stats}")
        for role, stats in (("TX", tx_stats), ("RX", rx_stats)):
            smoke.require([stats.get(key) for key in ("codec", "width", "height", "mtu", "ethertype")]
                          == ["raw", WIDTH, HEIGHT, MTU, "0x88b5"],
                          f"{role}: unexpected stream configuration: {stats}")
        smoke.require(rx_stats.get("packets_recv") == FRAMES * cfg["total_packets"]
                      and rx_stats.get("duplicate") == 0,
                      "RX: unexpected packet or duplicate count")

        tx_rows = smoke.parse_hash_rows(run / "tx-hash.jsonl", FRAMES, "TX hashlog")
        rx_rows = smoke.parse_hash_rows(run / "rx-hash.jsonl", FRAMES, "RX hashlog")
        tx_hashes = {fid: row["hash"] for fid, row in tx_rows.items()}
        rx_hashes = {fid: row["hash"] for fid, row in rx_rows.items()}
        smoke.require_no_mismatch(tx_hashes, rx_hashes, "TX", "RX", "Raw performance")
        report["matched_frames"] = FRAMES
        report.update(timing(tx_rows, rx_rows))
        report["shm"] = smoke.validate_shm(view, shm, rx, cfg, rx_hashes, "RX private SHM")
    finally:
        view.close()

    # Do the reference calculation after the endpoints stop so it cannot affect timing.
    ref_ids = smoke.sample_frames(FRAMES, ref_samples)
    reference = smoke.reference_hashes(ref_ids, WIDTH, HEIGHT)
    smoke.require_no_mismatch(tx_hashes, reference, "TX", "pattern_ref", "Raw performance")
    report["reference_frame_ids"] = ref_ids


def self_check():
    # Full-run p99 can pass while a single frame exceeds the strict limit.
    tx, rx = {}, {}
    for fid in range(100):
        start = 1_000_000_000 + fid * 4_166_667
        tx[fid] = {"qpc_ns": start, "send_qpc_ns": start + 100}
        rx[fid] = {"first_packet_qpc_ns": start + 200,
                   "qpc_ns": start + 100 + (LIMIT_NS if fid == 99 else 500_000)}
    result = timing(tx, rx)
    smoke.require(result["latency_p99_lt_10ms"] and not result["all_frames_lt_10ms"]
                  and result["worst_frame_id"] == 99 and result["source_ready_to_rx_ms"]["p99_ms"] == 0.5,
                  result)
    rx[99]["qpc_ns"] = tx[99]["qpc_ns"]
    smoke.expect_smoke_error(lambda: timing(tx, rx), "negative same-host latency")
    print("Raw performance timing self-check passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rust-exe", type=Path, default=smoke.DEFAULT_RUST_EXE)
    parser.add_argument("--ref-samples", type=int, default=12, help="0 checks all frames; default 12")
    parser.add_argument("--batch", type=int, default=DEFAULT_BATCH,
                        help="explicit sender batch size; default 1; no retry or fallback")
    parser.add_argument("--self-check", action="store_true")
    args = parser.parse_args()
    if args.self_check:
        self_check()
        return 0
    if sys.platform != "win32" or args.ref_samples < 0 or not 1 <= args.batch <= 10000:
        parser.error("live measurement requires Windows, non-negative --ref-samples and batch 1..10000")

    token = uuid.uuid4().hex[:10]
    run = smoke.ROOT / "run" / ("rust-raw-perf-" + token)
    run.mkdir(parents=True)
    report = {"run": str(run), "scope": "same-host Npcap Loopback only",
              "adapter": smoke.LOOPBACK, "pattern": "motion", "logical_cpu_count": os.cpu_count()}
    try:
        measure(args.rust_exe.resolve(), run, token, args.ref_samples, args.batch, report)
    except Exception as error:
        report["error"] = str(error)
    finally:
        report["cleanup_errors"] = smoke.stop_all()
    report["run_valid"] = "error" not in report and not report["cleanup_errors"]
    report["t0_resource_gates"] = {
        "sender_cpu_pct_below_25": report.get("tx_stats", {}).get("cpu_pct", 1000) < 25,
        "receiver_cpu_pct_below_30": report.get("rx_stats", {}).get("cpu_pct", 1000) < 30,
    }
    report["t0_resource_gates"]["all_pass"] = all(report["t0_resource_gates"].values())
    report["ok"] = bool(report.get("run_valid") and
                        report.get("requested_performance_pass"))
    (run / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    keys = ("ok", "run_valid", "requested_performance_pass", "sender_cadence_gate", "run",
            "batch", "tx_steady_fps", "rx_steady_fps", "tx_interval_ms", "source_ready_to_rx_ms",
            "matched_frames", "t0_resource_gates", "error", "cleanup_errors")
    print(json.dumps({key: report[key] for key in keys if key in report}))
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
