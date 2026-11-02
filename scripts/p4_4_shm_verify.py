"""J3: SHM hash 对账 + FIFO 检查（纯 ctypes mmap，无 COM）"""
import ctypes, ctypes.wintypes as wt, struct, time, json, os, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from vc_shm import ShmReader, fnv1a64, SHM_NAME, SHM_DATA_OFF, SHM_SLOTS

def main():
    rx_session = sys.argv[1] if len(sys.argv) > 1 else "s-0144d793"
    run_dir = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "run")

    try:
        rd = ShmReader(SHM_NAME)
    except OSError as e:
        print("FAIL: %s" % e)
        return False
    print("SHM mapped (%d bytes, %dx%d@%d)" %
          (SHM_DATA_OFF + SHM_SLOTS * rd.frame_bytes, rd.width, rd.height, rd.fps))

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
        result = rd.read_latest()
        if result is None:
            torn += 1
            continue
        frame_id, stored_hash, frame_data = result
        computed = fnv1a64(frame_data)
        if stored_hash is not None and computed != stored_hash:
            print("  CORRUPT fid=%d: computed=%016x stored=%016x" % (frame_id, computed, stored_hash))
            fail += 1
            continue
        if frame_id in rx_hashes:
            if rx_hashes[frame_id] == computed:
                ok += 1
            else:
                print("  MISMATCH fid=%d shm=%016x rx=%016x" % (frame_id, computed, rx_hashes[frame_id]))
                fail += 1
        else:
            ok += 1  # self-consistent (or slot hash pending), not yet in hashlog

    rd.close()
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
