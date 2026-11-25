"""OTA layout/dependency transactions in private roots; no live service changes."""
import importlib.util
import io
import json
import shutil
import subprocess
import os
import sys
import zipfile
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture
def agent(tmp_path, monkeypatch):
    spec = importlib.util.spec_from_file_location('ota_layout', ROOT / 'agent/vc-agent.py')
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    mod.ROOT = str(tmp_path)
    mod.BUILD_DIR = str(tmp_path / 'build')
    mod.RUN_DIR = str(tmp_path / 'run')
    mod.VERSION_FILE = str(tmp_path / 'version.json')
    (tmp_path / 'version.json').write_text('{"version":"old"}', encoding='utf-8')
    (tmp_path / 'agent').mkdir()
    (tmp_path / 'agent/vc-agent.py').write_text('# old', encoding='utf-8')
    monkeypatch.setattr(mod, 'kill_slot', lambda slot: None)
    return mod


def payload(agent, *, self_update=True, runtimes=True):
    files = {'version.json': json.dumps({'version': 'new'}),
             **{n: b'new-' + n.encode() for n in agent.UPDATE_NATIVE_FILES}}
    if self_update:
        files['vc-agent.py'] = '# new\n'
    if runtimes:
        files.update({n: b'new-' + n.encode() for n in agent.UPDATE_RUNTIME_DLLS})
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w') as z:
        for name, data in files.items():
            z.writestr(name, data)
    return buf.getvalue()


