"""OTA升级验证：stage验包不重启，activate原地切换"""
"""stage只验不重启；activate原地exec；不起孤进程"""
import json
import os
import shutil as _tsh
import socket
import subprocess
import sys
import tempfile as _ttf
import time
import urllib.request
import urllib.error as _ue
import zipfile as _tzf

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

def _free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p

def _wait_health(base, timeout=15):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            with urllib.request.urlopen(base + "/health", timeout=3) as r:
                json.loads(r.read().decode()); return True
        except Exception:
            time.sleep(0.5)
    return False

def _wait_version(base, want, tries=12):
    for _ in range(tries):
        time.sleep(2)
        try:
            with urllib.request.urlopen(base + "/version", timeout=5) as r:
                if json.loads(r.read().decode()).get("version", "") == want:
                    return True
        except Exception:
            pass
    return False

def _post_start(base, path, body):
    import urllib.error as _e
    data = json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data,
                                 headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, json.loads(r.read().decode())
    except _e.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode())
        except Exception:
            return e.code, {}

def _ota_post_zip(base, path):
    with open(path, "rb") as f:
        data = f.read()
    req = urllib.request.Request(base + "/update", data=data,
                                    headers={"Content-Type": "application/zip"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, json.loads(r.read().decode() or "{}")
    except _ue.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode() or "{}")
        except Exception:
            return e.code, {}

def _ota_health(base):
    with urllib.request.urlopen(base + "/health", timeout=10) as r:
        return json.loads(r.read().decode())

def _ota_activate(base, body):
    code, res = _post_start(base, "/activate", body)
    assert code == 200, (code, res)
    assert res.get("restart") == "execv", res
    return res

def _assert_handover(proc, base, pid0, want):
    hp = _ota_health(base)
    assert hp.get("version") == want, hp
    assert proc.poll() is not None or hp.get("pid") == pid0
    return hp


def _ota_cleanup(proc, base, troot, extra=()):
    try:
        proc.terminate()
    except Exception:
        pass
    try:
        proc.wait(timeout=5)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass
    _pids = [proc.pid] + [int(x) for x in (extra or [])]
    for _pid in _pids:
        try:
            subprocess.run(['taskkill', '/PID', str(_pid), '/F'], capture_output=True, timeout=15)
        except Exception:
            pass
    import time as _tc
    for _r in range(3):
        try:
            with urllib.request.urlopen(base + '/health', timeout=5) as _hr:
                _hr.read()
        except Exception:
            break
        _tc.sleep(2)
        for _pid in _pids:
            try:
                subprocess.run(['taskkill', '/PID', str(_pid), '/F'], capture_output=True, timeout=15)
            except Exception:
                pass
    else:
        raise AssertionError('temp agent still listening on ' + base + '; refusing to rmtree a live root')
    try:
        _tsh.rmtree(troot)
    except Exception:
        _tc.sleep(2)
        _tsh.rmtree(troot, ignore_errors=True)


def _ota_mkpkg(path, ver, tag):
    src = os.path.join(ROOT, "agent", "vc-agent.py")
    with open(src, encoding="utf-8") as f:
        content = f.read()
    with _tzf.ZipFile(path, "w", _tzf.ZIP_DEFLATED) as zf:
        zf.writestr("version.json", json.dumps({"version": ver}))
        zf.writestr("vc-agent.py", content)
        zf.writestr("scripts/ok.ps1", tag)

def _ota_spawn(troot, startver):
    with open(os.path.join(troot, "version.json"), "w", encoding="utf-8") as f:
        f.write(json.dumps({"version": startver}))
    os.makedirs(os.path.join(troot, "agent"), exist_ok=True)
    _tsh.copy2(os.path.join(ROOT, "agent", "vc-agent.py"), os.path.join(troot, "agent", "vc-agent.py"))
    port = _free_port()
    agent = os.path.join(ROOT, "agent", "vc-agent.py")
    proc = subprocess.Popen([sys.executable, agent, "--role", "sender", "--port", str(port), "--root", troot], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = "http://127.0.0.1:%d" % port
    assert _wait_health(base, timeout=15), "agent up"
    return proc, base, port

def test_update_roundtrip_and_rollback():
    import pytest as _pt
    if os.name != "nt":
        _pt.skip("execv flow is Windows-first")
    troot = _ttf.mkdtemp(prefix="vc-upd-")
    proc, base, port = _ota_spawn(troot, "0.2.0")
    pid0 = proc.pid; seen = {pid0}; cur = pid0
    try:
        hp0 = _ota_health(base)
        assert hp0.get("pid") == proc.pid, (hp0, proc.pid)
        ver = "0.2.0"
        for i in range(10):
            nxt = "9.9.%d" % i
            zp = os.path.join(troot, "up.zip")
            _ota_mkpkg(zp, nxt, "r%d" % i)
            code, res = _ota_post_zip(base, zp)
            assert code == 200 and res.get("staged") is True and res.get("version") == nxt, (code, res)
            assert _ota_health(base).get("version") == ver, "stage must not apply"
            assert _ota_health(base).get("pid") == cur, "stage must not respawn"
            if i == 0:
                assert proc.poll() is None, "agent died during stage"
            pv = os.path.join(troot, "prev", "mirror", "version.json")
            assert json.loads(open(pv, encoding="utf-8").read()).get("version") == ver, "prev must mirror pre-stage version"
            t_gap0 = time.time()
            _ota_activate(base, {"version": nxt})
            assert _wait_version(base, nxt, tries=45), "activate did not take effect"
            gap = time.time() - t_gap0
            assert gap < 180, gap
            hp = _assert_handover(proc, base, cur, nxt)
            seen.add(hp.get("pid")); cur = hp.get("pid")
            assert os.path.isfile(os.path.join(troot, "scripts", "ok.ps1")), "staged files applied"
            ver = nxt
        code, res = _post_start(base, "/activate", {"rollback": True})
        assert code == 200, (code, res)
        assert _wait_version(base, "9.9.8", tries=45), "rollback did not take effect"
        hp = _assert_handover(proc, base, pid0, "9.9.8")
        seen.add(hp.get("pid"))
        assert os.path.isfile(os.path.join(troot, "run", "updates.log")), "audit trail missing"
    finally:
        _ota_cleanup(proc, base, troot, seen)

def test_stage_activate_rollback_preserves_pyrowave_runtime_dll():
    """OTA 运行时 DLL 必须随 stage 快照、activate 覆盖和 rollback 恢复（隔离临时根）。"""
    import pytest as _pt
    if os.name != 'nt':
        _pt.skip('execv flow is Windows-first')
    troot = _ttf.mkdtemp(prefix='vc-dll-')
    dll_live = os.path.join(troot, 'build', 'libpyrowave-shared-1.dll')
    os.makedirs(os.path.dirname(dll_live), exist_ok=True)
    old_dll = b'old-pyrowave-runtime'
    new_dll = b'new-pyrowave-runtime-with-new-abi'
    with open(dll_live, 'wb') as f:
        f.write(old_dll)
    proc, base, port = _ota_spawn(troot, '0.2.0')
    seen = {proc.pid}
    try:
        src = os.path.join(ROOT, 'agent', 'vc-agent.py')
        with open(src, encoding='utf-8') as f:
            agent_source = f.read()
        zp = os.path.join(troot, 'up-dll.zip')
        with _tzf.ZipFile(zp, 'w', _tzf.ZIP_DEFLATED) as zf:
            zf.writestr('version.json', json.dumps({'version': '9.9.98-dll'}))
            zf.writestr('vc-agent.py', agent_source)
            zf.writestr('libpyrowave-shared-1.dll', new_dll)
        code, res = _ota_post_zip(base, zp)
        assert code == 200 and res.get('staged') is True, (code, res)
        with open(dll_live, 'rb') as f:
            assert f.read() == old_dll, 'stage must not touch the live runtime DLL'
        prev_dll = os.path.join(troot, 'prev', 'mirror', 'libpyrowave-shared-1.dll')
        with open(prev_dll, 'rb') as f:
            assert f.read() == old_dll, 'prev snapshot must include the managed runtime DLL'
        _ota_activate(base, {'version': '9.9.98-dll'})
        assert _wait_version(base, '9.9.98-dll', tries=45)
        seen.add(_ota_health(base).get('pid'))
        with open(dll_live, 'rb') as f:
            assert f.read() == new_dll, 'activate must update the runtime DLL'
        code, res = _post_start(base, '/activate', {'rollback': True})
        assert code == 200, (code, res)
        assert _wait_version(base, '0.2.0', tries=45)
        seen.add(_ota_health(base).get('pid'))
        with open(dll_live, 'rb') as f:
            assert f.read() == old_dll, 'rollback must restore the pre-update runtime DLL'
    finally:
        _ota_cleanup(proc, base, troot, seen)


def test_rollback_removes_runtime_dll_introduced_by_update():
    """升级首次引入 DLL 时没有旧备份；rollback 必须删除它而不是留下新运行时。"""
    import pytest as _pt
    if os.name != 'nt':
        _pt.skip('execv flow is Windows-first')
    troot = _ttf.mkdtemp(prefix='vc-dlldel-')
    dll_live = os.path.join(troot, 'build', 'libpyrowave-shared-1.dll')
    proc, base, port = _ota_spawn(troot, '0.2.0')
    seen = {proc.pid}
    try:
        assert not os.path.exists(dll_live)
        src = os.path.join(ROOT, 'agent', 'vc-agent.py')
        with open(src, encoding='utf-8') as f:
            agent_source = f.read()
        zp = os.path.join(troot, 'up-dll-new.zip')
        with _tzf.ZipFile(zp, 'w', _tzf.ZIP_DEFLATED) as zf:
            zf.writestr('version.json', json.dumps({'version': '9.9.98-newdll'}))
            zf.writestr('vc-agent.py', agent_source)
            zf.writestr('libpyrowave-shared-1.dll', b'first-introduced-runtime')
        code, res = _ota_post_zip(base, zp)
        assert code == 200 and res.get('staged') is True, (code, res)
        assert not os.path.exists(os.path.join(troot, 'prev', 'mirror', 'libpyrowave-shared-1.dll'))
        _ota_activate(base, {'version': '9.9.98-newdll'})
        assert _wait_version(base, '9.9.98-newdll', tries=45)
        seen.add(_ota_health(base).get('pid'))
        assert os.path.isfile(dll_live)
        code, res = _post_start(base, '/activate', {'rollback': True})
        assert code == 200, (code, res)
        assert _wait_version(base, '0.2.0', tries=45)
        seen.add(_ota_health(base).get('pid'))
        assert not os.path.isfile(dll_live), 'rollback must remove a runtime DLL that had no previous version'
    finally:
        _ota_cleanup(proc, base, troot, seen)


def _fz_cleanup(proc, base, troot, seen):
    try:
        hp = _ota_health(base)
        if hp.get('pid'): seen.add(hp.get('pid'))
    except Exception:
        pass
    try:
        proc.terminate()
    except Exception:
        pass
    try:
        proc.wait(timeout=5)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass
    for _pid in [int(x) for x in seen]:
        try:
            subprocess.run(['taskkill', '/PID', str(_pid), '/F'], capture_output=True, timeout=15)
        except Exception:
            pass
    import time as _tc2
    for _r in range(3):
        try:
            with urllib.request.urlopen(base + '/health', timeout=5) as _hr:
                _hr.read()
        except Exception:
            break
        _tc2.sleep(2)
        for _pid in [int(x) for x in seen]:
            try:
                subprocess.run(['taskkill', '/PID', str(_pid), '/F'], capture_output=True, timeout=15)
            except Exception:
                pass
    else:
        raise AssertionError('temp agent still listening on ' + base + '; refusing to rmtree a live root')
    try:
        _tsh.rmtree(troot)
    except Exception:
        _tc2.sleep(2)
        _tsh.rmtree(troot, ignore_errors=True)

def test_frozen_roundtrip_and_rollback():
    import pytest as _pt
    if os.name != 'nt':
        _pt.skip('frozen execv flow is Windows-first')
    _exe = os.path.join(ROOT, 'dist', 'vc-agent.exe')
    if not os.path.isfile(_exe):
        _pt.skip('dist/vc-agent.exe not built')
    troot = _ttf.mkdtemp(prefix='vc-fz-')
    _live = os.path.join(troot, 'vc-agent.exe')
    _tsh.copy2(_exe, _live)
    with open(os.path.join(troot, 'version.json'), 'w', encoding='utf-8') as f:
        f.write(json.dumps({'version': '0.3.8'}))
    port = _free_port()
    proc = subprocess.Popen([_live, '--role', 'sender', '--port', str(port), '--root', troot], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = 'http://127.0.0.1:%d' % port
    seen = {proc.pid}
    try:
        assert _wait_health(base, timeout=30), 'frozen agent up'
        assert _ota_health(base).get('version') == '0.3.8'
        zp = os.path.join(troot, 'up.zip')
        with _tzf.ZipFile(zp, 'w', _tzf.ZIP_DEFLATED) as zf:
            zf.writestr('version.json', json.dumps({'version': '9.9.97-frozen'}))
            with open(_exe, 'rb') as _ef:
                zf.writestr('vc-agent.exe', _ef.read())
            zf.writestr('scripts/ok.ps1', 'Write-Host fz-ok')
        code, res = _ota_post_zip(base, zp)
        assert code == 200 and res.get('staged') is True, (code, res)
        assert _ota_health(base).get('version') == '0.3.8', 'stage must not apply'
        code, res = _post_start(base, '/activate', {'version': 'wrong'})
        assert code == 409, (code, res)
        assert _ota_health(base).get('version') == '0.3.8'
        _ota_activate(base, {'version': '9.9.97-frozen'})
        assert _wait_version(base, '9.9.97-frozen', tries=45), 'frozen activate stalled'
        hp = _ota_health(base)
        assert hp.get('version') == '9.9.97-frozen', hp
        seen.add(hp.get('pid'))
        import glob as _gg
        assert _gg.glob(os.path.join(troot, 'vc-agent-*.exe')), 'no versioned exe'
        assert json.loads(open(os.path.join(troot, 'version.json'), encoding='utf-8').read()).get('exe', '').startswith('vc-agent-'), 'exe pointer missing'
        zp2 = os.path.join(troot, 'up2.zip')
        with _tzf.ZipFile(zp2, 'w', _tzf.ZIP_DEFLATED) as _zf2:
            _zf2.writestr('version.json', json.dumps({'version': '9.9.97-frozen'}))
            with open(_exe, 'rb') as _ef2:
                _zf2.writestr('vc-agent.exe', _ef2.read())
            _zf2.writestr('scripts/ok.ps1', 'Write-Host fz-ok2')
        code, res = _ota_post_zip(base, zp2)
        assert code == 200 and res.get('staged') is True, (code, res)
        _ota_activate(base, {'version': '9.9.97-frozen'})
        assert _wait_version(base, '9.9.97-frozen', tries=45), 'same-version frozen re-apply stalled'
        hp2 = _ota_health(base)
        assert hp2.get('version') == '9.9.97-frozen'
        seen.add(hp2.get('pid'))
        assert hp2.get('pid') != hp.get('pid'), 're-apply must hand over to a fresh file'
        assert len(_gg.glob(os.path.join(troot, 'vc-agent-*.exe'))) >= 2, 'expected versioned plus unique exe'
        code, res = _post_start(base, '/activate', {'rollback': True})
        assert code == 200, (code, res)
        assert _wait_version(base, '0.3.8', tries=45), 'frozen rollback stalled'
        assert os.path.isfile(os.path.join(troot, 'run', 'updates.log')), 'audit trail missing'
    finally:
        _fz_cleanup(proc, base, troot, seen)

def test_same_version_keeps_prev():
    import pytest as _pt
    if os.name != 'nt':
        _pt.skip('execv flow is Windows-first')
    troot = _ttf.mkdtemp(prefix='vc-same-')
    proc, base, port = _ota_spawn(troot, '0.2.0')
    seen = {proc.pid}
    try:
        zp = os.path.join(troot, 'up.zip')
        _ota_mkpkg(zp, '1.0.0', 'first')
        code, res = _ota_post_zip(base, zp)
        assert code == 200 and res.get('staged') is True, (code, res)
        _ota_activate(base, {'version': '1.0.0'})
        assert _wait_version(base, '1.0.0', tries=45)
        seen.add(_ota_health(base).get('pid'))
        _pv = os.path.join(troot, 'prev', 'mirror', 'version.json')
        assert json.loads(open(_pv, encoding='utf-8').read()).get('version') == '0.2.0'
        _ota_mkpkg(zp, '1.0.0', 'second')
        code, res = _ota_post_zip(base, zp)
        assert code == 200 and res.get('staged') is True, (code, res)
        _ota_activate(base, {'version': '1.0.0'})
        assert _wait_version(base, '1.0.0', tries=45)
        seen.add(_ota_health(base).get('pid'))
        assert json.loads(open(_pv, encoding='utf-8').read()).get('version') == '0.2.0', 'same-version re-apply must keep older prev'
        code, res = _post_start(base, '/activate', {'rollback': True})
        assert code == 200, (code, res)
        assert _wait_version(base, '0.2.0', tries=45), 'rollback should reach pre-1.0.0'
        seen.add(_ota_health(base).get('pid'))
    finally:
        _ota_cleanup(proc, base, troot, seen)

def test_run_remote_cli_update_and_rollback():
    import pytest as _pt
    if os.name != 'nt':
        _pt.skip('execv flow is Windows-first')
    import importlib.util as _ilu
    _spec = _ilu.spec_from_file_location('run_remote', os.path.join(ROOT, 'ctrl', 'run_remote.py'))
    _rr = _ilu.module_from_spec(_spec); _spec.loader.exec_module(_rr)
    troot = _ttf.mkdtemp(prefix='vc-rr-')
    proc, base, port = _ota_spawn(troot, '0.2.0')
    seen = {proc.pid}
    try:
        zp = os.path.join(troot, 'up.zip')
        _ota_mkpkg(zp, '7.7.7-rr', 'rr')
        import sys as _sys
        _old = _sys.argv
        try:
            _sys.argv = ['run_remote.py', '--agent', base, '--update', zp, '--force']
            try:
                _code = _rr.main()
            except SystemExit as _e:
                _code = _e.code
            assert _code == 0, _code
        finally:
            _sys.argv = _old
        assert _wait_version(base, '7.7.7-rr', tries=45)
        seen.add(_ota_health(base).get('pid'))
        try:
            _sys.argv = ['run_remote.py', '--agent', base, '--rollback']
            try:
                _code = _rr.main()
            except SystemExit as _e:
                _code = _e.code
            assert _code == 0, _code
        finally:
            _sys.argv = _old
        assert _wait_version(base, '0.2.0', tries=45)
        seen.add(_ota_health(base).get('pid'))
    finally:
        _ota_cleanup(proc, base, troot, seen)

def test_stage_rejects_unmappable_self_update():
    import pytest as _pt
    if os.name != 'nt':
        _pt.skip('execv flow is Windows-first')
    troot = _ttf.mkdtemp(prefix='vc-400-')
    proc, base, port = _ota_spawn(troot, '0.2.0')
    seen = {proc.pid}
    try:
        zp = os.path.join(troot, 'up.zip')
        with _tzf.ZipFile(zp, 'w', _tzf.ZIP_DEFLATED) as zf:
            zf.writestr('version.json', json.dumps({'version': '9.9.99-bad'}))
            zf.writestr('vc-agent.exe', 'not-a-real-exe')
        code, res = _ota_post_zip(base, zp)
        assert code == 400, (code, res)
        assert _ota_health(base).get('version') == '0.2.0'
        assert proc.poll() is None, 'agent must survive rejected stage'
    finally:
        _ota_cleanup(proc, base, troot, seen)

def test_files_pagination_roundtrip():
    import pytest as _pt
    if os.name != 'nt':
        _pt.skip('files pagination is Windows-first')
    import urllib.error as _ue2
    troot = _ttf.mkdtemp(prefix='vc-pg-')
    proc, base, port = _ota_spawn(troot, '0.3.8')
    seen = {proc.pid}
    try:
        blob = bytes((i % 26) + 65 for i in range(300000))
        with open(os.path.join(troot, 'big.bin'), 'wb') as f:
            f.write(blob)
        want = blob.decode('ascii')
        with urllib.request.urlopen(base + '/files?path=big.bin', timeout=15) as r:
            res0 = json.loads(r.read().decode())
        assert res0.get('truncated') is True and len(res0.get('content', '')) == 262144, res0
        got = []
        off = 0
        for _ in range(10):
            with urllib.request.urlopen(base + ('/files?path=big.bin&offset=%d&limit=100000' % off), timeout=15) as r:
                res = json.loads(r.read().decode())
            assert res.get('size') == 300000, res
            got.append(res.get('content', ''))
            if res.get('next_offset') is None:
                break
            off = int(res['next_offset'])
        else:
            assert False, 'paging did not terminate'
        assert ''.join(got) == want, 'paged bytes must reassemble exactly'
        try:
            with urllib.request.urlopen(base + '/files?path=big.bin&offset=abc', timeout=15) as r:
                _ = r.read()
            assert False, 'bad offset should 400'
        except _ue2.HTTPError as e:
            assert e.code == 400, e.code
    finally:
        _ota_cleanup(proc, base, troot, seen)
