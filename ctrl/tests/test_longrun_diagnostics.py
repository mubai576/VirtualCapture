import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid

import pytest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('longrun', ROOT / 'scripts/longrun_diagnostics.py')
diagnostics = importlib.util.module_from_spec(spec); spec.loader.exec_module(diagnostics)


def sample(index=0, **kwargs):
    return dict(sample=index, utc='2026-11-08T00:00:00+00:00', monotonic_s=float(index), **kwargs)


def test_growth_identity_and_pending_detection():
    analyzer = diagnostics.Analyzer()
    process = dict(pid=55, created_100ns=1, private_bytes=20*diagnostics.MIB, handles=10, threads=2)
    assert not analyzer.observe(sample(processes=[process]))
    grown = dict(process, private_bytes=300*diagnostics.MIB, handles=600,
        native_timing={'lanes': [dict(kind='downstream_receive', pending_ms=999)]})
    events = analyzer.observe(sample(1, processes=[grown]))
    assert {event['type'] for event in events} == {'private_bytes_growth', 'handles_growth', 'native_call_pending'}
    assert all(event['context_samples'] == [0, 61] for event in events)
    assert not analyzer.observe(sample(2, processes=[grown]))
    recycled = dict(grown, created_100ns=2, native_timing={'lanes': []})
    events = analyzer.observe(sample(3, processes=[recycled]))
    assert [event['type'] for event in events] == ['process_exited_or_unobservable']


def test_static_dda_is_not_a_stall_or_cross_host_latency():
    analyzer = diagnostics.Analyzer()
    for second in range(10):
        row = sample(second, endpoints={'receiver': {'ok': True, 'stats': dict(
            session_id='a', running=True, complete=10, uptime_s=second, qpc_ns=2**60)}})
        assert not analyzer.observe(row)
    # A frozen stats producer is explicitly distinguished from a quiet frame source.
    assert analyzer.observe(sample(20, endpoints=row['endpoints']))[0]['type'] == 'stats_not_advancing'
    assert not analyzer.observe(sample(21, endpoints={'receiver': {'ok': False, 'error': 'offline'}}))


def test_rotation_markers_and_live_recorder(tmp_path, monkeypatch):
    class Source:
        closed = False
        def sample(self): return dict(system={'available_bytes': 40*diagnostics.MIB}, processes=[])
        def close(self): self.closed = True
    source = Source()
    monkeypatch.setattr(diagnostics, 'fetch_stats', lambda *a: {'ok': False, 'error': 'offline'})
    args = argparse.Namespace(output=tmp_path, interval=.01, seconds=.045,
        sender='http://192.168.2.59:5143', receiver='http://127.0.0.1:5143', pid=[], discovery_interval=15)
    diagnostics.record(args, source)
    summary = json.loads((tmp_path / 'summary.json').read_text())
    assert source.closed and summary['samples'] >= 2 and summary['source_errors'] == 0
    assert json.loads((tmp_path / 'events.jsonl').read_text())['type'] == 'low_available_memory'
    diagnostics.append_json(tmp_path / 'marks.jsonl', dict(diagnostics.stamp(), text='开始变卡'))
    assert json.loads((tmp_path / 'marks.jsonl').read_text(encoding='utf-8'))['text'] == '开始变卡'
    rotation = tmp_path / 'rotation'; rotation.mkdir()
    writer = diagnostics.JsonlWriter(rotation, samples_per_file=2)
    for i in range(5): writer.write(sample(i))
    writer.close()
    assert len(list(rotation.glob('samples-*.jsonl'))) == 3


@pytest.mark.skipif(os.name != 'nt', reason='Windows COM lifecycle')
def test_real_candidate_com_lifecycle():
    build = Path(os.environ.get('VC_NATIVE_BUILD_DIR', ROOT / 'build'))
    exe, dll = build / 'vcam_verify/Release/vc-vcam-lifecycle.exe', build / 'Release/vc-vcam.dll'
    if not exe.is_file() or not dll.is_file(): pytest.skip('native lifecycle build required')
    result = subprocess.run([str(exe), str(dll)], capture_output=True, text=True, timeout=30,
        env=dict(os.environ, VC_DIAG='0', VCAM_FRAME_MAP='Local'+chr(92)+'VC-Absent-'+uuid.uuid4().hex))
    assert result.returncode == 0, result.stdout+result.stderr
    assert '1000 create/release' in result.stdout and 'DllCanUnloadNow=S_OK' in result.stdout


def test_endpoint_validation_never_uses_proxy_or_dns():
    for url in ('https://192.168.2.59:5143', 'http://example.invalid', 'http://name:secret@127.0.0.1', 'http://127.0.0.1/mutate'):
        result = diagnostics.fetch_stats(url, 'receiver')
        assert not result['ok'] and result['elapsed_ms'] < 100