def put(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


@pytest.mark.parametrize('layout', ['', 'build', 'build/Release', 'mixed'])
def test_stage_activate_rollback_preserves_native_paths_and_runtime_copies(agent, tmp_path, layout):
    before = {}
    for name in agent.UPDATE_NATIVE_FILES:
        directory = 'build/Release' if layout == 'mixed' and name == 'vc-vcam.dll' else 'build' if layout == 'mixed' else layout
        path = tmp_path / directory / name
        before[path] = b'old-' + name.encode()
    # Reproduce PC2: build native images but DLLs exist only at ROOT.
    for name in agent.UPDATE_RUNTIME_DLLS:
        before[tmp_path / name] = b'old-' + name.encode()
    for path, data in before.items():
        put(path, data)
    slots = agent._update_slots()
    assert {Path(slots[n]) for n in agent.UPDATE_NATIVE_FILES} == {p for p in before if p.suffix == '.exe' or p.name == 'vc-vcam.dll'}
    code, result = agent.handle_update_binary(payload(agent))
    assert code == 200 and result['staged']
    for path, data in before.items():
        assert path.read_bytes() == data
    assert agent._do_activate('new', False)[0] == 200
    for name in agent.UPDATE_NATIVE_FILES:
        path = Path(slots[name])
        assert path.read_bytes() == b'new-' + name.encode()
        for dll in agent.UPDATE_RUNTIME_DLLS:
            assert (path.parent / dll).read_bytes() == b'new-' + dll.encode()
    assert agent._do_activate(None, True)[0] == 200
    for path, data in before.items():
        assert path.read_bytes() == data
    for name in agent.UPDATE_NATIVE_FILES:
        for dll in agent.UPDATE_RUNTIME_DLLS:
            path = Path(slots[name]).parent / dll
            if path not in before:
                assert not path.exists(), 'rollback must restore absence in every native directory'
    if not layout:
        assert not (tmp_path / 'build/vc-sender.exe').exists()


def test_runtime_only_update_fans_out_and_keeps_frozen_image_pointer(agent, tmp_path, monkeypatch):
    monkeypatch.setattr(sys, 'frozen', True, raising=False)
    image = tmp_path / 'vc-agent-new.exe'
    put(image, b'active-agent')
    monkeypatch.setattr(sys, 'executable', str(image))
    (tmp_path / 'version.json').write_text('{"version":"old","exe":"vc-agent-new.exe"}', encoding='utf-8')
    put(tmp_path / 'build/vc-sender.exe', b'sender')
    put(tmp_path / 'vc-receiver.exe', b'receiver')
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w') as z:
        z.writestr('version.json', '{"version":"old"}')
        z.writestr('libpyrowave-shared-1.dll', b'updated-runtime')
    code, result = agent.handle_update_binary(buf.getvalue())
    assert code == 200 and result['restart'] == 'none'
    assert (tmp_path / 'build/libpyrowave-shared-1.dll').read_bytes() == b'updated-runtime'
    assert (tmp_path / 'libpyrowave-shared-1.dll').read_bytes() == b'updated-runtime'
    assert json.loads((tmp_path / 'version.json').read_text())['exe'] == image.name


@pytest.mark.parametrize('self_update', [False, True])
def test_reject_native_update_without_app_local_dependencies(agent, tmp_path, self_update):
    put(tmp_path / 'build/vc-sender.exe', b'old')
    for name in agent.UPDATE_RUNTIME_DLLS:
        put(tmp_path / name, b'wrong-directory')
    code, result = agent.handle_update_binary(payload(agent, self_update=self_update, runtimes=False))
    assert code == 400 and result['missing']
    assert not (tmp_path / 'prev').exists()
    assert (tmp_path / 'build/vc-sender.exe').read_bytes() == b'old'


@pytest.mark.parametrize('self_update', [False, True])
def test_partial_copy_failure_restores_all_native_and_runtime_files(agent, tmp_path, monkeypatch, self_update):
    for name in (*agent.UPDATE_NATIVE_FILES, *agent.UPDATE_RUNTIME_DLLS):
        put(tmp_path / name, b'old-' + name.encode())
    original = shutil.copy2
    failed = False

    def copy(src, dst, *args, **kwargs):
        nonlocal failed
        if 'staging' in str(src) and Path(dst) == tmp_path / 'vc-receiver.exe' and not failed:
            failed = True
            raise OSError('injected native copy failure')
        return original(src, dst, *args, **kwargs)

    monkeypatch.setattr(shutil, 'copy2', copy)
    code, result = agent.handle_update_binary(payload(agent, self_update=self_update))
    if self_update:
        assert code == 200
        code, result = agent._do_activate('new', False)
    assert code == 500 and 'live files restored' in result['error']
    for name in (*agent.UPDATE_NATIVE_FILES, *agent.UPDATE_RUNTIME_DLLS):
        assert (tmp_path / name).read_bytes() == b'old-' + name.encode()


def test_legacy_prev_does_not_delete_unrecorded_msvc_dlls(agent, tmp_path):
    for name in agent.UPDATE_NATIVE_FILES:
        put(tmp_path / name, b'current-native')
    for name in agent.UPDATE_RUNTIME_DLLS[1:]:
        put(tmp_path / name, b'keep-runtime')
    prev = tmp_path / 'prev/mirror'
    put(prev / 'version.json', b'{"version":"legacy"}')
    put(prev / 'vc-agent.py', b'# legacy')
    for name in agent.UPDATE_NATIVE_FILES:
        put(prev / name, b'legacy-native')
    assert agent._do_activate(None, True)[0] == 200
    for name in agent.UPDATE_RUNTIME_DLLS[1:]:
        assert (tmp_path / name).read_bytes() == b'keep-runtime'


def test_private_runtime_snapshot_paths_are_not_allowed_in_upload(agent):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w') as z:
        z.writestr('version.json', '{"version":"new"}')
        z.writestr('_runtime/build/msvcp140.dll', b'bad')
    assert agent.handle_update_binary(buf.getvalue())[0] == 400


@pytest.mark.skipif(os.name != 'nt', reason='Windows DLL loader integration')
def test_real_portable_native_and_loader_survive_update_and_rollback(agent, tmp_path):
    source = ROOT / 'dist/VirtualCapture-0.5.1'
    needed = (*agent.UPDATE_NATIVE_FILES, *agent.UPDATE_RUNTIME_DLLS)
    if not all((source / name).is_file() for name in needed):
        pytest.skip('requires an existing complete portable build')
    for name in needed:
        shutil.copy2(source / name, tmp_path / name)
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w', zipfile.ZIP_DEFLATED) as z:
        z.writestr('version.json', '{"version":"new"}')
        z.writestr('vc-agent.py', '# isolated self update')
        for name in needed:
            z.write(source / name, name)
    assert agent.handle_update_binary(buf.getvalue())[0] == 200
    assert agent._do_activate('new', False)[0] == 200
    for _ in range(2):
        for kind in ('sender', 'receiver'):
            exe = Path(agent.exe_path(kind))
            assert exe.parent == tmp_path
            result = subprocess.run([str(exe), '--capabilities'], cwd=tmp_path, capture_output=True, text=True, timeout=15)
            assert result.returncode == 0, (result.returncode, result.stderr)
            assert 'raw' in json.loads(result.stdout)['codecs']
        if _ == 0:
            assert agent._do_activate(None, True)[0] == 200
    assert not (tmp_path / 'build/vc-sender.exe').exists()


def test_empty_frozen_layout_defaults_to_portable_root(agent, tmp_path, monkeypatch):
    monkeypatch.setattr(sys, 'frozen', True, raising=False)
    monkeypatch.setattr(sys, 'executable', str(tmp_path / 'vc-agent.exe'))
    slots = agent._update_slots()
    for name in (*agent.UPDATE_NATIVE_FILES, *agent.UPDATE_RUNTIME_DLLS):
        assert Path(slots[name]).parent == tmp_path
