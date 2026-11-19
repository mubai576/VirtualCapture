"""Rust 便携包入口；参数原样交给唯一打包器。"""
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]

if __name__ == "__main__":
    raise SystemExit(subprocess.call([
        sys.executable, "-I", str(ROOT / "rust/packaging/package.py"), *sys.argv[1:]
    ], cwd=ROOT))
