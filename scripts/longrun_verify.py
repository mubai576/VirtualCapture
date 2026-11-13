"""Isolated motion/Loopback lifecycle and blocking regression; never registers or deploys a DLL."""
import argparse
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


def rows(path):
    return [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines()]


def run_case(build, output, name, flags, enabled=True):
    from vc_shm import ShmReader
    from longrun_windows import WindowsSource
    case = output / name; case.mkdir()
    mapping = 'Local'+chr(92)+'VCLongrun-'+uuid.uuid4().hex
    mac = '02:'+':'.join(uuid.uuid4().hex[i:i+2] for i in range(0, 10, 2))
    processes, logs = [], []
    environment = dict(os.environ, VC_DIAG='1' if enabled else '0')

    def spawn(command, log):
        handle = (case / log).open('wb'); logs.append(handle)
        process = subprocess.Popen(list(map(str, command)), stdout=handle, stderr=subprocess.STDOUT,
            env=environment, creationflags=subprocess.CREATE_NO_WINDOW | subprocess.CREATE_NEW_PROCESS_GROUP)
        processes.append(process); return process

    source = None
    try:
        receiver = spawn([build / 'Release/vc-receiver.exe', '--adapter', 'Loopback', '--timeout-s', 8,
            '--source-mac', mac, '--shm', mapping, '--stats', case / 'rx.json', '--hashlog', case / 'rx.jsonl'], 'rx.log')
        deadline = time.monotonic()+4
        while True:
            if receiver.poll() is not None: raise RuntimeError((case / 'rx.log').read_text())
            try:
                reader = ShmReader(mapping); reader.close(); break
            except OSError:
                if time.monotonic() > deadline: raise
                time.sleep(.05)
        verifier = spawn([build / 'vcam_verify/Release/vc-vcam-verify.exe', '--dll', build / 'Release/vc-vcam.dll',
            '--shm', mapping, '--seconds', 5, '--hashlog', case / 'vc.jsonl', *flags], 'vc.log')
        time.sleep(.1)
        sender = spawn([build / 'Release/vc-sender.exe', '--adapter', 'Loopback', '--mode', 'pattern',
            '--pattern', 'motion', '--fps', 240, '--frames', 720, '--batch', 1, '--source-mac', mac,
            '--stats', case / 'tx.json', '--hashlog', case / 'tx.jsonl'], 'tx.log')
        source = WindowsSource([receiver.pid, verifier.pid], discovery_interval=1)
        pending, timings, discovered = set(), set(), False
        deadline = time.monotonic()+15
        with (case / 'resources.jsonl').open('w', encoding='utf-8') as samples:
            while receiver.poll() is None:
                snapshot = source.sample()
                snapshot['monotonic_s'] = time.monotonic()
                samples.write(json.dumps(snapshot)+chr(10)); samples.flush()
                for process in snapshot['processes']:
                    if process['pid'] == verifier.pid and process.get('vcam_dll'): discovered = True
                    for lane in process.get('native_timing', {}).get('lanes', []):
                        if lane['count']: timings.add(lane['kind'])
                        if lane['pending_ms'] > 100: pending.add(lane['kind'])
                if time.monotonic() > deadline: raise TimeoutError('isolated case did not finish')
                time.sleep(.2)
        for process, log in ((sender,'tx.log'), (verifier,'vc.log'), (receiver,'rx.log')):
            assert process.wait(5) == 0, (case / log).read_text(errors='replace')
        sent, received, video = rows(case / 'tx.jsonl'), rows(case / 'rx.jsonl'), rows(case / 'vc.jsonl')
        expected = {row['frame_id']: row['hash'] for row in sent}
        actual = {row['frame_id']: row['hash'] for row in received}
        assert len(sent) == len(expected) == len(received) == len(actual) == 720 and expected == actual
        hashes = set(expected.values())
        assert video and all(row['hash'] in hashes for row in video)
        stats = json.loads((case / 'rx.json').read_text())
        assert stats['dropped'] == stats['hash_mismatch'] == 0
        assert discovered, 'downstream DLL automatic discovery failed'
        if enabled:
            assert {'stats_write','hash_write_flush','stats_prepare','sample_getbuffer'} <= timings
            if '--block-receive' in flags: assert 'downstream_receive' in pending
            elif '--hold-samples' in flags: assert 'sample_getbuffer' in pending
            else: assert 'downstream_receive' in timings
        else: assert not timings and not pending
        result = dict(case=name, adapter='Loopback', pattern='motion', batch=1, codec='raw',
            frames_sent=len(sent), frames_received=len(received), missing=0, duplicate_received=len(received)-len(actual),
            vcam_samples=len(video), vcam_distinct_hashes=len({row['hash'] for row in video}),
            vcam_repeated_hash_samples=len(video)-len({row['hash'] for row in video}),
            native_timings=sorted(timings), pending_calls_observed=sorted(pending), pass_result=True,
            stop_evidence=(case / 'vc.log').read_text(errors='replace'))
        (case / 'result.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
        return result
    finally:
        if source: source.close()
        for process in reversed(processes):
            if process.poll() is None: process.terminate()
            process.wait(10)
        for handle in logs: handle.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build', type=Path, default=ROOT / 'build')
    parser.add_argument('--output', type=Path, default=ROOT / 'run' / ('longrun-verify-'+datetime.now().strftime('%Y%m%d-%H%M%S')))
    args = parser.parse_args()
    build, output = args.build.resolve(), args.output.resolve(); output.mkdir(parents=True)
    lifecycle = subprocess.run([str(build / 'vcam_verify/Release/vc-vcam-lifecycle.exe'), str(build / 'Release/vc-vcam.dll')],
        capture_output=True, text=True, timeout=30, env=dict(os.environ, VC_DIAG='0', VCAM_FRAME_MAP='Local'+chr(92)+'VC-Absent-'+uuid.uuid4().hex))
    (output / 'lifecycle.log').write_text(lifecycle.stdout+lifecycle.stderr, encoding='utf-8')
    assert lifecycle.returncode == 0, lifecycle.stdout+lifecycle.stderr
    result = []
    for name, flags, enabled in [('normal', [], True), ('normal-disabled', [], False),
        ('receive-stop', ['--block-receive'], True), ('receive-disconnect', ['--block-receive','--disconnect-running'], True),
        ('allocator-stop', ['--hold-samples'], True), ('allocator-disconnect', ['--hold-samples','--disconnect-running'], True)]:
        result.append(run_case(build, output, name, flags, enabled))
        (output / 'results.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
        print(json.dumps(result[-1]), flush=True)
    hashes = {str(path.relative_to(build)): hashlib.sha256(path.read_bytes()).hexdigest() for path in (
        build / 'Release/vc-vcam.dll', build / 'Release/vc-receiver.exe', build / 'Release/vc-sender.exe',
        build / 'vcam_verify/Release/vc-vcam-verify.exe', build / 'vcam_verify/Release/vc-vcam-lifecycle.exe')}
    (output / 'binaries.json').write_text(json.dumps(hashes, indent=2), encoding='utf-8')
    print(f'ALL PASS: {output}', flush=True)


if __name__ == '__main__': main()
