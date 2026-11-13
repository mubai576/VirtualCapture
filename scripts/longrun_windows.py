"""Read-only Windows probes: stdlib ctypes, English PDH names, no helper processes."""
import ctypes as C
from ctypes import wintypes as W
import re
import struct
import time

KINDS = {1: 'stats_write', 2: 'hash_write_flush', 3: 'sample_getbuffer',
         4: 'downstream_receive', 5: 'stats_prepare'}


class ProcessEntry(C.Structure):
    _fields_ = [('size', W.DWORD), ('usage', W.DWORD), ('pid', W.DWORD),
                ('heap', C.c_size_t), ('module', W.DWORD), ('threads', W.DWORD),
                ('parent', W.DWORD), ('priority', W.LONG), ('flags', W.DWORD), ('name', W.WCHAR * 260)]


class ModuleEntry(C.Structure):
    _fields_ = [('size', W.DWORD), ('module', W.DWORD), ('pid', W.DWORD),
                ('global_usage', W.DWORD), ('process_usage', W.DWORD),
                ('base', C.c_void_p), ('base_size', W.DWORD), ('handle', C.c_void_p),
                ('name', W.WCHAR * 256), ('path', W.WCHAR * 260)]


class ProcessMemory(C.Structure):
    _fields_ = [('size', W.DWORD), ('faults', W.DWORD)] + [
        (name, C.c_size_t) for name in ('peak_working', 'working', 'peak_paged',
        'paged', 'peak_nonpaged', 'nonpaged', 'pagefile', 'peak_pagefile', 'private')]


class Performance(C.Structure):
    _fields_ = [('size', W.DWORD)] + [(name, C.c_size_t) for name in (
        'commit', 'commit_limit', 'commit_peak', 'physical', 'available', 'cache',
        'kernel', 'kernel_paged', 'kernel_nonpaged', 'page_size')] + [
        (name, W.DWORD) for name in ('handles', 'processes', 'threads')]


class CounterValue(C.Structure):
    _fields_ = [('status', W.DWORD), ('value', C.c_double)]


class CounterItem(C.Structure):
    _fields_ = [('name', W.LPWSTR), ('value', CounterValue)]


def function(dll, name, restype, *args):
    result = getattr(dll, name)
    result.restype, result.argtypes = restype, list(args)
    return result


class Pdh:
    PATHS = {
        'cpu_percent': ['Processor(_Total)', '% Processor Time'],
        'dpc_percent': ['Processor(_Total)', '% DPC Time'],
        'interrupt_percent': ['Processor(_Total)', '% Interrupt Time'],
        'disk_read_s': ['PhysicalDisk(_Total)', 'Avg. Disk sec/Read'],
        'disk_write_s': ['PhysicalDisk(_Total)', 'Avg. Disk sec/Write'],
        'disk_queue': ['PhysicalDisk(_Total)', 'Current Disk Queue Length'],
        'page_reads_s': ['Memory', 'Page Reads/sec'],
        'gpu_dedicated_bytes': ['GPU Process Memory(*)', 'Dedicated Usage'],
        'gpu_shared_bytes': ['GPU Process Memory(*)', 'Shared Usage'],
    }

    def __init__(self):
        dll = C.WinDLL('pdh', use_last_error=True)
        status = C.c_uint32
        self.open = function(dll, 'PdhOpenQueryW', status, W.LPCWSTR, C.c_size_t, C.POINTER(C.c_void_p))
        self.add = function(dll, 'PdhAddEnglishCounterW', status, C.c_void_p, W.LPCWSTR, C.c_size_t, C.POINTER(C.c_void_p))
        self.collect = function(dll, 'PdhCollectQueryData', status, C.c_void_p)
        self.value = function(dll, 'PdhGetFormattedCounterValue', status, C.c_void_p, W.DWORD, C.c_void_p, C.POINTER(CounterValue))
        self.array = function(dll, 'PdhGetFormattedCounterArrayW', status, C.c_void_p, W.DWORD, C.POINTER(W.DWORD), C.POINTER(W.DWORD), C.c_void_p)
        self.close_query = function(dll, 'PdhCloseQuery', status, C.c_void_p)
        self.query, self.counters, self.errors = C.c_void_p(), {}, {}
        rc = self.open(None, 0, C.byref(self.query))
        if rc:
            self.errors['query'] = hex(rc)
            return
        for key, parts in self.PATHS.items():
            counter = C.c_void_p()
            rc = self.add(self.query, chr(92) + chr(92).join(parts), 0, C.byref(counter))
            if rc: self.errors[key] = hex(rc)
            else: self.counters[key] = counter
        self.collect(self.query)

    def sample(self):
        result, errors, gpu = {}, dict(self.errors), {}
        if not self.query: return result, gpu, errors
        rc = self.collect(self.query)
        if rc: errors['collect'] = hex(rc)
        for key, counter in self.counters.items():
            if key.startswith('gpu_'):
                size, count = W.DWORD(), W.DWORD()
                rc = self.array(counter, 0x200, C.byref(size), C.byref(count), None)
                if rc != 0x800007D2 or not size.value:
                    errors[key] = hex(rc) if rc else 'no_instances'
                    continue
                buffer = C.create_string_buffer(size.value)
                rc = self.array(counter, 0x200, C.byref(size), C.byref(count), buffer)
                if rc:
                    errors[key] = hex(rc)
                    continue
                items = C.cast(buffer, C.POINTER(CounterItem))
                for i in range(count.value):
                    item = items[i]
                    match = re.search(r'^pid_([0-9]+)_', item.name or '')
                    if not match or item.value.status not in (0, 1): continue
                    target = gpu.setdefault(int(match[1]), {})
                    target[key] = target.get(key, 0) + int(item.value.value)
            else:
                value = CounterValue()
                rc = self.value(counter, 0x200, None, C.byref(value))
                if rc or value.status not in (0, 1): errors[key] = hex(rc or value.status)
                else: result[key] = value.value
        return result, gpu, errors

    def close(self):
        if self.query: self.close_query(self.query); self.query = None


