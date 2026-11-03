"""Explicit-backend codec -> L2 -> decode -> SHM/DShow loopback; no registration or deployment."""
import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


def run_case(output, codec, encoder, decoder, width=416, height=416, fps=30, frames=60, mode="pattern", loss=False,
             bitrate=20, max_bitrate=80, batch=1, decode_in_flight=2, max_au_bytes=8388608):
    case = output / f"{codec}-{width}x{height}-{fps}-{mode}-{'loss' if loss else 'clean'}"
    case.mkdir()
    processes, logs = [], []
    mapping = "Local\\VCCodec-" + uuid.uuid4().hex
    token = uuid.uuid4().hex[:10]
    mac = "02:" + ":".join(token[i:i+2] for i in range(0, 10, 2))

    def spawn(command, log):
        handle = open(case / log, "wb")
        logs.append(handle)
        child = subprocess.Popen(list(map(str, command)), stdout=handle, stderr=subprocess.STDOUT,
                                 creationflags=subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW)
        processes.append(child)
        return child

    def rows(name):
        return [json.loads(row) for row in (case / name).read_text(encoding="utf-8").splitlines()]

    common = ["--adapter", "Loopback", "--width", width, "--height", height, "--fps", fps,
              "--codec", codec, "--source-mac", mac, "--bitrate", bitrate*1000000, "--max-bitrate", max_bitrate*1000000,
              "--gop", 15, "--decode-in-flight", decode_in_flight, "--max-au-bytes", max_au_bytes]
    try:
        receiver = spawn([ROOT / "build/vc-receiver.exe", *common, "--codec-backend", decoder,
                          "--timeout-s", 8, "--shm", mapping, "--stats", case / "rx.json",
                          "--hashlog", case / "rx.jsonl"], "rx.log")
        from vc_shm import ShmReader
        deadline = time.monotonic() + 5
        while True:
            if receiver.poll() is not None:
                raise RuntimeError((case / "rx.log").read_text())
            try:
                reader = ShmReader(mapping)
                reader.close()
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(.05)
        verifier = spawn([ROOT / "build/vcam_verify/Release/vc-vcam-verify.exe", "--dll",
                          ROOT / "build/Release/vc-vcam.dll", "--shm", mapping, "--width", width,
                          "--height", height, "--fps", fps, "--seconds", 6,
                          "--hashlog", case / "vc.jsonl"], "vc.log")
        time.sleep(.2)
        sender = spawn([ROOT / "build/vc-sender.exe", *common, "--codec-backend", encoder,
                        "--codec-hardware", 1, "--mode", mode, "--pattern", "motion", "--transform", "fit",
                        "--frames", frames, "--batch", batch, "--gpu-convert", *(["--test-drop-frame", 20] if loss else []),
                        "--stats", case / "tx.json", "--hashlog", case / "tx.jsonl"], "tx.log")
        for child, log in ((sender, "tx.log"), (verifier, "vc.log"), (receiver, "rx.log")):
            assert child.wait(30) == 0, (case / log).read_text()
        sent, received, transported, video = rows("tx.jsonl"), rows("rx.jsonl"), rows("rx.jsonl.transport.jsonl"), rows("vc.jsonl")
        expected = {row["frame_id"]: row["transport_hash"] for row in sent}
        assert len(sent) == frames
        if loss:
            assert sent[20]["fault_injected"] is True
            del expected[20]
        assert expected == {row["frame_id"]: row["transport_hash"] for row in transported}
        actual = {row["frame_id"]: row["transport_hash"] for row in received}
        if loss:
            assert all(expected[fid] == value for fid, value in actual.items())
            assert set(range(30, frames)) <= actual.keys()
            assert not set(range(20, 30)) & actual.keys()
        else:
            assert expected == actual
        decoded = {row["decoded_hash"] for row in received}
        assert video and all(row["hash"] in decoded for row in video)
        intervals = [b["send_qpc_ns"] - a["send_qpc_ns"] for a, b in zip(sent, sent[1:])]
        assert min(intervals) >= (10**9 + fps - 1) // fps
        window, window_bytes, peak = [], 0, 0
        for row in sent:
            while window and row["send_qpc_ns"] - window[0][0] >= 10**9:
                window_bytes -= window.pop(0)[1]
            count = row["encoded_bytes"] + 48
            window.append((row["send_qpc_ns"], count))
            window_bytes += count
            peak = max(peak, window_bytes)
        assert peak * 8 <= max_bitrate * 1000000
        stats = json.loads((case / "rx.json").read_text())
        assert stats["complete"] == len(actual)
        if loss:
            assert stats["decode_resets"] >= 1 and stats["decode_discarded"] >= 9
        for key in (("transport_errors", "decode_errors", "decode_queue_drops") if loss else
                    ("dropped", "transport_errors", "decode_errors", "decode_resets", "decode_queue_drops")):
            assert stats[key] == 0, stats
        result = {"pass": True, "codec": codec, "size": [width, height], "fps_limit": fps,
                  "frames": frames, "transport_hashes_matched": len(transported), "decoded_frames": len(received),
                  "dshow_samples_including_stall_repeats": len(video), "dshow_distinct_hashes": len({row["hash"] for row in video}),
                  "minimum_send_interval_ns": min(intervals), "pattern": "motion", "mode": mode,
                  "adapter": "Loopback", "batch": batch, "encoder": encoder, "decoder": decoder,
                  "bitrate_mbps": bitrate, "max_bitrate_mbps": max_bitrate, "peak_one_second_payload_bytes": peak,
                  "decoder_in_flight_limit": decode_in_flight, "max_au_bytes": max_au_bytes,
                  "injected_loss_frame": 20 if loss else None,
                  "decode_resets": stats["decode_resets"], "decode_discarded": stats["decode_discarded"],
                  "pixel_equality_gate": False, "dual_host": False}
        (case / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
        return result
    finally:
        for child in reversed(processes):
            if child.poll() is None:
                child.terminate()
            child.wait(10)
        for handle in logs:
            handle.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codec", choices=("h264", "hevc"), required=True)
    parser.add_argument("--encoder", required=True)
    parser.add_argument("--decoder", required=True)
    parser.add_argument("--matrix", action="store_true")
    parser.add_argument("--dda", action="store_true")
    parser.add_argument("--loss", action="store_true")
    parser.add_argument("--bitrate", type=int, default=20, help="Mbps")
    parser.add_argument("--max-bitrate", type=int, default=80, help="Mbps")
    parser.add_argument("--batch", type=int, default=1)
    parser.add_argument("--decode-in-flight", type=int, default=2)
    parser.add_argument("--max-au-bytes", type=int, default=8388608)
    args = parser.parse_args()
    output = ROOT / "run" / ("p6-codec-transport-" + datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + args.codec + "-" + uuid.uuid4().hex[:6])
    output.mkdir(parents=True)
    print(output, flush=True)
    results = []
    cases = [(416, 416), (640, 640), (1280, 720), (1920, 1080), (2560, 1440), (418, 282)] if args.matrix else [(416, 416)]
    for width, height in cases:
        result = run_case(output, args.codec, args.encoder, args.decoder, width, height, mode="dda" if args.dda else "pattern", loss=args.loss,
                          bitrate=args.bitrate, max_bitrate=args.max_bitrate, batch=args.batch,
                          decode_in_flight=args.decode_in_flight, max_au_bytes=args.max_au_bytes)
        results.append(result)
        (output / "matrix.json").write_text(json.dumps(results, indent=2), encoding="utf-8")
        print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
