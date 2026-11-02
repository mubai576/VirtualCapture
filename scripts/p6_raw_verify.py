"""Isolated motion/Raw/SHM/DShow loopback matrix; never registers a DLL or calls a resident agent."""
import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


def run_case(output, width, height, fps, mode="pattern", transform="crop", gpu=False):
    from vc_shm import ShmReader
    case = output / f"{mode}-{width}x{height}-{fps}-{transform}-{'gpu' if gpu else 'cpu'}"
    case.mkdir()
    name = "Local\\VCP6-" + uuid.uuid4().hex
    token = uuid.uuid4().hex[:10]
    mac = "02:" + ":".join(token[i:i+2] for i in range(0, 10, 2))
    processes, logs = [], []

    def spawn(argv, log):
        handle = open(case / log, "wb")
        logs.append(handle)
        process = subprocess.Popen(list(map(str, argv)), stdout=handle, stderr=subprocess.STDOUT,
                                   creationflags=subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW)
        processes.append(process)
        return process

    def rows(path):
        return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]

    try:
        receiver = spawn([ROOT / "build/vc-receiver.exe", "--adapter", "Loopback", "--width", width,
                          "--height", height, "--fps", fps, "--source-mac", mac, "--timeout-s", 8,
                          "--shm", name, "--stats", case / "rx.json", "--hashlog", case / "rx.jsonl"], "rx.log")
        deadline = time.monotonic() + 3
        while True:
            if receiver.poll() is not None: raise RuntimeError((case / "rx.log").read_text())
            try:
                reader = ShmReader(name)
                break
            except OSError:
                if time.monotonic() >= deadline: raise
                time.sleep(.05)
        try:
            assert (reader.width, reader.height, reader.fps) == (width, height, fps)
        finally:
            reader.close()
        verifier = spawn([ROOT / "build/vcam_verify/Release/vc-vcam-verify.exe", "--dll",
                          ROOT / "build/Release/vc-vcam.dll", "--shm", name, "--width", width,
                          "--height", height, "--fps", fps, "--seconds", 6,
                          "--hashlog", case / "vc.jsonl"], "vc.log")
        time.sleep(.2)
        command = [ROOT / "build/vc-sender.exe", "--adapter", "Loopback", "--mode", mode,
                   "--pattern", "motion", "--width", width, "--height", height, "--fps", fps,
                   "--transform", transform, "--frames", fps*3, "--batch", 1, "--source-mac", mac,
                   "--stats", case / "tx.json", "--hashlog", case / "tx.jsonl"]
        if gpu: command.append("--gpu-convert")
        sender = spawn(command, "tx.log")
        assert sender.wait(30) == 0, (case / "tx.log").read_text()
        assert verifier.wait(15) == 0, (case / "vc.log").read_text()
        assert receiver.wait(15) == 0, (case / "rx.log").read_text()
        sent, received, video = rows(case / "tx.jsonl"), rows(case / "rx.jsonl"), rows(case / "vc.jsonl")
        expected = {row["frame_id"]: row["hash"] for row in sent}
        actual = {row["frame_id"]: row["hash"] for row in received}
        assert len(expected) == len(sent) == fps*3 and expected == actual
        hashes = set(expected.values())
        assert video and all(row["hash"] in hashes for row in video)
        intervals = [b["send_qpc_ns"]-a["send_qpc_ns"] for a, b in zip(sent, sent[1:])]
        assert min(intervals) >= (10**9+fps-1)//fps
        stats = json.loads((case / "rx.json").read_text())
        assert stats["dropped"] == stats["hash_mismatch"] == 0
        rate = (len(sent)-1)*1e9 / (sent[-1]["send_qpc_ns"]-sent[0]["send_qpc_ns"])
        result = {"size": [width, height], "fps_limit": fps, "mode": mode, "pattern": "motion" if mode == "pattern" else None,
                  "batch": 1, "adapter": "Loopback", "transform": transform, "gpu_convert": gpu,
                  "sent": len(sent), "received": len(received), "vcam_samples_including_stall_repeats": len(video),
                  "vcam_distinct_hashes": len({row["hash"] for row in video}),
                  "minimum_send_interval_ns": min(intervals), "measured_send_fps": rate, "pass": True}
        (case / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
        return result
    finally:
        # Only the exact processes created here, never a product agent/test window owned by the user.
        for process in reversed(processes):
            if process.poll() is None: process.terminate()
            process.wait(timeout=10)
        for handle in logs: handle.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dda", action="store_true", help="requires a changing desktop; does not create/modify desktop windows")
    args = parser.parse_args()
    if os.name != "nt": raise SystemExit("Windows/Npcap is required")
    output = ROOT / "run" / ("p6-raw-" + datetime.now().strftime("%Y%m%d-%H%M%S"))
    output.mkdir(parents=True)
    results = []
    cases = [(416,416,60), (640,640,240), (418,282,120), (1280,720,60),
             (1920,1080,30), (2560,1440,15), (2,2,30)] if not args.dda else [(416,416,10), (418,282,30), (1280,720,60)]
    print(output, flush=True)
    for width, height, fps in cases:
        result = run_case(output, width, height, fps, "dda" if args.dda else "pattern",
                          "fit" if args.dda else "crop", args.dda)
        results.append(result)
        (output / "matrix.json").write_text(json.dumps(results, indent=2), encoding="utf-8")
        print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
