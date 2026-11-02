"""Isolated product HTTP -> native Raw -> SHM -> DShow test; registry operations are replaced."""
from datetime import datetime
import importlib.util
import json
from pathlib import Path
import subprocess
import threading
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


def main():
    output = ROOT / "run" / ("p6-product-" + datetime.now().strftime("%Y%m%d-%H%M%S"))
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
    assert host.save_config({"adapter": r"\Device\NPF_Loopback", "peer_url": "http://127.0.0.1:5143",
                             "capture": {"width": 418, "height": 282, "fps": 30, "mode": "pattern",
                                         "pattern": "motion", "transform": "fill", "batch": 1}})[0] == 200
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
        samples = rows(output / "vc.jsonl")
        expected = {row["frame_id"]: row["hash"] for row in sent}
        assert len(expected) >= 90
        assert expected == {row["frame_id"]: row["hash"] for row in received}
        assert samples and all(row["hash"] in set(expected.values()) for row in samples)
        assert min(b["send_qpc_ns"] - a["send_qpc_ns"] for a, b in zip(sent, sent[1:])) >= 33333334
        result = {"pass": True, "size": [418, 282], "fps_limit": 30, "mode": "pattern", "pattern": "motion",
                  "batch": 1, "adapter": "Loopback", "sent": len(sent), "received": len(received),
                  "vcam_samples": len(samples), "session": rx["stream"], "registry_operations": registration,
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
