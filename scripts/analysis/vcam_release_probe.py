"""Direct-load release probe. Never registers/runs capture or force-frees leaked objects."""
import argparse
from collections import Counter
import ctypes as C
from ctypes import wintypes as W
import json
import os
from pathlib import Path
import sys
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from longrun_windows import ProcessMemory, function


def guid(value):
    return C.create_string_buffer(uuid.UUID(value).bytes_le)


def method(pointer, slot, result, *arguments):
    table = C.cast(pointer, C.POINTER(C.POINTER(C.c_void_p))).contents
    return C.WINFUNCTYPE(result, C.c_void_p, *arguments)(table[slot])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dll', type=Path, required=True)
    parser.add_argument('--count', type=int, default=100)
    args = parser.parse_args()
    if not 1 <= args.count <= 2000: parser.error('count must be 1..2000')
    os.environ['VCAM_FRAME_MAP'] = 'Local'+chr(92)+'VC-Absent-'+uuid.uuid4().hex
    dll = C.WinDLL(str(args.dll.resolve()))
    get = function(dll, 'DllGetClassObject', C.c_long, C.c_void_p, C.c_void_p, C.POINTER(C.c_void_p))
    unload = function(dll, 'DllCanUnloadNow', C.c_long)
    kernel, ps = C.WinDLL('kernel32'), C.WinDLL('psapi')
    memory = function(ps, 'GetProcessMemoryInfo', W.BOOL, C.c_void_p, C.POINTER(ProcessMemory), W.DWORD)
    current = function(kernel, 'GetCurrentProcess', C.c_void_p)()

    def private():
        value = ProcessMemory(); value.size = C.sizeof(value)
        if not memory(current, C.byref(value), value.size): raise C.WinError()
        return value.private

    initial = unload()
    factory = C.c_void_p()
    assert get(guid('b84c7e21-4a5f-4e2b-9c11-5a3d7f01229a'), guid('00000001-0000-0000-c000-000000000046'), C.byref(factory)) == 0
    create = method(factory, 3, C.c_long, C.c_void_p, C.c_void_p, C.POINTER(C.c_void_p))
    counts = Counter(); before = private()
    for index in range(args.count):
        instance = C.c_void_p()
        assert create(factory, None, guid('00000000-0000-0000-c000-000000000046'), C.byref(instance)) == 0
        counts[method(instance, 2, C.c_ulong)(instance)] += 1
    after = private()
    factory_refs = method(factory, 2, C.c_ulong)(factory)
    print(json.dumps(dict(dll=str(args.dll.resolve()), iterations=args.count, release_returns=dict(counts),
        private_before=before, private_after=after, private_delta=after-before,
        factory_release=factory_refs, initial_can_unload=initial, final_can_unload=unload()), indent=2))
    # Process exit reclaims baseline leaks. No double Release, FreeLibrary or registration.


if __name__ == '__main__': main()
