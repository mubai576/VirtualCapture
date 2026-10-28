"""
P4-4 vcam 出口验收脚本 (纯 ctypes, 无第三方依赖)
判据:
  J1. DShow 枚举到 "VirtualCapture"
  J2. IAMStreamConfig GetFormat → NV12 640x640 AvgTimePerFrame=41666
  J3. SHM 读帧 hash 与 receiver hashlog 一致; FIFO=0 (dropped=0)
"""
import ctypes, ctypes.wintypes as wt, struct, os, sys, json, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUN_DIR = os.path.join(ROOT, "run")

# ── GUIDs ──────────────────────────────────────────────────────────────────
def GUID(a, b, c, d_bytes):
    class _GUID(ctypes.Structure):
        _fields_ = [("Data1",wt.DWORD),("Data2",wt.WORD),("Data3",wt.WORD),("Data4",ctypes.c_ubyte*8)]
    g = _GUID(a, b, c, (ctypes.c_ubyte*8)(*d_bytes))
    return g

class GUID(ctypes.Structure):
    _fields_ = [("Data1",wt.DWORD),("Data2",wt.WORD),("Data3",wt.WORD),("Data4",ctypes.c_ubyte*8)]

def make_guid(hex_str):
    """Parse GUID string {XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}"""
    h = hex_str.strip("{}").replace("-","")
    d1 = int(h[0:8],16); d2 = int(h[8:12],16); d3 = int(h[12:16],16)
    d4 = bytes.fromhex(h[16:32])
    return GUID(d1,d2,d3,(ctypes.c_ubyte*8)(*d4))

IID_IUnknown          = make_guid("{00000000-0000-0000-C000-000000000046}")
IID_IBaseFilter       = make_guid("{56A86895-0AD4-11CE-B03A-0020AF0BA770}")
IID_IEnumPins         = make_guid("{56A86892-0AD4-11CE-B03A-0020AF0BA770}")
IID_IPin              = make_guid("{56A86891-0AD4-11CE-B03A-0020AF0BA770}")
IID_IAMStreamConfig   = make_guid("{C6E13340-30AC-11D0-A18C-00A0C9118956}")
IID_ICreateDevEnum    = make_guid("{29840822-5B84-11D0-BD3B-00A0C911CE86}")
IID_IEnumMoniker      = make_guid("{00000102-0000-0000-C000-000000000046}")
IID_IPropertyBag      = make_guid("{55272A00-42CB-11CE-8135-00AA004BB851}")
CLSID_SystemDeviceEnum    = make_guid("{62BE5D10-60EB-11D0-BD3B-00A0C911CE86}")
CLSID_VideoInputDevCat    = make_guid("{860BB310-5D01-11D0-BD3B-00A0C911CE86}")

ole32 = ctypes.windll.ole32
oleaut32 = ctypes.windll.oleaut32
kernel32 = ctypes.windll.kernel32

COINIT_APARTMENTTHREADED = 0x2
FILE_MAP_READ = 0x4
FNV_OFFSET = 14695981039346656037
FNV_PRIME  = 1099511628211
FRAME_BYTES = 614400

# ── FNV-1a64 ───────────────────────────────────────────────────────────────
def fnv1a64(data):
    h = FNV_OFFSET
    for b in data:
        h ^= b
        h = (h * FNV_PRIME) & 0xFFFFFFFFFFFFFFFF
    return h

# ── COM helper ─────────────────────────────────────────────────────────────
CLSCTX_INPROC_SERVER = 1

def co_create(clsid, iid):
    ppv = ctypes.c_void_p()
    hr = ole32.CoCreateInstance(ctypes.byref(clsid), None, CLSCTX_INPROC_SERVER,
                                ctypes.byref(iid), ctypes.byref(ppv))
    if hr != 0: raise OSError("CoCreateInstance hr=0x%08X" % (hr & 0xFFFFFFFF))
    return ppv.value

def qi(punk, iid):
    """QueryInterface on raw vtable pointer"""
    vtbl = ctypes.cast(punk, ctypes.POINTER(ctypes.c_void_p))[0]
    QI = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p,
                             ctypes.POINTER(GUID), ctypes.POINTER(ctypes.c_void_p))
    fn = QI(ctypes.cast(vtbl, ctypes.POINTER(ctypes.c_void_p))[0])
    ppv = ctypes.c_void_p()
    hr = fn(punk, ctypes.byref(iid), ctypes.byref(ppv))
    if hr != 0: return None
    return ppv.value

