"""Read-only six-hour PC1 Raw diagnostic recorder. Never starts/stops capture or changes a device."""
import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import sys
import time
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
MIB = 1024 ** 2


def stamp():
    return {'utc': datetime.now(timezone.utc).isoformat(), 'monotonic_s': time.monotonic()}


def fetch_stats(base, kind, timeout=.4):
    """Direct GET with no proxy, credentials, mutation or unbounded response body."""
    started = time.monotonic()
    conn = None
    try:
        url = urlsplit(base)
        if url.scheme != 'http' or url.username or url.password or url.path not in ('', '/') or url.query or url.fragment:
            raise ValueError('expected plain http://numeric-IP:port')
        socket.inet_pton(socket.AF_INET, url.hostname) # No DNS can block the sampling thread.
        conn = http.client.HTTPConnection(url.hostname, url.port or 5143, timeout=timeout)
        conn.request('GET', f'/{kind}/stats')
        response = conn.getresponse()
        if response.status != 200: raise ValueError(f'HTTP {response.status}')
        data = response.read(262145)
        if len(data) > 262144: raise ValueError('response exceeds 256 KiB')
        document = json.loads(data)
        if not isinstance(document, dict): raise ValueError('stats must be an object')
        return {'ok': True, 'stats': document, 'elapsed_ms': (time.monotonic()-started)*1000}
    except (OSError, ValueError, http.client.HTTPException) as exc:
        return {'ok': False, 'error': f'{type(exc).__name__}: {exc}', 'elapsed_ms': (time.monotonic()-started)*1000}
    finally:
        if conn: conn.close()


class Analyzer:
    """Heuristics label evidence windows; they do not replace frozen acceptance gates."""
    def __init__(self):
        self.processes, self.cooldowns, self.stats_seen = {}, {}, {}
        self.kernel_baseline = None

    def observe(self, sample):
        now, events = sample['monotonic_s'], []

        def emit(kind, subject, value, reference=None):
            key = (kind, subject)
            if now - self.cooldowns.get(key, -1e30) < 60: return
            self.cooldowns[key] = now
            events.append(dict(type=kind, subject=subject, value=value, baseline=reference,
                sample=sample['sample'], utc=sample['utc'], monotonic_s=now,
                context_samples=[max(0, sample['sample']-60), sample['sample']+60]))

        live = set()
        for process in sample.get('processes', []):
            identity = f"{process['pid']}:{process.get('created_100ns', 'unknown')}"
            live.add(identity)
            baseline = self.processes.setdefault(identity, process)
            # GPU counters can warm up after the first process observation.
            if not baseline.get('gpu') and process.get('gpu'): baseline['gpu'] = dict(process['gpu'])
            for key in ('private_bytes', 'handles', 'threads'):
                if key not in baseline and key in process: baseline[key] = process[key]
            for key, threshold in [('private_bytes', 256*MIB), ('handles', 512), ('threads', 32)]:
                current, first = process.get(key), baseline.get(key)
                if current is not None and first is not None and current-first >= threshold:
                    emit(key+'_growth', identity, current, first)
            for key in ('gpu_dedicated_bytes', 'gpu_shared_bytes'):
                current, first = (process.get('gpu') or {}).get(key), (baseline.get('gpu') or {}).get(key)
                if current is not None and first is not None and current-first >= 256*MIB:
                    emit(key+'_growth', identity, current, first)
            for lane in process.get('native_timing', {}).get('lanes', []):
                if lane.get('pending_ms', 0) >= 100:
                    emit('native_call_pending', identity+':'+lane['kind'], lane['pending_ms'])
        for identity in self.processes.keys() - live:
            emit('process_exited_or_unobservable', identity, None)
        self.processes = {key: value for key, value in self.processes.items() if key in live}
        system = sample.get('system', {})
        if self.kernel_baseline is None and 'kernel_nonpaged_bytes' in system: self.kernel_baseline = system
        for key in ('kernel_nonpaged_bytes', 'kernel_paged_bytes'):
            if self.kernel_baseline and system.get(key, 0)-self.kernel_baseline.get(key, 0) >= 128*MIB:
                emit(key+'_growth', 'system', system[key], self.kernel_baseline[key])
        if system.get('available_bytes', 2**63) < 1024*MIB: emit('low_available_memory', 'system', system['available_bytes'])
        for key, threshold in [('dpc_percent', 10), ('interrupt_percent', 10), ('disk_read_s', .05), ('disk_write_s', .05)]:
            value = system.get('counters', {}).get(key)
            if value is not None and value >= threshold: emit('high_'+key, 'system', value)
        for endpoint, response in sample.get('endpoints', {}).items():
            if not response.get('ok'): continue
            stats = response['stats']
            for key in ('rseg_p99_ms', 'vseg_p99_ms'):
                if stats.get('running') and stats.get(key, 0) >= 20: emit('high_'+key, endpoint, stats[key])
            identity = (stats.get('session_id'), stats.get('uptime_s'), stats.get('running'))
            old, since = self.stats_seen.get(endpoint, (None, now))
            if identity != old: since = now
            if stats.get('running') and now-since >= 5: emit('stats_not_advancing', endpoint, now-since)
            self.stats_seen[endpoint] = (identity, since)
        if sample.get('collection_ms', 0) > 1500: emit('sampler_delayed', 'collector', sample['collection_ms'])
        self.cooldowns = {key: at for key, at in self.cooldowns.items() if now-at < 3600}
        return events


