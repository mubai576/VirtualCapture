import os, importlib.util
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
_spec = importlib.util.spec_from_file_location("vc_ctrl", os.path.join(ROOT, "ctrl", "vc-ctrl.py"))
vc_ctrl = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(vc_ctrl)
r = vc_ctrl.run_case("http://127.0.0.1:18080", "http://127.0.0.1:18080", seconds=6,
                     pattern="motion", fps=240, mtu=9000, pacing_us=0, case="Tloopback",
                     expect_frames=480, adapter="Loopback", dst_mac="ff:ff:ff:ff:ff:ff")
print("LOOPBACK-VERDICT", r["verdict"], r["reasons"])
