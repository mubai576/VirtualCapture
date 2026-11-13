"""Native allocation/ownership fault injection without Npcap or a physical link."""
from pathlib import Path
import shutil
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture(scope="module")
def native_buffers(tmp_path_factory):
    tmp_path = tmp_path_factory.mktemp("mjpeg-native-buffers")
    library = ROOT / "build/_deps/libjpeg-turbo-build/Release/turbojpeg-static.lib"
    headers = ROOT / "build/_deps/libjpeg-turbo-3.1.2/src"
    if not library.exists() or not shutil.which("cmake"):
        pytest.skip("MSVC and the native MJPEG build are required")
    (tmp_path / "CMakeLists.txt").write_text(f'''cmake_minimum_required(VERSION 3.20)
project(mjpeg_buffer_regression LANGUAGES CXX)
foreach(kind IN ITEMS sender receiver)
  if(kind STREQUAL "sender")
    set(source "{ROOT.as_posix()}/ctrl/tests/native/mjpeg_buffers.cpp")
  else()
    set(source "{ROOT.as_posix()}/ctrl/tests/native/mjpeg_receiver_buffers.cpp")
  endif()
  add_executable(mjpeg-${{kind}}-buffers "${{source}}")
  target_compile_features(mjpeg-${{kind}}-buffers PRIVATE cxx_std_17)
  target_compile_options(mjpeg-${{kind}}-buffers PRIVATE /utf-8)
  target_compile_definitions(mjpeg-${{kind}}-buffers PRIVATE VC_HAS_MJPEG=1)
  target_include_directories(mjpeg-${{kind}}-buffers PRIVATE "{headers.as_posix()}" "{ROOT.as_posix()}/common" "{ROOT.as_posix()}/sender")
  target_link_libraries(mjpeg-${{kind}}-buffers PRIVATE "{library.as_posix()}" ws2_32 winmm d3d11 dxgi d3dcompiler mfplat mf mfuuid ole32 oleaut32 windowsapp dwmapi)
endforeach()
''', encoding="utf-8")
    for command in (["cmake", "-S", str(tmp_path), "-B", str(tmp_path / "build"), "-G", "Visual Studio 17 2022", "-A", "x64"],
                    ["cmake", "--build", str(tmp_path / "build"), "--config", "Release"]):
        result = subprocess.run(command, capture_output=True, timeout=90)
        assert result.returncode == 0, result.stdout + result.stderr
    return tmp_path


@pytest.mark.parametrize("kind, evidence", [("sender", b"no leaked queues"), ("receiver", b"next JPEG recovers exact pixels")])
def test_mjpeg_buffer_lifetimes_and_failure_cleanup(native_buffers, kind, evidence):
    result = subprocess.run([str(native_buffers / f"build/Release/mjpeg-{kind}-buffers.exe"), str(native_buffers)],
                            capture_output=True, timeout=15)
    assert result.returncode == 0, result.stdout + result.stderr
    assert evidence in result.stdout