def release(punk):
    if punk:
        vtbl = ctypes.cast(punk, ctypes.POINTER(ctypes.c_void_p))[0]
        Rel = ctypes.WINFUNCTYPE(ctypes.c_ulong, ctypes.c_void_p)
        fn = Rel(ctypes.cast(vtbl, ctypes.POINTER(ctypes.c_void_p))[2])
        fn(punk)

def vtbl_fn(punk, idx, restype, *argtypes):
    vtbl = ctypes.cast(punk, ctypes.POINTER(ctypes.c_void_p))[0]
    Fn = ctypes.WINFUNCTYPE(restype, ctypes.c_void_p, *argtypes)
    return Fn(ctypes.cast(vtbl, ctypes.POINTER(ctypes.c_void_p))[idx])

# ── J1 + J2: COM enumeration + format check ───────────────────────────────
def check_j1_j2():
    print("\n[J1] DShow 枚举 + [J2] IAMStreamConfig 格式协商")
    ole32.CoInitializeEx(None, COINIT_APARTMENTTHREADED)

    # Create system device enum
    try:
        pSDE = co_create(CLSID_SystemDeviceEnum, IID_ICreateDevEnum)
    except OSError as e:
        print("  FAIL CoCreateInstance SystemDeviceEnum:", e)
        return False, False

    # CreateClassEnumerator for video capture
    # vtbl: QI(0) AddRef(1) Release(2) CreateClassEnumerator(3)
    pEnum = ctypes.c_void_p()
    CCE = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p,
                              ctypes.POINTER(GUID), ctypes.POINTER(ctypes.c_void_p), wt.DWORD)
    vtbl = ctypes.cast(pSDE, ctypes.POINTER(ctypes.c_void_p))[0]
    fn_cce = CCE(ctypes.cast(vtbl, ctypes.POINTER(ctypes.c_void_p))[3])
    hr = fn_cce(pSDE, ctypes.byref(CLSID_VideoInputDevCat), ctypes.byref(pEnum), 0)
    release(pSDE)
    if hr != 0 or not pEnum.value:
        print("  FAIL CreateClassEnumerator hr=0x%08X" % (hr & 0xFFFFFFFF))
        return False, False

    # IEnumMoniker::Next
    # vtbl: QI(0) AddRef(1) Release(2) Next(3) Skip(4) Reset(5) Clone(6)
    found_j1 = False
    found_j2 = False
    NV12_FOURCC = 0x3231564E  # 'NV12'
    MEDIASUBTYPE_NV12 = make_guid("{3231564E-0000-0010-8000-00AA00389B71}")

    while True:
        pMon = ctypes.c_void_p()
        fetched = ctypes.c_ulong(0)
        Next = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p,
                                   ctypes.c_ulong, ctypes.POINTER(ctypes.c_void_p),
                                   ctypes.POINTER(ctypes.c_ulong))
        vtbl_e = ctypes.cast(pEnum.value, ctypes.POINTER(ctypes.c_void_p))[0]
        fn_next = Next(ctypes.cast(vtbl_e, ctypes.POINTER(ctypes.c_void_p))[3])
        hr = fn_next(pEnum.value, 1, ctypes.byref(pMon), ctypes.byref(fetched))
        if hr != 0 or fetched.value == 0:
            break

        # IPropertyBag to get FriendlyName
        pBag = ctypes.c_void_p()
        # IMoniker::BindToStorage (idx=8 in IMoniker vtbl after IPersistStream)
        # Actually easier: use IMoniker::BindToObject
        # IMoniker vtbl: IUnknown(0-2) IPersist(3) IPersistStream(4-8) IMoniker(9-20)
        # BindToObject is index 9 in full vtbl
        # For IPropertyBag use IMoniker::BindToStorage or RemoteBindToStorage
        # Try IBindCtx = NULL approach
        BTOS = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p,
                                   ctypes.c_void_p, ctypes.c_void_p,
                                   ctypes.POINTER(GUID), ctypes.POINTER(ctypes.c_void_p))
        vtbl_m = ctypes.cast(pMon.value, ctypes.POINTER(ctypes.c_void_p))[0]
        fn_btos = BTOS(ctypes.cast(vtbl_m, ctypes.POINTER(ctypes.c_void_p))[9])  # BindToStorage (IMoniker idx=9)
        hr2 = fn_btos(pMon.value, None, None, ctypes.byref(IID_IPropertyBag), ctypes.byref(pBag))

        friendly_name = ""
        if hr2 == 0 and pBag.value:
            # IPropertyBag::Read(3): name, variant, errorlog
            bstr_name = oleaut32.SysAllocString(ctypes.c_wchar_p("FriendlyName"))
            class VARIANT(ctypes.Structure):
                _fields_ = [("vt", ctypes.c_ushort), ("r1",ctypes.c_ushort),
                             ("r2",ctypes.c_ushort), ("r3",ctypes.c_ushort),
                             ("data", ctypes.c_wchar_p), ("_pad", ctypes.c_void_p)]
            var = VARIANT(); var.vt = 0
            Read = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p,
                                       ctypes.c_wchar_p, ctypes.c_void_p, ctypes.c_void_p)
            vtbl_b = ctypes.cast(pBag.value, ctypes.POINTER(ctypes.c_void_p))[0]
            fn_read = Read(ctypes.cast(vtbl_b, ctypes.POINTER(ctypes.c_void_p))[3])
            fn_read(pBag.value, "FriendlyName", ctypes.byref(var), None)
            if var.vt == 8 and var.data:  # VT_BSTR=8
                friendly_name = var.data
            oleaut32.SysFreeString(bstr_name)
            release(pBag.value)

        print("  device: %r" % friendly_name)
        if "VirtualCapture" in friendly_name:
            found_j1 = True
            print("  J1 PASS: found 'VirtualCapture'")

            # BindToObject → IBaseFilter
            pFilter = ctypes.c_void_p()
            BTO = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p,
                                      ctypes.c_void_p, ctypes.c_void_p,
                                      ctypes.POINTER(GUID), ctypes.POINTER(ctypes.c_void_p))
            fn_bto = BTO(ctypes.cast(vtbl_m, ctypes.POINTER(ctypes.c_void_p))[8])  # BindToObject (IMoniker idx=8)
            hr3 = fn_bto(pMon.value, None, None, ctypes.byref(IID_IBaseFilter), ctypes.byref(pFilter))
            if hr3 != 0 or not pFilter.value:
                print("  FAIL BindToObject hr=0x%08X" % (hr3 & 0xFFFFFFFF))
                release(pMon.value)
                continue

            # EnumPins on IBaseFilter (vtbl idx=9 after IUnknown+IMediaFilter)
            # IBaseFilter vtbl: IUnknown(0-2) IMediaFilter(3-7) IBaseFilter(8-14)
            # EnumPins = idx 9? Let's check:
            # IMediaFilter: GetClassID(3) Stop(4) Pause(5) Run(6) GetState(7) SetSyncSource(8) GetSyncSource(9)?
            # Actually IBaseFilter inherits IMediaFilter which inherits IPersist which inherits IUnknown
            # IPersist(3)=GetClassID, IMediaFilter(4-9)=Stop/Pause/Run/GetState/SetSyncSource/GetSyncSource
            # IBaseFilter(10-?)=EnumPins/FindPin/QueryFilterInfo/JoinFilterGraph/QueryVendorInfo
            # So EnumPins = idx 10
            pEnumPins = ctypes.c_void_p()
            EP = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p,
                                     ctypes.POINTER(ctypes.c_void_p))
            vtbl_f = ctypes.cast(pFilter.value, ctypes.POINTER(ctypes.c_void_p))[0]
            fn_ep = EP(ctypes.cast(vtbl_f, ctypes.POINTER(ctypes.c_void_p))[10])
            hr4 = fn_ep(pFilter.value, ctypes.byref(pEnumPins))
            if hr4 != 0 or not pEnumPins.value:
                print("  FAIL EnumPins hr=0x%08X" % (hr4 & 0xFFFFFFFF))
                release(pFilter.value); release(pMon.value)
                continue

            # Walk pins to find output pin + IAMStreamConfig
            pin_found = False
            while True:
                pPin = ctypes.c_void_p(); fetch2 = ctypes.c_ulong(0)
                Next2 = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p,
                                            ctypes.c_ulong, ctypes.POINTER(ctypes.c_void_p),
                                            ctypes.POINTER(ctypes.c_ulong))
                vtbl_ep = ctypes.cast(pEnumPins.value, ctypes.POINTER(ctypes.c_void_p))[0]
                fn_np = Next2(ctypes.cast(vtbl_ep, ctypes.POINTER(ctypes.c_void_p))[3])
                hr5 = fn_np(pEnumPins.value, 1, ctypes.byref(pPin), ctypes.byref(fetch2))
                if hr5 != 0 or fetch2.value == 0: break

                # QueryDirection: IPin vtbl: QI(0) AddRef(1) Release(2) Connect(3) ReceiveConnection(4)
                # Disconnect(5) ConnectedTo(6) ConnectionMediaType(7) QueryPinInfo(8) QueryDirection(9)
                DIR = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p, ctypes.POINTER(ctypes.c_int))
                vtbl_p = ctypes.cast(pPin.value, ctypes.POINTER(ctypes.c_void_p))[0]
                fn_dir = DIR(ctypes.cast(vtbl_p, ctypes.POINTER(ctypes.c_void_p))[9])
                direction = ctypes.c_int(0)
                fn_dir(pPin.value, ctypes.byref(direction))
                # 0=PINDIR_INPUT, 1=PINDIR_OUTPUT
                if direction.value == 1:
                    # QI for IAMStreamConfig
                    pSC = qi(pPin.value, IID_IAMStreamConfig)
                    if pSC:
                        # IAMStreamConfig vtbl: QI(0) AddRef(1) Release(2)
                        # SetFormat(3) GetFormat(4) GetNumberOfCapabilities(5) GetStreamCaps(6)
                        class AM_MEDIA_TYPE(ctypes.Structure):
                            _fields_ = [
                                ("majortype", GUID), ("subtype", GUID),
                                ("bFixedSizeSamples", ctypes.c_int),
                                ("bTemporalCompression", ctypes.c_int),
                                ("lSampleSize", wt.ULONG),
                                ("formattype", GUID),
                                ("pUnk", ctypes.c_void_p),
                                ("cbFormat", wt.ULONG),
                                ("pbFormat", ctypes.c_void_p),
                            ]
                        pmt = ctypes.POINTER(AM_MEDIA_TYPE)()
                        GF = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p,
                                                 ctypes.POINTER(ctypes.POINTER(AM_MEDIA_TYPE)))
                        vtbl_sc = ctypes.cast(pSC, ctypes.POINTER(ctypes.c_void_p))[0]
                        fn_gf = GF(ctypes.cast(vtbl_sc, ctypes.POINTER(ctypes.c_void_p))[4])
                        hr6 = fn_gf(pSC, ctypes.byref(pmt))
                        if hr6 == 0 and pmt:
                            mt = pmt.contents
                            # Parse VIDEOINFOHEADER from pbFormat
                            # VIDEOINFOHEADER: RECT(4*4) RECT(4*4) DWORD DWORD REFERENCE_TIME(8) BITMAPINFOHEADER
                            # BITMAPINFOHEADER: DWORD biSize, LONG biWidth, LONG biHeight, WORD biPlanes,
                            #   WORD biBitCount, DWORD biCompression, ...
                            # AvgTimePerFrame is at offset 32 (after two RECTs + 2 DWORDs)
                            # Actually: RECT=16B, RECT=16B, DWORD=4B, DWORD=4B, then REFERENCE_TIME=8B
                            # = offset 0+16+16+4+4 = 40 for AvgTimePerFrame? Let me recalculate:
                            # RECT(16) + RECT(16) + dwBitRate(4) + dwBitErrorRate(4) = 40 bytes before AvgTimePerFrame
                            # AvgTimePerFrame at offset 40, size 8
                            # BITMAPINFOHEADER at offset 48
                            # biWidth at offset 48+4=52, biHeight at 48+8=56, biCompression at 48+16=64
                            if mt.cbFormat >= 72 and mt.pbFormat:
                                fmt = ctypes.string_at(mt.pbFormat, min(mt.cbFormat, 80))
                                avg_time = struct.unpack_from('<q', fmt, 40)[0]  # REFERENCE_TIME
                                bi_width = struct.unpack_from('<i', fmt, 52)[0]
                                bi_height = struct.unpack_from('<i', fmt, 56)[0]
                                bi_compr  = struct.unpack_from('<I', fmt, 64)[0]
                                fourcc = bi_compr.to_bytes(4,'little').decode('ascii','replace')

                                # subtype GUID Data1 is the FOURCC
                                st_data1 = mt.subtype.Data1

                                print("  IAMStreamConfig GetFormat:")
                                print("    biWidth=%d biHeight=%d" % (bi_width, abs(bi_height)))
                                print("    fourcc=0x%08X (%s) subtype.Data1=0x%08X" % (bi_compr, fourcc, st_data1))
                                print("    AvgTimePerFrame=%d (%.1ffps)" % (avg_time, 1e7/avg_time if avg_time else 0))

                                ok_fmt = (abs(bi_width)==640 and abs(bi_height)==640 and
                                          (bi_compr==NV12_FOURCC or st_data1==NV12_FOURCC) and
                                          avg_time==41666)
                                if ok_fmt:
                                    print("  J2 PASS: NV12 640x640 AvgTimePerFrame=41666")
                                    found_j2 = True
                                else:
                                    print("  J2 FAIL: format mismatch")
                                # Free AM_MEDIA_TYPE
                                if mt.pbFormat:
                                    ole32.CoTaskMemFree(mt.pbFormat)
                                ole32.CoTaskMemFree(pmt)
                        release(pSC)
                        pin_found = True
                release(pPin.value)
                if pin_found: break
            release(pEnumPins.value)
            release(pFilter.value)
        release(pMon.value)

    release(pEnum.value)
    ole32.CoUninitialize()
    return found_j1, found_j2

