"""Native watchdog regression, no GPU/driver fault or live deployment required."""
from pathlib import Path
import shutil
import subprocess
import time

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture(scope="module")
def deadline_exe(tmp_path_factory):
    if not shutil.which("cmake"):
        pytest.skip("CMake and MSVC required")
    work = tmp_path_factory.mktemp("operation-deadline")
    source = ROOT / "ctrl/tests/native/operation_deadline.cpp"
    (work / "CMakeLists.txt").write_text(f'''cmake_minimum_required(VERSION 3.20)
project(operation_deadline LANGUAGES CXX)
add_executable(operation-deadline "{source.as_posix()}")
target_compile_features(operation-deadline PRIVATE cxx_std_17)
target_compile_options(operation-deadline PRIVATE /utf-8)
''', encoding="utf-8")
    for command in (["cmake", "-S", str(work), "-B", str(work / "build"), "-G", "Visual Studio 17 2022", "-A", "x64"],
                    ["cmake", "--build", str(work / "build"), "--config", "Release"]):
        result = subprocess.run(command, capture_output=True, timeout=90)
        assert result.returncode == 0, result.stdout + result.stderr
    return work / "build/Release/operation-deadline.exe"


@pytest.fixture(scope="module")
def pyro_deadline_exe(tmp_path_factory):
    headers = ROOT / "build/_deps/pyrowave-src"
    if not shutil.which("cmake") or not (headers / "pyrowave.h").exists():
        pytest.skip("CMake/MSVC and configured PyroWave headers required")
    work = tmp_path_factory.mktemp("pyrowave-deadline")
    source = ROOT / "ctrl/tests/native/pyrowave_deadline.cpp"
    (work / "CMakeLists.txt").write_text(f'''cmake_minimum_required(VERSION 3.20)
project(pyrowave_deadline LANGUAGES CXX)
add_executable(pyrowave-deadline "{source.as_posix()}")
target_compile_features(pyrowave-deadline PRIVATE cxx_std_17)
target_compile_options(pyrowave-deadline PRIVATE /utf-8)
target_include_directories(pyrowave-deadline PRIVATE "{headers.as_posix()}" "{headers.as_posix()}/Granite/third_party/khronos/vulkan-headers/include")
''', encoding="utf-8")
    for command in (["cmake", "-S", str(work), "-B", str(work / "build"), "-G", "Visual Studio 17 2022", "-A", "x64"],
                    ["cmake", "--build", str(work / "build"), "--config", "Release"]):
        result = subprocess.run(command, capture_output=True, timeout=90)
        assert result.returncode == 0, result.stdout + result.stderr
    return work / "build/Release/pyrowave-deadline.exe"


def test_pyrowave_actual_wrapper_normal_calls(pyro_deadline_exe):
    result = subprocess.run([str(pyro_deadline_exe), "normal"], capture_output=True, timeout=5)
    assert result.returncode == 0, result.stdout + result.stderr


@pytest.mark.parametrize("operation", ["create", "encode", "packets", "decode", "reset", "destroy"])
def test_pyrowave_actual_wrapper_bounds_blocked_calls(pyro_deadline_exe, operation):
    start = time.monotonic()
    result = subprocess.run([str(pyro_deadline_exe), operation], capture_output=True, timeout=12)
    assert result.returncode == 124, result.stdout + result.stderr
    assert b"injected blocked" in result.stderr
    assert time.monotonic() - start < 11


def test_idle_success_exception_and_rearm_do_not_timeout(deadline_exe):
    result = subprocess.run([str(deadline_exe)], capture_output=True, timeout=10)
    assert result.returncode == 0, result.stdout + result.stderr


def test_blocked_operation_exits_without_waiting_for_unwind(deadline_exe):
    start = time.monotonic()
    result = subprocess.run([str(deadline_exe), "hang"], capture_output=True, timeout=5)
    assert result.returncode == 124
    assert time.monotonic() - start < 4
