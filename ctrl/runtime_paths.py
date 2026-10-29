"""Locate binaries in both a source checkout and a portable package."""
from pathlib import Path


def find_binary(name, root=None):
    root = Path(root) if root is not None else Path(__file__).resolve().parents[1]
    candidates = (root / "build" / name, root / "build/Release" / name, root / name)
    return str(next((p for p in candidates if p.is_file()), candidates[-1]))
