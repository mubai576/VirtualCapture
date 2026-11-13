"""Explicit MJPEG/PyroWave -> L2 -> decode -> SHM/DShow loopback; no registration or deployment."""
import argparse
from datetime import datetime
import json
from pathlib import Path
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


def validate_case(width, height, fps, frames, seconds, loss, bitrate=20, max_bitrate=80,
                  batch=1, decode_in_flight=2, max_au_bytes=8388608, gop=1):
    if any(not 2 <= size <= 8192 or size % 2 for size in (width, height)):
        raise ValueError("width/height must be even integers in 2..8192")
    if not 1 <= fps <= 1000 or not 2 <= frames <= 1000000:
        raise ValueError("fps must be 1..1000 and frames must be 2..1000000")
    if not 3 <= seconds <= 122 or frames / fps >= seconds - 2:
        raise ValueError("test-seconds must be 3..122 and leave more than two seconds after frames/fps")
    if loss and frames < 45:
        raise ValueError("loss recovery requires at least 45 frames to observe the next independent frame")
    if not 1 <= bitrate <= max_bitrate <= 2000:
        raise ValueError("bitrate must be 1..max-bitrate, max-bitrate must be <=2000 Mbps")
    if not 1 <= batch <= 10000 or not 1 <= decode_in_flight <= 8:
        raise ValueError("batch must be 1..10000 and decode-in-flight must be 1..8")
    if not 1024 <= max_au_bytes <= 16777216:
        raise ValueError("max-au-bytes must be 1024..16777216")
    if type(gop) is not int or gop != 1:
        raise ValueError("encoded frames are independent; gop must be 1")


