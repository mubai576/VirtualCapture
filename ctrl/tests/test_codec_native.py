"""Codec development-tool validation; never infer product support from MFT enumeration."""
import json
from pathlib import Path
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.mark.parametrize("args", [[], ["--width"], ["--width", "416.5"], ["--width", "417"],
                                   ["--fps", "0"], ["--frames", "0"], ["--max-in-flight", "9"]])
def test_roundtrip_requires_explicit_valid_backend(args):
    exe = ROOT / "build/Release/vc-codec-roundtrip.exe"
    if not exe.exists(): pytest.skip("native build required")
    result = subprocess.run([str(exe), *args], capture_output=True, timeout=5)
    assert result.returncode == 2, result.stdout + result.stderr


def test_probe_is_not_a_stream_validation():
    exe = ROOT / "build/Release/vc-codec-probe.exe"
    if not exe.exists(): pytest.skip("native build required")
    result = subprocess.run([str(exe), "--fps", "60"], capture_output=True, timeout=30)
    assert result.returncode == 0, result.stderr
    report = json.loads(result.stdout)
    assert report["status"] == "candidates_only_not_stream_validated"
    for candidate in report["candidates"]:
        if "frames_tested" in candidate:
            assert candidate["frames_tested"] == 0 and candidate["stream_supported"] is False


def test_missing_mft_fails_without_fallback():
    exe = ROOT / "build/Release/vc-codec-roundtrip.exe"
    if not exe.exists(): pytest.skip("native build required")
    identity = "{00000001-0002-0003-0405-060708090a0b}"
    result = subprocess.run([str(exe), "--encoder-clsid", identity, "--decoder-clsid", identity],
                            capture_output=True, timeout=10)
    assert result.returncode == 1
    assert b"not uniquely present; no fallback" in result.stderr


def test_decoded_nv12_aperture_and_stride_selftest():
    exe = ROOT / "build/Release/vc-codec-roundtrip.exe"
    if not exe.exists(): pytest.skip("native build required")
    result = subprocess.run([str(exe), "--selftest"], capture_output=True, timeout=10)
    assert result.returncode == 0, result.stdout + result.stderr
