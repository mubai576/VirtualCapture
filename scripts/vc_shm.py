"""Read-only Python mirror of vcam/vcam_shm.h: variable-size v3 plus legacy v2."""
import ctypes, ctypes.wintypes as wt, struct

FRAME_BYTES = 614400
SHM_NAME    = "Local\\VirtualCaptureVcamFrame"
SHM_MAGIC   = 0x33414356          # "VCA3"; read legacy v2 as 640x640@240
SHM_VERSION = 3
SHM_SLOTS   = 12
SHM_DATA_OFF = 4096
SHM_SIZE    = SHM_DATA_OFF + SHM_SLOTS * FRAME_BYTES
HASH_NONE   = 0xFFFFFFFF
FILE_MAP_READ = 0x4
HDR_FMT  = '<IIIIIiiI'            # magic version slots frame_bytes data_off latest_slot latest_seq writer_pid
SLOT_FMT = '<iIQQQI'              # seq frame_id qpc_first_ns qpc_pub_ns hash hash_fid

k32 = ctypes.windll.kernel32
k32.OpenFileMappingA.restype = ctypes.c_void_p
k32.OpenFileMappingA.argtypes = [wt.DWORD, wt.BOOL, ctypes.c_char_p]
k32.MapViewOfFile.restype = ctypes.c_void_p
k32.MapViewOfFile.argtypes = [ctypes.c_void_p, wt.DWORD, wt.DWORD, wt.DWORD, ctypes.c_size_t]
k32.UnmapViewOfFile.argtypes = [ctypes.c_void_p]
k32.CloseHandle.argtypes = [ctypes.c_void_p]


def fnv1a64(data: bytes) -> int:
    h = 14695981039346656037
    for b in data:
        h ^= b
        h = (h * 1099511628211) & 0xFFFFFFFFFFFFFFFF
    return h


class ShmReader:
    def __init__(self, name=SHM_NAME):
        self.hmap = k32.OpenFileMappingA(FILE_MAP_READ, False, name.encode())
        if not self.hmap:
            raise OSError("OpenFileMapping %s failed err=%d (receiver not running / --shm not passed?)"
                          % (name, ctypes.GetLastError()))
        self.view = k32.MapViewOfFile(self.hmap, FILE_MAP_READ, 0, 0, 64)
        if not self.view:
            k32.CloseHandle(self.hmap)
            raise OSError("MapViewOfFile failed (mapping smaller than v2 -> old v1 receiver?)")
        try:
            header = self.header()
            magic, ver, slots, fb, doff, *_ = header
            self.width, self.height, self.fps = struct.unpack_from('<III', ctypes.string_at(self.view + 32, 12)) if ver == 3 else (640, 640, 240)
            if (not ((magic == SHM_MAGIC and ver == 3) or (magic == 0x32414356 and ver == 2)) or
                    slots != SHM_SLOTS or doff != SHM_DATA_OFF or not 2 <= self.width <= 8192 or
                    not 2 <= self.height <= 8192 or self.width % 2 or self.height % 2 or
                    not 1 <= self.fps <= 1000 or fb != self.width * self.height * 3 // 2):
                raise OSError("Invalid SHM geometry/version")
            self.frame_bytes = fb
            self.identity = (magic, ver, slots, fb, doff, header[-1])
            k32.UnmapViewOfFile(self.view)
            self.view = k32.MapViewOfFile(self.hmap, FILE_MAP_READ, 0, 0, doff + slots * fb)
            if not self.view:
                raise OSError("Truncated SHM mapping")
        except Exception:
            self.close()
            raise

    def header(self):
        return struct.unpack_from(HDR_FMT, ctypes.string_at(self.view, 32))

    def read_latest(self):
        """(frame_id, hash or None, frame_bytes) of the latest publish, or None if torn/bad.
        hash is None when the receiver has not hashed this slot yet (hash is computed after publish)."""
        magic, ver, slots, fb, doff, ls, lseq, _ = self.header()
        if (magic, ver, slots, fb, doff, _) != self.identity or not 0 <= ls < slots:
            return None
        so = self.view + 64 + 64 * ls
        seq1, fid, _, _, h, hfid = struct.unpack_from(SLOT_FMT, ctypes.string_at(so, 36))
        if seq1 & 1:
            return None
        data = ctypes.string_at(self.view + doff + ls * fb, fb)
        seq2, = struct.unpack_from('<i', ctypes.string_at(so, 4))
        if seq1 != seq2:
            return None
        return fid, (h if hfid == fid else None), data

    def close(self):
        if self.view: k32.UnmapViewOfFile(self.view); self.view = None
        if self.hmap: k32.CloseHandle(self.hmap); self.hmap = None