def distribution(values):
    if not values:
        raise ValueError("cannot summarize an empty sample")
    ordered = sorted(values)
    return {"avg": sum(values) / len(values), "p50": ordered[len(values) // 2],
            "p99": ordered[min(len(values) - 1, len(values) * 99 // 100)], "max": ordered[-1]}


def preparation_metrics(sent, fps, gop):
    """Replay the bounded predictor rather than treating higher FPS as fresher frames."""
    fields = {"prepare_lead_ns", "prepare_work_ns"}
    if not any(fields & row.keys() for row in sent):
        return None  # Historical binaries do not expose predictor evidence.
    recent = [[], []]
    period = (10**9 + fps - 1) // fps
    leads, work, over_budget, cold_frames = [], [], 0, 0
    for row in sent:
        if not fields <= row.keys() or any(type(row[key]) is not int or row[key] < 0 for key in fields):
            raise ValueError("incomplete/invalid preparation evidence")
        values = recent[int(row["frame_id"] % gop == 0)]
        expected = (min(period, sorted(values)[(len(values) * 95 + 99) // 100 - 1] + 100000)
                    if values else 0)
        if row["prepare_lead_ns"] != expected:
            raise ValueError("preparation lead is not bounded recent64 p95 + 0.1ms")
        leads.append(expected / 10**6)
        work.append(row["prepare_work_ns"] / 10**6)
        over_budget += row["prepare_work_ns"] > expected
        cold_frames += not values
        values.append(row["prepare_work_ns"])
        del values[:-64]
    return {"policy": "p95_recent64_by_frame_type", "samples": len(sent),
            "lead_ms": distribution(leads), "work_ms": distribution(work),
            "work_exceeds_prediction_frames": over_budget, "cold_frames": cold_frames}


def frame_metrics(sent, received, video, mode, gpu_convert):
    intervals = [b["send_qpc_ns"] - a["send_qpc_ns"] for a, b in zip(sent, sent[1:])]
    if not intervals or min(intervals) <= 0:
        raise ValueError("need at least two strictly ordered send timestamps")
    return {"minimum_send_interval_ns": min(intervals),
            "send_interval_ms": distribution([value / 10**6 for value in intervals]),
            "sender_capture_to_send_ms": distribution([(row["send_qpc_ns"] - row["qpc_ns"]) / 10**6 for row in sent]),
            "pattern": "motion" if mode == "pattern" else None, "mode": mode,
            "gpu_convert": gpu_convert and mode == "dda",
            "measured_send_fps": (len(sent) - 1) * 10**9 / (sent[-1]["send_qpc_ns"] - sent[0]["send_qpc_ns"]),
            "measured_decode_fps": ((len(received) - 1) * 10**9 / (received[-1]["qpc_ns"] - received[0]["qpc_ns"])
                                    if len(received) > 1 and received[-1]["qpc_ns"] > received[0]["qpc_ns"] else None),
            "source_distinct_hashes": len({row["source_hash"] for row in sent}),
            "decoded_distinct_hashes": len({row["decoded_hash"] for row in received}),
            "dshow_samples_including_stall_repeats": len(video),
            "dshow_distinct_hashes": len({row["hash"] for row in video}),
            "decode_pipeline_ms": distribution([row["decode_pipeline_ms"] for row in received])}


def wait_children(children, case, seconds):
    pending = list(children)
    deadline = time.monotonic() + max(30, seconds + 10)
    while pending:
        for child, log in pending[:]:
            code = child.poll()
            if code is None:
                continue
            if code:
                raise RuntimeError(f"{log} exited {code}: {(case / log).read_text(encoding='utf-8', errors='replace')}")
            pending.remove((child, log))
        if pending and time.monotonic() >= deadline:
            raise TimeoutError("timed out waiting for " + ", ".join(log for _, log in pending))
        if pending:
            time.sleep(.05)


def run_case(output, codec, encoder, decoder, width=416, height=416, fps=30, frames=60, mode="pattern", loss=False,
             bitrate=20, max_bitrate=80, batch=1, decode_in_flight=2, max_au_bytes=8388608,
             transform="fit", seconds=8, encoder_hardware=False, decoder_hardware=False, gpu_convert=True, gop=1,
             jpeg_quality=None):
    validate_case(width, height, fps, frames, seconds, loss, bitrate, max_bitrate, batch, decode_in_flight, max_au_bytes, gop)
    jpeg = codec_options(codec, encoder, decoder, encoder_hardware, decoder_hardware, gop, jpeg_quality)
    case = output / f"{codec}-{width}x{height}-{fps}-{mode}-{transform}-{'gpu' if gpu_convert and mode == 'dda' else 'cpu'}-{'loss' if loss else 'clean'}"
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
              "--gop", gop, "--decode-in-flight", decode_in_flight, "--max-au-bytes", max_au_bytes]
    if jpeg is not None: common += ["--jpeg-quality", jpeg]
    try:
        receiver = spawn([ROOT / "build/vc-receiver.exe", *common, "--codec-backend", decoder,
                          "--codec-hardware", int(decoder_hardware), "--timeout-s", seconds, "--shm", mapping, "--stats", case / "rx.json",
                          "--hashlog", case / "rx.jsonl"], "rx.log")
        from vc_shm import ShmReader
        deadline = time.monotonic() + 5
        while True:
            if receiver.poll() is not None:
                raise RuntimeError((case / "rx.log").read_text())
            try:
                reader = ShmReader(mapping)
                try:
                    assert (reader.width, reader.height, reader.fps) == (width, height, fps)
                finally:
                    reader.close()
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(.05)
        verifier = spawn([ROOT / "build/vcam_verify/Release/vc-vcam-verify.exe", "--dll",
                          ROOT / "build/Release/vc-vcam.dll", "--shm", mapping, "--width", width,
                          "--height", height, "--fps", fps, "--seconds", seconds - 2,
                          "--hashlog", case / "vc.jsonl"], "vc.log")
        time.sleep(.2)
        sender = spawn([ROOT / "build/vc-sender.exe", *common, "--codec-backend", encoder,
                        "--codec-hardware", int(encoder_hardware), "--mode", mode, "--pattern", "motion", "--transform", transform,
                        "--frames", frames, "--batch", batch, *(["--gpu-convert"] if gpu_convert and mode == "dda" else []),
                        *(["--test-drop-frame", 20] if loss else []),
                        "--stats", case / "tx.json", "--hashlog", case / "tx.jsonl"], "tx.log")
        wait_children(((sender, "tx.log"), (verifier, "vc.log"), (receiver, "rx.log")), case, seconds)
        sent, received, transported, video = rows("tx.jsonl"), rows("rx.jsonl"), rows("rx.jsonl.transport.jsonl"), rows("vc.jsonl")
        expected = {row["frame_id"]: row["transport_hash"] for row in sent}
        assert len(sent) == frames
        assert [row["frame_id"] for row in sent] == list(range(frames))
        if loss:
            assert sent[20]["fault_injected"] is True
            del expected[20]
        assert expected == {row["frame_id"]: row["transport_hash"] for row in transported}
        assert len(transported) == len(expected)
        actual = {row["frame_id"]: row["transport_hash"] for row in received}
        if loss:
            assert all(expected[fid] == value for fid, value in actual.items())
            recovered = 21 if codec in ("mjpeg", "pyrowave") else 30
            assert set(range(recovered, frames)) <= actual.keys()
            assert not set(range(20, recovered)) & actual.keys()
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
        sender_stats = json.loads((case / "tx.json").read_text())
        quality = sender_stats.get("encoder_quality")
        encoder_config = sender_stats.get("encoder_configuration")
        if codec == "mjpeg":
            assert quality is None and encoder_config["family"] == "turbojpeg"
            assert encoder_config["quality"] == jpeg and encoder_config["identity"] == "turbojpeg:cpu"
            assert encoder_config["target_bitrate_applied"] is False
        else:
            budget = min(max_bitrate * 10**6 // 8 // fps, max_au_bytes - 48, width * height * 3 // 2)
            assert quality is None and encoder_config == {
                "family": "pyrowave", "identity": "pyrowave:vulkan", "revision": 1,
                "width": width, "height": height, "subsampling": "420",
                "max_bytes_per_frame": budget, "upload_path": "cpu_nv12", "decode_path": "cpu_yuv420p"}
            assert all(row["encoded_bytes"] <= budget for row in sent)
        assert all(row["keyframe"] for row in sent)
        assert stats["complete"] == len(actual) == len(received)
        assert stats["decode_pending"] == 0 and stats["failed"] is False
        if loss:
            assert stats["decode_resets"] >= 1 and stats["decode_discarded"] == 0
        for key in (("transport_errors", "decode_errors", "decode_queue_drops") if loss else
                    ("dropped", "transport_errors", "decode_errors", "decode_resets", "decode_queue_drops")):
            assert stats[key] == 0, stats
        result = {"pass": True, "codec": codec, "size": [width, height], "fps_limit": fps,
                  "frames": frames, "transport_hashes_matched": len(transported), "decoded_frames": len(received),
                  **frame_metrics(sent, received, video, mode, gpu_convert),
                  "adapter": "Loopback", "batch": batch, "encoder": encoder, "decoder": decoder,
                  "encoder_hardware_category": encoder_hardware, "decoder_hardware_category": decoder_hardware,
                  "transform": transform, "test_seconds": seconds,
                  "bitrate_mbps": bitrate, "max_bitrate_mbps": max_bitrate, "peak_one_second_payload_bytes": peak,
                  "decoder_in_flight_limit": decode_in_flight, "max_au_bytes": max_au_bytes,
                  "injected_loss_frame": 20 if loss else None,
                  "decode_resets": stats["decode_resets"], "decode_discarded": stats["decode_discarded"],
                  "sender_timing_ms": sender_stats.get("timing_ms"),
                  "sender_preparation": preparation_metrics(sent, fps, gop),
                  "encoder_configuration": encoder_config,
                  "gop_frames": gop,
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


def codec_options(codec, encoder, decoder, encoder_hardware, decoder_hardware, gop, quality):
    if codec not in ("mjpeg", "pyrowave"):
        if quality is not None: raise ValueError("JPEG quality is only valid for MJPEG")
        raise ValueError("only MJPEG and PyroWave remain; H.264/H.265 were removed")
    if gop != 1:
        raise ValueError(f"{codec} frames are independent; gop must be 1")
    if encoder_hardware or decoder_hardware:
        raise ValueError(f"{codec} is not a vendor hardware codec; hardware must be 0")
    if codec == "pyrowave":
        if quality is not None: raise ValueError("JPEG quality is only valid for MJPEG")
        if encoder != "pyrowave:vulkan" or decoder != "pyrowave:vulkan":
            raise ValueError("PyroWave requires explicit pyrowave:vulkan on both sides")
        return None
    if encoder != "turbojpeg:cpu" or decoder != "turbojpeg:cpu":
        raise ValueError("MJPEG requires explicit turbojpeg:cpu on both sides")
    quality = 80 if quality is None else quality
    if type(quality) is not int or not 1 <= quality <= 100:
        raise ValueError("JPEG quality must be an integer in 1..100")
    return quality


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codec", choices=("mjpeg", "pyrowave"), required=True)
    parser.add_argument("--encoder", required=True)
    parser.add_argument("--decoder", required=True)
    parser.add_argument("--matrix", action="store_true")
    parser.add_argument("--dda", action="store_true")
    parser.add_argument("--width", type=int)
    parser.add_argument("--height", type=int)
    parser.add_argument("--fps", type=int, default=30)
    parser.add_argument("--frames", type=int, default=60)
    parser.add_argument("--transform", choices=("crop", "fit", "fill", "stretch"), default="fit")
    parser.add_argument("--test-seconds", type=int, default=8, help="receiver lifetime, 3..122s; DShow observes two seconds less")
    parser.add_argument("--encoder-hardware", type=int, choices=(0,), default=0)
    parser.add_argument("--decoder-hardware", type=int, choices=(0,), default=0)
    parser.add_argument("--cpu-convert", action="store_true", help="DDA conversion on CPU, not a fallback")
    parser.add_argument("--loss", action="store_true")
    parser.add_argument("--bitrate", type=int, default=20, help="Mbps")
    parser.add_argument("--max-bitrate", type=int, default=80, help="Mbps")
    parser.add_argument("--batch", type=int, default=1)
    parser.add_argument("--decode-in-flight", type=int, default=2)
    parser.add_argument("--max-au-bytes", type=int, default=8388608)
    parser.add_argument("--gop", type=int, default=1, help="encoded frames are independent; must be 1")
    parser.add_argument("--jpeg-quality", type=int, help="MJPEG quality 1..100; default 80")
    args = parser.parse_args()
    if args.loss and args.frames < 45: parser.error("loss diagnostic requires at least 45 frames")
    if (args.width is None) != (args.height is None) or args.matrix and args.width is not None:
        parser.error("provide width and height together, or use matrix, not both")
    cases = ([(416, 416), (640, 640), (1280, 720), (1920, 1080), (2560, 1440), (418, 282)]
             if args.matrix else [(416 if args.width is None else args.width, 416 if args.height is None else args.height)])
    try:
        codec_options(args.codec, args.encoder, args.decoder, bool(args.encoder_hardware), bool(args.decoder_hardware),
                      args.gop, args.jpeg_quality)
        for width, height in cases:
            validate_case(width, height, args.fps, args.frames, args.test_seconds, args.loss,
                          args.bitrate, args.max_bitrate, args.batch, args.decode_in_flight, args.max_au_bytes, args.gop)
    except ValueError as error:
        parser.error(str(error))
    output = ROOT / "run" / ("p6-codec-transport-" + datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + args.codec + "-" + uuid.uuid4().hex[:6])
    output.mkdir(parents=True)
    print(output, flush=True)
    results = []
    for width, height in cases:
        result = run_case(output, args.codec, args.encoder, args.decoder, width, height, fps=args.fps, frames=args.frames,
                          mode="dda" if args.dda else "pattern", loss=args.loss,
                          bitrate=args.bitrate, max_bitrate=args.max_bitrate, batch=args.batch,
                          decode_in_flight=args.decode_in_flight, max_au_bytes=args.max_au_bytes,
                          transform=args.transform, seconds=args.test_seconds, encoder_hardware=bool(args.encoder_hardware),
                          decoder_hardware=bool(args.decoder_hardware), gpu_convert=not args.cpu_convert, gop=args.gop,
                          jpeg_quality=args.jpeg_quality)
        results.append(result)
        (output / "matrix.json").write_text(json.dumps(results, indent=2), encoding="utf-8")
        print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
