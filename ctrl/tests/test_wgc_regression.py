"""Deterministic WGC resize/lifecycle regression, without a live capture session.

Builds ctrl/tests/native/wgc_regression.cpp against the real sender/wgc.h and
runs only pure helpers plus teardown on an uninitialized WGCCap. Nothing here
opens a window, registers the virtual camera, or touches Npcap.
"""
from pathlib import Path
import shutil
import subprocess
import os

import pytest

ROOT = Path(__file__).resolve().parents[2]


def _cppwinrt_include():
    root = Path("C:/Program Files (x86)/Windows Kits/10/Include")
    candidates = sorted((p for p in root.glob("*/cppwinrt") if (p / "winrt/base.h").is_file()),
                        key=lambda p: p.parent.name)
    return candidates[-1] if candidates else None


@pytest.fixture(scope="module")
def wgc_regression_exe(tmp_path_factory):
    cppwinrt = _cppwinrt_include()
    if not shutil.which("cmake") or cppwinrt is None:
        pytest.skip("MSVC, the Windows SDK cppwinrt headers, and cmake are required")
    tmp_path = tmp_path_factory.mktemp("wgc-regression")
    source = (ROOT / "ctrl/tests/native/wgc_regression.cpp").as_posix()
    (tmp_path / "CMakeLists.txt").write_text(f'''cmake_minimum_required(VERSION 3.20)
project(wgc_regression LANGUAGES CXX)
add_executable(wgc-regression "{source}")
add_executable(wgc-live "{ROOT.as_posix()}/ctrl/tests/native/wgc_live.cpp")
add_executable(wgc-live-window "{ROOT.as_posix()}/ctrl/tests/native/wgc_live_window.cpp")
target_compile_features(wgc-live-window PRIVATE cxx_std_17)
target_link_libraries(wgc-live-window PRIVATE user32 gdi32)
foreach(target IN ITEMS wgc-regression wgc-live)
target_compile_features(${{target}} PRIVATE cxx_std_17)
target_compile_options(${{target}} PRIVATE /utf-8)
target_compile_definitions(${{target}} PRIVATE NOMINMAX WIN32_LEAN_AND_MEAN)
target_include_directories(${{target}} PRIVATE
  "{ROOT.as_posix()}/sender" "{ROOT.as_posix()}/common" "{cppwinrt.as_posix()}")
target_link_libraries(${{target}} PRIVATE
  ws2_32 winmm d3d11 dxgi d3dcompiler ole32 oleaut32 windowsapp dwmapi)
endforeach()
''', encoding="utf-8")
    for command in (["cmake", "-S", str(tmp_path), "-B", str(tmp_path / "build"),
                     "-G", "Visual Studio 17 2022", "-A", "x64"],
                    ["cmake", "--build", str(tmp_path / "build"), "--config", "Release"]):
        result = subprocess.run(command, capture_output=True, timeout=180)
        assert result.returncode == 0, result.stdout + result.stderr
    return tmp_path / "build/Release/wgc-regression.exe"


def test_wgc_resize_content_and_lifecycle(wgc_regression_exe):
    result = subprocess.run([str(wgc_regression_exe)], capture_output=True, timeout=15)
    assert result.returncode == 0, result.stdout + result.stderr
    assert b"WGC regression: PASS" in result.stdout
    assert b"FAIL" not in result.stdout


@pytest.mark.parametrize("gpu", [False, True])
@pytest.mark.skipif(os.environ.get("VC_WGC_LIVE") != "1", reason="explicit VC_WGC_LIVE=1 required for private window capture")
def test_wgc_private_window_resize_close(wgc_regression_exe, gpu):
    directory = wgc_regression_exe.parent
    command = [str(directory / "wgc-live.exe"), str(directory / "wgc-live-window.exe")]
    if gpu:
        command.append("--gpu")
    result = subprocess.run(command, capture_output=True, timeout=30)
    if result.returncode in (2, 3):
        pytest.skip(result.stdout.decode("utf-8", errors="replace"))
    assert result.returncode == 0, result.stdout + result.stderr
    assert b"WGC-LIVE: PASS" in result.stdout
    print(result.stdout.decode("utf-8", errors="replace"))
