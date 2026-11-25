"""Isolated real DShow NV12/MJPG negotiation and delivery; no registration/network."""
import mmap
import os
from pathlib import Path
import struct
import subprocess
import uuid

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.mark.skipif(os.name != "nt", reason="DirectShow requires Windows")
@pytest.mark.parametrize("width,height", [(640, 640), (418, 282), (2, 2)])
@pytest.mark.parametrize("format", ["nv12", "mjpeg"])
@pytest.mark.parametrize("blocking", [[], ["--block-receive"], ["--hold-samples", "--disconnect-running"]])
def test_client_selects_real_output_format(width, height, format, blocking, tmp_path):
    exe = ROOT / "build/vcam_verify/Release/vc-vcam-verify.exe"
    dll = ROOT / "build/Release/vc-vcam.dll"
    if not exe.is_file() or not dll.is_file():
        pytest.skip("native build required")
    name = "Local\\VCFormatTest-" + uuid.uuid4().hex
    size = width * height * 3 // 2
    with mmap.mmap(-1, 4096 + 12 * size, tagname=name) as mapping:
        mapping[:44] = struct.pack("<IIIIIiiIIII", 0x33414356, 3, 12, size, 4096, 0, 2,
                                   os.getpid(), width, height, 60)
        mapping[64:100] = struct.pack("<iIQQQI", 2, 1, 0, 0, 0, 0xFFFFFFFF)
        mapping[4096:4096 + size] = bytes([96]) * (width * height) + bytes([32, 208]) * (width * height // 4)
        result = subprocess.run([str(exe), "--dll", str(dll), "--shm", name,
                                 "--width", str(width), "--height", str(height), "--fps", "60",
                                 "--format", format, "--check-flat", "--seconds", "1", "--hashlog", str(tmp_path / "samples.jsonl"), *blocking],
                                capture_output=True, text=True, timeout=15)
    assert result.returncode == 0, result.stdout + result.stderr
    assert "format=" + ("MJPG" if format == "mjpeg" else "NV12") in result.stdout
    assert "invalid=0" in result.stdout
    # A valid SHM frame is required; the D12 repeat path must continue delivering that frame.
    assert len((tmp_path / "samples.jsonl").read_text().splitlines()) >= (1 if blocking else 2)
