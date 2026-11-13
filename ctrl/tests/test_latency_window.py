"""Bounded encoded latency samples: unknown before frames, true zero, rolling eviction."""
from pathlib import Path
import json
import shutil
import subprocess
import pytest

ROOT = Path(__file__).resolve().parents[2]


def test_native_latency_window(tmp_path):
    if not shutil.which('cmake'):
        pytest.skip('CMake and MSVC required')
    source = ROOT / 'ctrl/tests/native/latency_window.cpp'
    (tmp_path / 'CMakeLists.txt').write_text(f'''cmake_minimum_required(VERSION 3.20)
project(latency_window LANGUAGES CXX)
add_executable(latency-window "{source.as_posix()}")
target_compile_features(latency-window PRIVATE cxx_std_17)
''', encoding='utf-8')
    for command in (["cmake", "-S", str(tmp_path), "-B", str(tmp_path / 'build'), '-G', 'Visual Studio 17 2022', '-A', 'x64'],
                    ['cmake', '--build', str(tmp_path / 'build'), '--config', 'Release']):
        r = subprocess.run(command, capture_output=True, timeout=90)
        assert r.returncode == 0, r.stdout + r.stderr
    r = subprocess.run([str(tmp_path / 'build/Release/latency-window.exe')], capture_output=True, timeout=10)
    assert r.returncode == 0, r.stdout + r.stderr
    empty, zero, rolling = map(json.loads, r.stdout.decode().splitlines())
    assert empty['rseg_samples'] == 0 and empty['rseg_p99_ms'] is None
    assert zero['rseg_samples'] == 1 and zero['rseg_p99_ms'] == 0
    assert rolling['pc1_seg_samples'] == 10000
    assert rolling['pc1_seg_avg_ms'] == 5020.5
    assert rolling['pc1_seg_p50_ms'] == 5021
    assert rolling['pc1_seg_p99_ms'] == 9921
    assert rolling['pc1_seg_max_ms'] == 10020