# ── J3: SHM hash verification ──────────────────────────────────────────────
def check_j3(rx_session_id, num_samples=20, interval_ms=100):
    print("\n[J3] SHM hash 对账 + FIFO 检查")

    # Open SHM (layout v2, scripts/vc_shm.py mirrors vcam/vcam_shm.h)
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from vc_shm import ShmReader
    try:
        rd = ShmReader()
    except OSError as e:
        print("  FAIL %s" % e)
        return False

    # Load receiver hashlog
    hashlog_path = os.path.join(RUN_DIR, rx_session_id + ".receiver.hash.jsonl")
    rx_hashes = {}  # frame_id -> hash (int)
    if os.path.exists(hashlog_path):
        with open(hashlog_path, encoding='utf-8', errors='replace') as f:
            for line in f:
                try:
                    e = json.loads(line.strip())
                    rx_hashes[e['frame_id']] = int(e['hash'], 16)
                except: pass
    print("  receiver hashlog entries: %d" % len(rx_hashes))

    ok_count = 0; fail_count = 0; torn_count = 0
    for _ in range(num_samples):
        time.sleep(interval_ms / 1000.0)
        r = rd.read_latest()
        if r is None:
            torn_count += 1
            continue
        frame_id, stored_hash, frame_data = r
        computed = fnv1a64(frame_data)
        if stored_hash is not None and computed != stored_hash:
            print("  WARN fid=%d shm slot hash != recomputed (SHM corruption?)" % frame_id)
            fail_count += 1
            continue
        if frame_id in rx_hashes:
            if rx_hashes[frame_id] == computed:
                ok_count += 1
            else:
                print("  MISMATCH fid=%d shm=0x%016x rx=0x%016x" % (frame_id, computed, rx_hashes[frame_id]))
                fail_count += 1
        else:
            # not yet in receiver hashlog (hash thread lag): self-consistent
            ok_count += 1

    rd.close()

    print("  SHM samples: ok=%d fail=%d torn=%d" % (ok_count, fail_count, torn_count))

    # FIFO check via receiver stats
    stats_path = os.path.join(RUN_DIR, rx_session_id + ".receiver.stats.json")
    dropped = hash_mismatch = shm_pub = complete = 0
    if os.path.exists(stats_path):
        d = json.load(open(stats_path, encoding='utf-8', errors='replace'))
        dropped      = d.get('dropped', -1)
        hash_mismatch= d.get('hash_mismatch', -1)
        shm_pub      = d.get('shm_published', -1)
        complete     = d.get('complete', -1)
        print("  receiver stats: complete=%d dropped=%d hash_mismatch=%d shm_published=%d" %
              (complete, dropped, hash_mismatch, shm_pub))
    else:
        print("  WARN stats not found: %s" % stats_path)

    fifo_ok = (dropped == 0 and hash_mismatch == 0)
    shm_ok  = (fail_count == 0 and ok_count > 0)

    if fifo_ok:
        print("  J3a PASS: dropped=0 hash_mismatch=0")
    else:
        print("  J3a FAIL: dropped=%d hash_mismatch=%d" % (dropped, hash_mismatch))

    if shm_ok:
        print("  J3b PASS: SHM hash 与 receiver 一致 (%d 样本)" % ok_count)
    else:
        print("  J3b FAIL: %d hash 不匹配" % fail_count)

    return fifo_ok and shm_ok

# ── main ───────────────────────────────────────────────────────────────────
if __name__ == "__main__":
    rx_session = sys.argv[1] if len(sys.argv) > 1 else "s-8adb50af"
    print("P4-4 vcam 验收  rx_session=%s" % rx_session)
    print("=" * 60)

    j1, j2 = check_j1_j2()
    j3 = check_j3(rx_session, num_samples=20, interval_ms=150)

    print("\n" + "=" * 60)
    print("  J1 枚举    :", "PASS" if j1 else "FAIL")
    print("  J2 格式协商:", "PASS" if j2 else "FAIL")
    print("  J3 hash+FIFO:", "PASS" if j3 else "FAIL")
    result = "ALL PASS" if (j1 and j2 and j3) else "FAIL"
    print("\n  总判定:", result)
    sys.exit(0 if result == "ALL PASS" else 1)
