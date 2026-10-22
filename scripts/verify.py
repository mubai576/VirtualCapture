"""verify.py - one-shot local gate for any change (no second PC needed).

Usage (from anywhere):
  python scripts/verify.py              # build + selftests + pattern_ref + pytest
  python scripts/verify.py --no-build   # skip cmake build (use existing build/Release exes)
  python scripts/verify.py --live Loopback   # also run live loopback pytest (VC_ADAPTER=<substr>)

Exit 0 only if every step passes. Steps print PASS/FAIL lines; first failure stops the run.
"""
import argparse, os, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def step(name, argv, env=None):
    print("== %s: %s" % (name, " ".join(argv)), flush=True)
    rc = subprocess.run(argv, cwd=ROOT, env=env).returncode
    print("%s %s (rc=%d)" % ("PASS" if rc == 0 else "FAIL", name, rc), flush=True)
    if rc != 0:
        sys.exit(rc)


def exe(name):
    # cmake VS generator writes build/Release/<name>; fall back to build/<name>.
    for p in (os.path.join(ROOT, "build", "Release", name), os.path.join(ROOT, "build", name)):
        if os.path.isfile(p):
            return p
    print("FAIL missing %s (build first: see docs/runbooks/build.md)" % name)
    sys.exit(2)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--live", default="", help="adapter substring for live loopback tests")
    a = ap.parse_args()

    if not a.no_build:
        step("cmake-configure", ["cmake", "-S", ".", "-B", "build"])
        step("cmake-build", ["cmake", "--build", "build", "--config", "Release"])
    step("sender-selftest", [exe("vc-sender.exe"), "--selftest"])
    step("receiver-selftest", [exe("vc-receiver.exe"), "--selftest"])
    step("pattern-ref", [sys.executable, "ctrl/pattern_ref.py"])
    env = dict(os.environ)
    if a.live:
        env["VC_ADAPTER"] = a.live
    step("pytest", [sys.executable, "-m", "pytest", "ctrl/tests", "-q"], env=env)
    print("ALL PASS")


if __name__ == "__main__":
    main()