class WindowsSource:
    def __init__(self, pids=(), discovery_interval=15):
        self.k = C.WinDLL('kernel32', use_last_error=True)
        self.ps = C.WinDLL('psapi', use_last_error=True)
        self.snapshot = function(self.k, 'CreateToolhelp32Snapshot', C.c_void_p, W.DWORD, W.DWORD)
        self.close_handle = function(self.k, 'CloseHandle', W.BOOL, C.c_void_p)
        self.first = function(self.k, 'Process32FirstW', W.BOOL, C.c_void_p, C.POINTER(ProcessEntry))
        self.next = function(self.k, 'Process32NextW', W.BOOL, C.c_void_p, C.POINTER(ProcessEntry))
        self.module_first = function(self.k, 'Module32FirstW', W.BOOL, C.c_void_p, C.POINTER(ModuleEntry))
        self.module_next = function(self.k, 'Module32NextW', W.BOOL, C.c_void_p, C.POINTER(ModuleEntry))
        self.open_process = function(self.k, 'OpenProcess', C.c_void_p, W.DWORD, W.BOOL, W.DWORD)
        self.times = function(self.k, 'GetProcessTimes', W.BOOL, C.c_void_p, *([C.POINTER(C.c_uint64)] * 4))
        self.handles = function(self.k, 'GetProcessHandleCount', W.BOOL, C.c_void_p, C.POINTER(W.DWORD))
        self.memory = function(self.ps, 'GetProcessMemoryInfo', W.BOOL, C.c_void_p, C.POINTER(ProcessMemory), W.DWORD)
        self.performance = function(self.ps, 'GetPerformanceInfo', W.BOOL, C.POINTER(Performance), W.DWORD)
        self.open_map = function(self.k, 'OpenFileMappingW', C.c_void_p, W.DWORD, W.BOOL, W.LPCWSTR)
        self.map_view = function(self.k, 'MapViewOfFile', C.c_void_p, C.c_void_p, W.DWORD, W.DWORD, W.DWORD, C.c_size_t)
        self.unmap = function(self.k, 'UnmapViewOfFile', W.BOOL, C.c_void_p)
        self.qpc = function(self.k, 'QueryPerformanceCounter', W.BOOL, C.POINTER(C.c_uint64))
        self.explicit = set(pids)
        self.downstream, self.previous_cpu = {}, {}
        self.discovery_interval, self.next_discovery = discovery_interval, 0
        self.discovery = {}
        self.pdh = Pdh()

    def processes(self):
        handle = self.snapshot(2, 0)
        if handle == C.c_void_p(-1).value: raise OSError(C.get_last_error(), 'process snapshot')
        result = []
        try:
            entry = ProcessEntry(); entry.size = C.sizeof(entry)
            ok = self.first(handle, C.byref(entry))
            while ok:
                result.append({'pid': entry.pid, 'name': entry.name, 'threads': entry.threads})
                ok = self.next(handle, C.byref(entry))
        finally: self.close_handle(handle)
        return result

    def camera_module(self, pid):
        handle = self.snapshot(8 | 16, pid)
        if handle == C.c_void_p(-1).value: return None, C.get_last_error()
        try:
            entry = ModuleEntry(); entry.size = C.sizeof(entry)
            ok = self.module_first(handle, C.byref(entry))
            while ok:
                if entry.name.lower() == 'vc-vcam.dll': return entry.path, None
                ok = self.module_next(handle, C.byref(entry))
            return None, None
        finally: self.close_handle(handle)

    def process_resources(self, item, gpu, now):
        result = dict(item)
        handle = self.open_process(0x1000 | 0x10, False, item['pid'])
        if not handle:
            result['error'] = f'OpenProcess:{C.get_last_error()}'
            return result
        try:
            mem = ProcessMemory(); mem.size = C.sizeof(mem)
            if self.memory(handle, C.byref(mem), mem.size):
                result.update(private_bytes=mem.private, working_bytes=mem.working, page_faults=mem.faults)
            else: result['memory_error'] = C.get_last_error()
            count = W.DWORD()
            if self.handles(handle, C.byref(count)): result['handles'] = count.value
            creation, exit_time, kernel, user = (C.c_uint64() for _ in range(4))
            if self.times(handle, C.byref(creation), C.byref(exit_time), C.byref(kernel), C.byref(user)):
                result['created_100ns'] = creation.value
                ticks = kernel.value + user.value
                previous = self.previous_cpu.get(item['pid'])
                if previous and previous[0] == creation.value and now > previous[1]:
                    result['cpu_core_percent'] = (ticks - previous[2]) / 1e7 / (now - previous[1]) * 100
                self.previous_cpu[item['pid']] = (creation.value, now, ticks)
            result['gpu'] = gpu.get(item['pid'])
            result['native_timing'] = self.native_timing(item['pid'])
            return result
        finally: self.close_handle(handle)

    def native_timing(self, pid):
        handle = self.open_map(4, False, f'Local{chr(92)}VirtualCapture.Diag.{pid}')
        if not handle: return {'available': False, 'winerror': C.get_last_error()}
        view = None
        try:
            view = self.map_view(handle, 4, 0, 0, 4160)
            if not view: return {'available': False, 'winerror': C.get_last_error()}
            magic, version, size, lanes, owner, _, frequency = struct.unpack('<6IQ32x', C.string_at(view, 64))
            if (magic, version, size, lanes, owner) != (0x56434447, 1, 4160, 32, pid) or not frequency:
                return {'available': False, 'error': 'layout_mismatch'}
            now = C.c_uint64(); self.qpc(C.byref(now))
            rows = []
            for index in range(lanes):
                address = view + 64 + index * 128
                raw = C.string_at(address, 128)
                seq, lease, tid, kind, generation, begin, count, total, maximum, last, slow, failures = struct.unpack('<iiII8Q48x', raw)
                verify = C.c_int32.from_address(address).value
                if seq & 1 or seq != verify or lease == 2 or not generation: continue
                rows.append(dict(lane=index, generation=generation, active=lease == 1, tid=tid,
                    kind=KINDS.get(kind, str(kind)), count=count, total_ms=total*1000/frequency,
                    max_ms=maximum*1000/frequency, last_ms=last*1000/frequency,
                    slow_over_20ms=slow, failures=failures,
                    pending_ms=max(0, now.value-begin)*1000/frequency if begin else 0))
            return {'available': True, 'lanes': rows}
        finally:
            if view: self.unmap(view)
            self.close_handle(handle)

    def sample(self):
        now = time.monotonic()
        inventory = self.processes()
        live = {p['pid'] for p in inventory}
        self.downstream = {pid: path for pid, path in self.downstream.items() if pid in live}
        self.previous_cpu = {pid: row for pid, row in self.previous_cpu.items() if pid in live}
        if now >= self.next_discovery:
            self.downstream = {}
            denied = errors = scanned = 0
            for process in inventory:
                if process['pid'] in (0, 4): continue
                path, error = self.camera_module(process['pid'])
                scanned += 1
                if path: self.downstream[process['pid']] = path
                if error == 5: denied += 1
                elif error: errors += 1
            self.discovery = dict(scanned=scanned, access_denied=denied, other_errors=errors,
                                  interval_s=self.discovery_interval, at_monotonic=now)
            self.next_discovery = now + self.discovery_interval
        counters, gpu, errors = self.pdh.sample()
        system = dict(counters=counters, counter_errors=errors)
        perf = Performance(); perf.size = C.sizeof(perf)
        if self.performance(C.byref(perf), perf.size):
            system.update({key+'_bytes': getattr(perf, key)*perf.page_size for key in (
                'commit', 'commit_limit', 'physical', 'available', 'kernel_paged', 'kernel_nonpaged')})
            system.update(handles=perf.handles, threads=perf.threads, processes=perf.processes)
        else: system['memory_error'] = C.get_last_error()
        selected = []
        names = {'vc-agent.exe', 'vc-receiver.exe', 'vc-sender.exe', 'vc-vcam-verify.exe', 'vc-vcam-lifecycle.exe'}
        for process in inventory:
            if process['pid'] in self.explicit or process['pid'] in self.downstream or process['name'].lower() in names:
                row = self.process_resources(process, gpu, now)
                if process['pid'] in self.downstream: row['vcam_dll'] = self.downstream[process['pid']]
                selected.append(row)
        return dict(system=system, processes=selected, discovery=self.discovery)

    def close(self):
        self.pdh.close()