class JsonlWriter:
    def __init__(self, output, samples_per_file=3600):
        self.output, self.samples_per_file = output, samples_per_file
        self.index, self.file = None, None

    def write(self, row):
        index = row['sample'] // self.samples_per_file
        if index != self.index:
            if self.file: self.file.close()
            self.file = (self.output / f'samples-{index:04d}.jsonl').open('x', encoding='utf-8')
            self.index = index
        self.file.write(json.dumps(row, ensure_ascii=False, separators=(',', ':')) + chr(10))
        self.file.flush()

    def close(self):
        if self.file: self.file.close()


def append_json(path, row):
    data = (json.dumps(row, ensure_ascii=False) + chr(10)).encode('utf-8')
    handle = os.open(path, os.O_CREAT | os.O_WRONLY | os.O_APPEND, 0o600)
    try: os.write(handle, data)
    finally: os.close(handle)


def record(args, source=None):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    session = dict(stamp(), pid=os.getpid(), interval_s=args.interval,
        duration_s=args.seconds, sender=args.sender, receiver=args.receiver,
        windows_session='Local mappings; run in the same Windows login as the downstream app',
        native_timing='requires candidate process environment VC_DIAG=1; old DLLs remain observable through resources',
        context='all samples retained; event context_samples are +/-60 sample indices; no automatic recovery actions')
    with (output / 'session.json').open('x', encoding='utf-8') as file: json.dump(session, file, ensure_ascii=False, indent=2)
    if source is None:
        from longrun_windows import WindowsSource
        source = WindowsSource([*args.pid, os.getpid()], args.discovery_interval)
    writer, analyzer = JsonlWriter(output), Analyzer()
    counts, event_counts, errors = 0, Counter(), 0
    endpoint_ok, endpoint_errors, collection_max = Counter(), Counter(), 0
    started, next_tick = time.monotonic(), time.monotonic()
    interrupted, reason = False, 'duration_elapsed'

    def stop(signum, frame):
        nonlocal interrupted
        interrupted = True

    previous_signal = signal.signal(signal.SIGINT, stop)
    print(f'采样目录：{output}', flush=True)
    try:
        with ThreadPoolExecutor(max_workers=2, thread_name_prefix='stats-read') as pool:
            while time.monotonic()-started < args.seconds:
                if interrupted or (output / 'stop.request').exists(): reason = 'requested'; break
                tick = time.monotonic()
                futures = {kind: pool.submit(fetch_stats, base, kind) for kind, base in (
                    ('sender', args.sender), ('receiver', args.receiver))}
                row = dict(stamp(), sample=counts)
                try: row.update(source.sample())
                except Exception as exc:
                    row['source_error'] = f'{type(exc).__name__}: {exc}'; errors += 1
                row['endpoints'] = {kind: future.result() for kind, future in futures.items()}
                row['collection_ms'] = (time.monotonic()-tick)*1000
                collection_max = max(collection_max, row['collection_ms'])
                for kind, result in row['endpoints'].items():
                    (endpoint_ok if result['ok'] else endpoint_errors)[kind] += 1
                writer.write(row)
                for event in analyzer.observe(row):
                    append_json(output / 'events.jsonl', event); event_counts[event['type']] += 1
                counts += 1
                if counts % 30 == 0: print(f'已采样 {counts} 次，异常标记 {sum(event_counts.values())} 个', flush=True)
                next_tick += args.interval
                # Never catch up with a burst if discovery or the OS stalled the collector.
                next_tick = max(next_tick, time.monotonic())
                time.sleep(max(0, next_tick-time.monotonic()))
    finally:
        signal.signal(signal.SIGINT, previous_signal)
        writer.close(); source.close()
        summary = dict(stamp(), samples=counts, elapsed_s=time.monotonic()-started,
                       source_errors=errors, events=dict(event_counts), end_reason=reason,
                       endpoint_ok=dict(endpoint_ok), endpoint_errors=dict(endpoint_errors), collection_max_ms=collection_max)
        (output / 'summary.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding='utf-8')
    print(f'采样结束：{counts} 次；记录保存在 {output}', flush=True)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--hours', type=float, default=6)
    parser.add_argument('--seconds', type=float)
    parser.add_argument('--interval', type=float, default=1)
    parser.add_argument('--discovery-interval', type=float, default=15)
    parser.add_argument('--sender', default='http://192.168.2.59:5143')
    parser.add_argument('--receiver', default='http://127.0.0.1:5143')
    parser.add_argument('--pid', type=int, action='append', default=[])
    parser.add_argument('--mark', help='append a timestamped observation, e.g. 开始变卡 / 停止采集 / 关闭下游')
    parser.add_argument('--stop', action='store_true', help='stop only this collector via a request file')
    args = parser.parse_args()
    if args.mark or args.stop:
        if args.output is None or not (args.output / 'session.json').is_file(): parser.error('--mark/--stop needs an existing --output session')
        if args.mark: append_json(args.output / 'marks.jsonl', dict(stamp(), text=args.mark[:2000]))
        if args.stop: (args.output / 'stop.request').touch()
        return
    if os.name != 'nt': parser.error('Windows is required')
    args.seconds = args.seconds if args.seconds is not None else args.hours*3600
    if not (0 < args.seconds <= 7*86400 and .2 <= args.interval <= 60 and 1 <= args.discovery_interval <= 3600):
        parser.error('duration 0..7 days, interval .2..60 s, discovery 1..3600 s required')
    if args.output is None: args.output = ROOT / 'run' / ('diagnostics-'+datetime.now().strftime('%Y%m%d-%H%M%S'))
    record(args)


if __name__ == '__main__': main()
