"""Rust 本地总闸：格式、测试、Clippy、Release 与独立协议/图案参考。

默认不打开网卡或采集桌面；--live Loopback 显式增加本机 DLL 诊断，不能作为双机验收。
"""
import argparse
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = str(ROOT / "rust/Cargo.toml")


def step(name, argv):
    print(f"== {name}: {subprocess.list2cmdline([str(a) for a in argv])}", flush=True)
    code = subprocess.call([str(a) for a in argv], cwd=ROOT)
    print(f"{'PASS' if code == 0 else 'FAIL'} {name} (rc={code})", flush=True)
    if code:
        raise SystemExit(code)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-build", action="store_true", help="省略 Release 重建，其他门禁仍执行")
    parser.add_argument("--live", choices=["Loopback"], help="显式启用本机 Npcap Loopback 诊断")
    args = parser.parse_args()
    step("fmt", ["cargo", "fmt", "--manifest-path", MANIFEST, "--all", "--check"])
    step("rust-tests", ["cargo", "test", "--manifest-path", MANIFEST, "--workspace", "--locked"])
    step("clippy", ["cargo", "clippy", "--manifest-path", MANIFEST, "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"])
    if not args.no_build:
        step("release", ["cargo", "build", "--manifest-path", MANIFEST, "--workspace", "--release", "--locked"])
    step("pattern-reference", [sys.executable, "-I", ROOT / "rust/tests/reference/pattern_ref.py"])
    step("wire-vectors", [sys.executable, "-I", ROOT / "rust/tests/transport_vectors.py", "--skip-cargo"])
    step("dll-smoke-self-check", [sys.executable, "-I", ROOT / "rust/tests/dll_stream_smoke.py", "--self-check"])
    if args.live:
        step("dll-loopback", [sys.executable, "-I", ROOT / "rust/tests/dll_stream_smoke.py", "--package", ROOT / "rust/target/release"])
    print("ALL PASS（仅表示本次执行的 Rust 门禁，未执行的硬件/GUI验收不计入）", flush=True)


if __name__ == "__main__":
    main()
