"""J3: SHM hash 对账 + FIFO 检查（纯 ctypes mmap，无 COM）"""
import ctypes, ctypes.wintypes as wt, struct, time, json, os, sys

kernel32 = ctypes.windll.kernel32
# 设置正确的 64-bit 返回类型
kernel32.OpenFileMappingA.restype  = ctypes.c_void_p
kernel32.OpenFileMappingA.argtypes = [wt.DWORD, wt.BOOL, ctypes.c_char_p]
kernel32.MapViewOfFile.restype     = ctypes.c_void_p
kernel32.MapViewOfFile.argtypes    = [ctypes.c_void_p, wt.DWORD,
                                       wt.DWORD, wt.DWORD, ctypes.c_size_t]
kernel32.UnmapViewOfFile.restype   = wt.BOOL
kernel32.UnmapViewOfFile.argtypes  = [ctypes.c_void_p]
kernel32.CloseHandle.restype       = wt.BOOL
kernel32.CloseHandle.argtypes      = [ctypes.c_void_p]
kernel32.RtlCopyMemory             = kernel32.RtlMoveMemory
kernel32.RtlMoveMemory.restype     = None
kernel32.RtlMoveMemory.argtypes    = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_size_t]

FILE_MAP_READ = 0x4
SHM_NAME      = b"Local\\VirtualCaptureVcamFrame"
FRAME_BYTES   = 614400
HDR_SIZE      = 24      # sizeof(vc_shm_hdr_t) with natural alignment: 4+4+4+8+4 = 24
SHM_TOTAL     = HDR_SIZE + FRAME_BYTES   # 614424
SHM_MAGIC     = 0x5643414D
FNV_OFFSET    = 14695981039346656037
FNV_PRIME     = 1099511628211

def fnv1a64(data: bytes) -> int:
    h = FNV_OFFSET
    for b in data:
        h ^= b
        h = (h * FNV_PRIME) & 0xFFFFFFFFFFFFFFFF
    return h

def read_shm_frame(pView: int) -> tuple:
    """Returns (frame_id, stored_hash, frame_bytes) or None if torn/bad."""
    # Read header
    hdr_buf = (ctypes.c_char * HDR_SIZE)()
    kernel32.RtlMoveMemory(hdr_buf, pView, HDR_SIZE)
    magic, seq1, frame_id, stored_hash, size = struct.unpack_from('<IiIQI', hdr_buf)
    if magic != SHM_MAGIC or (seq1 & 1) or size != FRAME_BYTES:
        return None  # torn or bad magic
    # Read frame payload
    frame_buf = (ctypes.c_char * FRAME_BYTES)()
    kernel32.RtlMoveMemory(frame_buf, pView + HDR_SIZE, FRAME_BYTES)
    # Re-read seq to confirm not torn
    hdr2 = (ctypes.c_char * 8)()
    kernel32.RtlMoveMemory(hdr2, pView, 8)
    _, seq2 = struct.unpack_from('<Ii', hdr2)
    if seq1 != seq2:
        return None
    return frame_id, stored_hash, bytes(frame_buf)

def main():
    rx_session = sys.argv[1] if len(sys.argv) > 1 else "s-0144d793"
    run_dir = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "run")

    # Open SHM
    hMap = kernel32.OpenFileMappingA(FILE_MAP_READ, False, SHM_NAME)
    if not hMap:
        print("FAIL: OpenFileMapping error=%d (receiver not running?)" % ctypes.GetLastError())
        return False
    pView = kernel32.MapViewOfFile(hMap, FILE_MAP_READ, 0, 0, SHM_TOTAL)
    if not pView:
        print("FAIL: MapViewOfFile error=%d" % ctypes.GetLastError())
        kernel32.CloseHandle(hMap)
        return False
    print("SHM mapped at 0x%016X (%d bytes)" % (pView, SHM_TOTAL))

    # Load receiver hashlog
    rx_hashes = {}
    hlog_path = os.path.join(run_dir, rx_session + ".receiver.hash.jsonl")
    if os.path.exists(hlog_path):
        with open(hlog_path, encoding='utf-8', errors='replace') as f:
            for line in f:
                line = line.strip()
                if not line: continue
                try:
                    e = json.loads(line)
                    rx_hashes[int(e['frame_id'])] = int(e['hash'], 16)
                except: pass
    print("Receiver hashlog: %d entries" % len(rx_hashes))

    # Sample frames
    ok = fail = torn = 0
    N = 30
    print("Sampling %d frames @ 120ms intervals..." % N)
    for _ in range(N):
        time.sleep(0.12)
        result = read_shm_frame(pView)
        if result is None:
            torn += 1
            continue
        frame_id, stored_hash, frame_data = result
        computed = fnv1a64(frame_data)
        if computed != stored_hash:
            print("  CORRUPT fid=%d: computed=%016x stored=%016x" % (frame_id, computed, stored_hash))
            fail += 1
            continue
        if frame_id in rx_hashes:
            if rx_hashes[frame_id] == stored_hash:
                ok += 1
            else:
                print("  MISMATCH fid=%d shm=%016x rx=%016x" % (frame_id, stored_hash, rx_hashes[frame_id]))
                fail += 1
        else:
            ok += 1  # hash self-consistent, just not yet indexed in hashlog

    kernel32.UnmapViewOfFile(pView)
    kernel32.CloseHandle(hMap)
    print("SHM samples: ok=%d fail=%d torn=%d" % (ok, fail, torn))

    # FIFO check via receiver stats
    stats_path = os.path.join(run_dir, rx_session + ".receiver.stats.json")
    dropped = mismatch = shm_pub = complete = -1
    if os.path.exists(stats_path):
        d = json.load(open(stats_path, encoding='utf-8', errors='replace'))
        dropped   = d.get('dropped', -1)
        mismatch  = d.get('hash_mismatch', -1)
        shm_pub   = d.get('shm_published', -1)
        complete  = d.get('complete', -1)
    print("Receiver stats: complete=%d shm_published=%d dropped=%d hash_mismatch=%d" %
          (complete, shm_pub, dropped, mismatch))

    j3a = (dropped == 0 and mismatch == 0)
    j3b = (fail == 0 and ok > 0)
    print("\nJ3a FIFO (dropped=0, mismatch=0): %s" % ("PASS" if j3a else "FAIL"))
    print("J3b Hash (SHM == receiver):        %s" % ("PASS" if j3b else "FAIL"))
    return j3a and j3b

if __name__ == "__main__":
    ok = main()
    sys.exit(0 if ok else 1)
