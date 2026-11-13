"""Isolated product HTTP -> native Raw/codec -> SHM -> DShow test; registry operations are replaced."""
from datetime import datetime
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import threading
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p6_codec_verify import codec_options  # noqa: E402
from dshow_sequence import verify_dshow_progression  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codec", choices=("raw", "mjpeg", "pyrowave"), default="raw")
    parser.add_argument("--encoder", default="")
    parser.add_argument("--decoder", default="")
    parser.add_argument("--jpeg-quality", type=int)
    options = parser.parse_args()
    if options.codec != "raw" and (not options.encoder or not options.decoder):
        parser.error("encoded mode requires explicit --encoder and --decoder")
    jpeg = None
    if options.codec != "raw":
        try:
            jpeg = codec_options(options.codec, options.encoder, options.decoder, False, False, 1, options.jpeg_quality)
        except ValueError as error:
            parser.error(str(error))
    output = ROOT / "run" / ("p6-product-" + datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + options.codec + "-" + uuid.uuid4().hex[:6])
    output.mkdir(parents=True)
    mapping = "Local\\VCP6Product-" + uuid.uuid4().hex
    modules, registration = {}, []
    for role in ("host", "capture"):
        spec = importlib.util.spec_from_file_location("p6_product_" + role, ROOT / "agent/vc-agent.py")
        agent = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(agent)
        root = output / role
        root.mkdir()
        agent.ROOT, agent.RUN_DIR = str(root), str(root / "run")
        agent.BUILD_DIR = str(ROOT / "build")
        agent.state["role"] = role
        # Keep production config semantics while giving this diagnostic a private mapping.
        agent.CONFIG_DEFAULT["receive"]["shm"] = mapping
        modules[role] = agent
    host, capture = modules["host"], modules["capture"]

    def register():
        registration.append("register-replaced-no-registry-write")
        return 200, {"match": True}

    def unregister():
        registration.append("unregister-replaced-no-registry-write")
        host.state["vcam_managed"] = False
        return 200, {"reclaimed": True}

    host.vcam_register, host.vcam_unregister = register, unregister
    capture_server = capture.ThreadingHTTPServer(("127.0.0.1", 0), capture.H)
    capture_server.daemon_threads = True
    worker = threading.Thread(target=capture_server.serve_forever, daemon=True)
    worker.start()
    peer = host.peer_call
    def isolated_peer(path, body=None, **kwargs):
        kwargs["url"] = "http://127.0.0.1:%d" % capture_server.server_port
        return peer(path, body, **kwargs)
    host.peer_call = isolated_peer
    codec_config = {} if options.codec == "raw" else {"codec": options.codec, "codec_experimental": True,
        "encoder_backend": options.encoder, "decoder_backend": options.decoder, "encoder_hardware": False,
        "decoder_hardware": False, "bitrate_mbps": 20, "max_bitrate_mbps": 800, "gop_frames": 1, "decode_in_flight": 1,
        **({"jpeg_quality": jpeg} if jpeg is not None else {})}
    assert host.save_config({"adapter": r"\Device\NPF_Loopback", "peer_url": "http://127.0.0.1:5143",
                             "capture": {"width": 418, "height": 282, "fps": 30, "mode": "pattern",
                                         "pattern": "motion", "transform": "fill", "batch": 1, **codec_config}})[0] == 200
    verifier = None
    try:
        code, result = host.product_start()
        (output / "start.json").write_text(json.dumps(result, indent=2, ensure_ascii=False), encoding="utf-8")
        assert code == 200, result
        rx, tx = result["steps"][0]["result"], result["steps"][1]["result"]
        assert rx["stream"] == tx["stream"]
        with (output / "vc.log").open("wb") as log:
            verifier = subprocess.Popen([str(ROOT / "build/vcam_verify/Release/vc-vcam-verify.exe"), "--dll",
                str(ROOT / "build/Release/vc-vcam.dll"), "--shm", mapping, "--width", "418", "--height", "282",
                "--fps", "30", "--seconds", "3", "--hashlog", str(output / "vc.jsonl")],
                stdout=log, stderr=subprocess.STDOUT, creationflags=subprocess.CREATE_NO_WINDOW)
            assert verifier.wait(15) == 0, (output / "vc.log").read_text()
        capture.product_local_stop()
        time.sleep(.5)
        code, stopped = host.product_stop()
        assert code == 200 and stopped["ok"], stopped
        def rows(path):
            return [json.loads(line) for line in Path(path).read_text(encoding="utf-8").splitlines()]
        sent, received = rows(capture.state["sender"]["hashlog"]), rows(host.state["receiver"]["hashlog"])
        sender_stats = json.loads(Path(capture.state["sender"]["stats"]).read_text(encoding="utf-8"))
        if options.codec == "mjpeg":
            assert host.mjpeg_configuration_matches({"mjpeg_revision": 1,
                "encoder_configuration": sender_stats["encoder_configuration"]}, host.load_config()["capture"])
        elif options.codec == "pyrowave":
            assert host.pyrowave_configuration_matches({"pyrowave_revision": 1,
                "encoder_configuration": sender_stats["encoder_configuration"]}, host.load_config()["capture"], encoder=True)
        samples = rows(output / "vc.jsonl")
        sent_key = "hash" if options.codec == "raw" else "transport_hash"
        received_key = "hash" if options.codec == "raw" else "decoded_hash"
        expected = {row["frame_id"]: row[sent_key] for row in sent}
        assert len(expected) >= 90
        assert expected == {row["frame_id"]: row[sent_key] for row in received}
        dshow_progression = verify_dshow_progression(
            received, samples, decoded_hash_key=received_key, require_motion=True, fps=30,
            max_terminal_lag=max(1, 30 // 4))
        assert min(b["send_qpc_ns"] - a["send_qpc_ns"] for a, b in zip(sent, sent[1:])) >= 33333334
        result = {"pass": True, "codec": options.codec, "size": [418, 282], "fps_limit": 30, "mode": "pattern", "pattern": "motion",
                  "batch": 1, "adapter": "Loopback", "sent": len(sent), "received": len(received),
                  "vcam_samples": len(samples), "dshow_progression": dshow_progression,
                  "session": rx["stream"], "registry_operations": registration,
                  "encoder_configuration": sender_stats.get("encoder_configuration"),
                  "boundary": "private roots/map/random HTTP port; registry replaced; real native children and DShow"}
        (output / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
        print(output)
        print(json.dumps(result))
    finally:
        if verifier and verifier.poll() is None:
            verifier.terminate()
            verifier.wait(10)
        capture.product_local_stop()
        host.product_local_stop()
        capture_server.shutdown()
        capture_server.server_close()
        worker.join(10)


if __name__ == "__main__":
    main()
