// vc-vcam.cpp — P4-3 DShow source filter (user-mode COM, no driver signing).
// Push source: 1 output pin, NV12 640x640, AvgTimePerFrame=41666 (240fps).
// Frame source (bring-up): local integer pattern generator (sender/pattern.h motion,
// deterministic in fid). Receiver shared-memory feed replaces FillFrame() at PC2 wiring (P4-4).
// Registration: HKCU-only (no admin): CLSID + VideoInputDeviceCategory instance.
// Hand-rolled COM: no ATL, no strmbase (not in modern Windows SDK). Links strmiids + winmm.
#include <windows.h>
#include <dshow.h>
// IKsControl and ISpecifyPropertyPages declared inline below (no ATL/strmbase)
#include <cstring>
#include <cstdio>
#include <new>
#include <string>
#include <timeapi.h>  // timeBeginPeriod(1) for 240Hz pacing
#include "../sender/pattern.h"  // vc_gen_motion (integer-only)
#include "vcam_shm.h"  // receiver -> vcam latest-frame handoff (canonical layout)

// {B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}
#include <initguid.h>  // storage for the CLSID below only; system GUIDs stay extern (strmiids)
DEFINE_GUID(CLSID_VirtualCaptureCam,
  0xb84c7e21, 0x4a5f, 0x4e2b, 0x9c, 0x11, 0x5a, 0x3d, 0x7f, 0x01, 0x22, 0x9a);

// NV12 subtype value {3231564E-0000-0010-8000-00AA00389B71}, own name to avoid
// any strmiids duplicate-definition risk.
static const GUID VCAM_SUBTYPE_NV12 =
  {0x3231564e, 0x0000, 0x0010, {0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71}};

// ISpecifyPropertyPages already in ocidl.h (via dshow.h)
// IKsControl {28F54685-06FD-11D2-B27A-00A0C9223196}
static const GUID IID_IKsControl_VC =
  {0x28f54685,0x06fd,0x11d2,{0xb2,0x7a,0x00,0xa0,0xc9,0x22,0x31,0x96}};
DECLARE_INTERFACE_(IKsControlVC, IUnknown) {
    STDMETHOD(QueryInterface)(REFIID,void**) PURE;
    STDMETHOD_(ULONG,AddRef)() PURE;
    STDMETHOD_(ULONG,Release)() PURE;
    STDMETHOD_(LONG,KsProperty)(void*,ULONG,void*,ULONG,ULONG*) PURE;
    STDMETHOD_(LONG,KsMethod)(void*,ULONG,void*,ULONG,ULONG*) PURE;
    STDMETHOD_(LONG,KsEvent)(void*,ULONG,void*,ULONG,ULONG*) PURE;
};

static const GUID IID_ISpecifyPropertyPages_VC =
  {0xb196b28b,0xbab4,0x101a,{0xb6,0x9c,0x00,0xaa,0x00,0x34,0x1d,0x07}};
// IKsPin {B61178D1-A2D9-11CF-9E53-00AA00A216A1}
// OBS win-dshow uses QI(IKsPin) to classify pin as (video) vs (none).
// Stub: just respond to QI; all methods return E_NOTIMPL.
static const GUID IID_IKsPin_VC =
  {0xb61178d1,0xa2d9,0x11cf,{0x9e,0x53,0x00,0xaa,0x00,0xa2,0x16,0xa1}};
DECLARE_INTERFACE_(IKsPinVC, IUnknown) {
    STDMETHOD(QueryInterface)(REFIID,void**) PURE;
    STDMETHOD_(ULONG,AddRef)() PURE;
    STDMETHOD_(ULONG,Release)() PURE;
    STDMETHOD(KsQueryMediums)(void**) PURE;
    STDMETHOD(KsQueryInterfaces)(void**) PURE;
    STDMETHOD(KsCreateSinkPinHandle)(ULONG,ULONG) PURE;
    STDMETHOD(KsGetCurrentCommunication)(void*,void*,void*) PURE;
    STDMETHOD(KsPropagateAcquire)() PURE;
    STDMETHOD(KsDeliver)(void*,ULONG) PURE;
    STDMETHOD(KsMediaSamplesCompleted)(void*) PURE;
    STDMETHOD_(void*,KsPeekAllocator)(ULONG) PURE;
    STDMETHOD(KsReceiveAllocator)(void*) PURE;
    STDMETHOD(KsRenegotiate)() PURE;
    STDMETHOD(KsPassEventToFilter)(void*) PURE;
    STDMETHOD(KsSetPinFramingCache)(void*,void*,ULONG) PURE;
};

// FriendlyName shown by DShow clients (OBS/ffmpeg/GraphEdit), styled after the
// hardware card "AVerMedia HD Capture GC573 1" whose single output pin is "Capture".
static const wchar_t* VCAM_NAME = L"VirtualCapture";
static const wchar_t* VCAM_PIN_ID = L"Capture";
static const wchar_t* VCAM_CLSID_KEY =
  L"Software\\Classes\\CLSID\\{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}";
static const wchar_t* VCAM_SHM_DEFAULT = L"Local\\VirtualCaptureVcamFrame";
static const LONG VCAM_W = 640, VCAM_H = 640;
static const GUID VCAM_AMPROPSETID_Pin =
  // AMPROPSETID_Pin per SDK shared/uuids.h. The tail was previously mistyped
  // (00a024cbed26), so every client's pin-category query failed and ffmpeg/OBS
  // classified the device as "(none)" / "Could not find output pin".
  {0x9b00f101, 0x1567, 0x11d1, {0xb3, 0xf1, 0x00, 0xaa, 0x00, 0x37, 0x61, 0xc5}};
static const GUID VCAM_PIN_CATEGORY_CAPTURE =
  {0xfb6c4281, 0x0353, 0x11d1, {0x90, 0x5f, 0x00, 0x00, 0xc0, 0xcc, 0x16, 0xba}};
#define VCAM_AMPROPERTY_PIN_CATEGORY 0
#define VCAM_KSPROPERTY_SUPPORT_GET 1
static const LONGLONG VCAM_FRAME_100NS = 41666;  // 240fps
static const LONG VCAM_FRAME_BYTES = 640 * 640 * 3 / 2;

static HMODULE g_hMod = nullptr;
static LONG g_locks = 0;
static LONG g_objs = 0;
// ---------- media-type helpers ----------
static AM_MEDIA_TYPE* VCamCreateType() {
  AM_MEDIA_TYPE* mt = (AM_MEDIA_TYPE*)CoTaskMemAlloc(sizeof(AM_MEDIA_TYPE));
  if (!mt) return nullptr;
  ZeroMemory(mt, sizeof(*mt));
  VIDEOINFOHEADER* vih = (VIDEOINFOHEADER*)CoTaskMemAlloc(sizeof(VIDEOINFOHEADER));
  if (!vih) { CoTaskMemFree(mt); return nullptr; }
  ZeroMemory(vih, sizeof(*vih));
  vih->rcSource.right = VCAM_W; vih->rcSource.bottom = VCAM_H;
  vih->rcTarget.right = VCAM_W; vih->rcTarget.bottom = VCAM_H;
  vih->dwBitRate = (DWORD)(VCAM_FRAME_BYTES * 8 * 240);
  vih->AvgTimePerFrame = VCAM_FRAME_100NS;
  BITMAPINFOHEADER& b = vih->bmiHeader;
  b.biSize = sizeof(b); b.biWidth = VCAM_W; b.biHeight = VCAM_H;
  b.biPlanes = 1; b.biBitCount = 12; b.biCompression = MAKEFOURCC('N', 'V', '1', '2');
  b.biSizeImage = (DWORD)VCAM_FRAME_BYTES;
  mt->majortype = MEDIATYPE_Video;
  mt->subtype = VCAM_SUBTYPE_NV12;
  mt->bFixedSizeSamples = TRUE;
  mt->bTemporalCompression = FALSE;
  mt->lSampleSize = (ULONG)VCAM_FRAME_BYTES;
  mt->formattype = FORMAT_VideoInfo;
  mt->cbFormat = sizeof(VIDEOINFOHEADER);
  mt->pbFormat = (BYTE*)vih;
  return mt;
}
static void VCamClearType(AM_MEDIA_TYPE* mt) {
  // frees internals only; struct itself may be embedded (never CoTaskMemFree here)
  if (!mt) return;
  if (mt->cbFormat && mt->pbFormat) CoTaskMemFree(mt->pbFormat);
  if (mt->pUnk) mt->pUnk->Release();
  ZeroMemory(mt, sizeof(*mt));
}
static void VCamFreeType(AM_MEDIA_TYPE* mt) {
  if (!mt) return;
  if (mt->cbFormat && mt->pbFormat) CoTaskMemFree(mt->pbFormat);
  if (mt->pUnk) mt->pUnk->Release();
  CoTaskMemFree(mt);
}
static HRESULT VCamCopyType(AM_MEDIA_TYPE* dst, const AM_MEDIA_TYPE* src) {
  ZeroMemory(dst, sizeof(*dst));
  *dst = *src;
  dst->pUnk = nullptr; dst->pbFormat = nullptr; dst->cbFormat = 0;
  if (src->pUnk) { src->pUnk->AddRef(); dst->pUnk = src->pUnk; }
  if (src->cbFormat && src->pbFormat) {
    dst->pbFormat = (BYTE*)CoTaskMemAlloc(src->cbFormat);
    if (!dst->pbFormat) { if (dst->pUnk) dst->pUnk->Release(); return E_OUTOFMEMORY; }
    memcpy(dst->pbFormat, src->pbFormat, src->cbFormat);
    dst->cbFormat = src->cbFormat;
  }
  return S_OK;
}
static BOOL VCamTypeMatches(const AM_MEDIA_TYPE* mt) {
  if (!mt) return FALSE;
  if (mt->majortype != GUID_NULL && mt->majortype != MEDIATYPE_Video) return FALSE;
  if (mt->subtype != GUID_NULL && memcmp(&mt->subtype, &VCAM_SUBTYPE_NV12, sizeof(GUID)) != 0) return FALSE;
  if (mt->formattype != GUID_NULL && mt->formattype != FORMAT_VideoInfo) return FALSE;
  if (mt->formattype == FORMAT_VideoInfo && mt->cbFormat >= sizeof(VIDEOINFOHEADER) && mt->pbFormat) {
    const VIDEOINFOHEADER* v = (const VIDEOINFOHEADER*)mt->pbFormat;
    if (v->bmiHeader.biWidth != VCAM_W || v->bmiHeader.biHeight != VCAM_H) return FALSE;
    if (v->bmiHeader.biCompression != (DWORD)MAKEFOURCC('N', 'V', '1', '2')) return FALSE;
  }
  return TRUE;
}

// ---------- forward decls ----------
class CVCamFilter;
class CVCamPin;

// ---------- tiny enum helpers ----------
class CEnumPins : public IEnumPins {
  LONG m_cRef = 1; CVCamPin* m_pin; BOOL m_done = FALSE;
public:
  explicit CEnumPins(CVCamPin* p) : m_pin(p) {}
  STDMETHODIMP QueryInterface(REFIID riid, void** ppv) {
    if (!ppv) return E_POINTER;
    if (riid == IID_IUnknown || riid == IID_IEnumPins) { *ppv = (IEnumPins*)this; AddRef(); return S_OK; }
    *ppv = nullptr; return E_NOINTERFACE;
  }
  STDMETHODIMP_(ULONG) AddRef() { return (ULONG)InterlockedIncrement(&m_cRef); }
  STDMETHODIMP_(ULONG) Release() {
    LONG n = InterlockedDecrement(&m_cRef);
    if (!n) delete this; return (ULONG)n;
  }
  STDMETHODIMP Next(ULONG c, IPin** pp, ULONG* pc);
  STDMETHODIMP Skip(ULONG c) {
    if (c == 0) return S_OK;
    if (c == 1 && !m_done) { m_done = TRUE; return S_OK; }
    return S_FALSE;
  }
  STDMETHODIMP Reset() { m_done = FALSE; return S_OK; }
  STDMETHODIMP Clone(IEnumPins** pp) {
    if (!pp) return E_POINTER;
    CEnumPins* e = new (std::nothrow) CEnumPins(m_pin);
    if (!e) return E_OUTOFMEMORY;
    e->m_done = m_done; *pp = e; return S_OK;
  }
};
class CEnumMT : public IEnumMediaTypes {
  LONG m_cRef = 1; BOOL m_done = FALSE;
public:
  CEnumMT() {}
  STDMETHODIMP QueryInterface(REFIID riid, void** ppv) {
    if (!ppv) return E_POINTER;
    if (riid == IID_IUnknown || riid == IID_IEnumMediaTypes) { *ppv = (IEnumMediaTypes*)this; AddRef(); return S_OK; }
    *ppv = nullptr; return E_NOINTERFACE;
  }
  STDMETHODIMP_(ULONG) AddRef() { return (ULONG)InterlockedIncrement(&m_cRef); }
  STDMETHODIMP_(ULONG) Release() {
    LONG n = InterlockedDecrement(&m_cRef);
    if (!n) delete this; return (ULONG)n;
  }
  STDMETHODIMP Next(ULONG c, AM_MEDIA_TYPE** pp, ULONG* pc) {
    if (!pp) return E_POINTER;
    ULONG got = 0;
    while (got < c && !m_done) {
      AM_MEDIA_TYPE* t = VCamCreateType();
      if (!t) { for (ULONG i = 0; i < got; i++) VCamFreeType(pp[i]); return E_OUTOFMEMORY; }
      pp[got++] = t; m_done = TRUE;
    }
    if (pc) *pc = got;
    return got == c ? S_OK : S_FALSE;
  }
  STDMETHODIMP Skip(ULONG c) {
    if (c == 0) return S_OK;
    if (c == 1 && !m_done) { m_done = TRUE; return S_OK; }
    return S_FALSE;
  }
  STDMETHODIMP Reset() { m_done = FALSE; return S_OK; }
  STDMETHODIMP Clone(IEnumMediaTypes** pp) {
    if (!pp) return E_POINTER;
    CEnumMT* e = new (std::nothrow) CEnumMT();
    if (!e) return E_OUTOFMEMORY;
    e->m_done = m_done; *pp = e; return S_OK;
  }
};

// ---------- filter ----------
class CVCamFilter : public IBaseFilter, public IAMFilterMiscFlags {
  friend class CVCamPin;
  LONG m_cRef = 1;
  CRITICAL_SECTION m_cs;
  FILTER_STATE m_state = State_Stopped;
  IFilterGraph* m_pGraph = nullptr;
  IReferenceClock* m_pClock = nullptr;
  CVCamPin* m_pPin = nullptr;
  HANDLE m_hShmMap = nullptr;     // receiver frame mapping
  uint8_t* m_shmView = nullptr;
  HANDLE m_hEvt[2] = { nullptr, nullptr };  // shm v2 publish events (<name>.evt0/1)
  HANDLE m_hVstatMap = nullptr;   // vcam -> receiver latency feedback ring (<name>.vcam)
  uint8_t* m_vstat = nullptr;
  REFERENCE_TIME m_tStart = 0;    // Run(tStart): stream time base for sample timestamps
  void VstatPush(uint32_t fid, uint64_t pub, uint64_t del, uint32_t flags);
  uint8_t* m_lastGoodFrame = nullptr; // D12: last successfully read frame (repeat-on-stall)
  bool m_hasLastGood = false;     // D12: true after first clean shm read in this session
  bool m_motionEnable = false;    // D12: explicit bring-up flag; false = stall when no receiver
  HANDLE m_hThread = nullptr;
  HANDLE m_hStop = nullptr;
  IMemInputPin* Input() ;
  static DWORD WINAPI ThreadProc(LPVOID p) { return ((CVCamFilter*)p)->Worker(); }
  DWORD Worker();
  HRESULT Activate();
  void Deactivate();
public:
  CVCamFilter();
  ~CVCamFilter();
  void Lock() { EnterCriticalSection(&m_cs); }
  void Unlock() { LeaveCriticalSection(&m_cs); }
  // IUnknown
  STDMETHODIMP QueryInterface(REFIID riid, void** ppv) {
    if (!ppv) return E_POINTER;
    if (riid == IID_IUnknown) *ppv = (IBaseFilter*)this;
    else if (riid == IID_IPersist) *ppv = (IPersist*)this;
    else if (riid == IID_IMediaFilter) *ppv = (IMediaFilter*)this;
    else if (riid == IID_IBaseFilter) *ppv = (IBaseFilter*)this;
    else if (riid == IID_IAMFilterMiscFlags) *ppv = (IAMFilterMiscFlags*)this;
    else { *ppv = nullptr; return E_NOINTERFACE; }
    AddRef(); return S_OK;
  }
  STDMETHODIMP_(ULONG) AddRef() { return (ULONG)InterlockedIncrement(&m_cRef); }
  STDMETHODIMP_(ULONG) Release() {
    LONG n = InterlockedDecrement(&m_cRef);
    if (!n) { InterlockedDecrement(&g_objs); delete this; } return (ULONG)n;
  }
  // IPersist
  STDMETHODIMP GetClassID(CLSID* p) { if (!p) return E_POINTER; *p = CLSID_VirtualCaptureCam; return S_OK; }
  void ShmEnsureOpen();
  void ShmClose();
  // IMediaFilter
  STDMETHODIMP Stop();
  STDMETHODIMP Pause();
  STDMETHODIMP Run(REFERENCE_TIME) ;
  STDMETHODIMP GetState(DWORD, FILTER_STATE* p) {
    if (!p) return E_POINTER;
    Lock(); *p = m_state; Unlock();
    return m_state == State_Paused ? VFW_S_CANT_CUE : S_OK;
  }
  STDMETHODIMP SetSyncSource(IReferenceClock* p) {
    Lock();
    if (m_pClock) m_pClock->Release();
    m_pClock = p; if (m_pClock) m_pClock->AddRef();
    Unlock(); return S_OK;
  }
  STDMETHODIMP GetSyncSource(IReferenceClock** p) {
    if (!p) return E_POINTER;
    Lock(); *p = m_pClock; if (*p) (*p)->AddRef(); Unlock(); return S_OK;
  }
  // IBaseFilter
  STDMETHODIMP EnumPins(IEnumPins** pp);
  STDMETHODIMP FindPin(LPCWSTR id, IPin** pp);
  STDMETHODIMP QueryFilterInfo(FILTER_INFO* p);
  STDMETHODIMP JoinFilterGraph(IFilterGraph* p, LPCWSTR) {
    Lock();
    if (m_pGraph) m_pGraph->Release();
    m_pGraph = p; if (m_pGraph) m_pGraph->AddRef();
    Unlock(); return S_OK;
  }
  STDMETHODIMP QueryPinVersion(LONG*) { return E_NOTIMPL; }
  STDMETHODIMP QueryVendorInfo(LPWSTR*) { return E_NOTIMPL; }
  BOOL HasPin() { return m_pPin != nullptr; }
  // IAMFilterMiscFlags
  STDMETHODIMP_(ULONG) GetMiscFlags() { return AM_FILTER_MISC_FLAGS_IS_SOURCE; }
};

// ---------- pin ----------
class CVCamPin : public IPin, public IAMStreamConfig,
                 public IKsPropertySet, public ISpecifyPropertyPages,
                 public IKsControlVC, public IKsPinVC {
  friend class CVCamFilter;
  LONG m_cRef = 1;
  CVCamFilter* m_pFilter;  // owning filter, ref'd
  IPin* m_pPeer = nullptr;
  IMemInputPin* m_pInput = nullptr;
  IMemAllocator* m_pAlloc = nullptr;
  AM_MEDIA_TYPE m_mt; BOOL m_connected = FALSE;
public:
  explicit CVCamPin(CVCamFilter* f) : m_pFilter(f) {
    m_pFilter->AddRef(); ZeroMemory(&m_mt, sizeof(m_mt));
  }
  ~CVCamPin() {
    VCamClearType(&m_mt);
    if (m_pAlloc) m_pAlloc->Release();
    if (m_pInput) m_pInput->Release();
    if (m_pPeer) m_pPeer->Release();
    m_pFilter->Release();
  }
  // IUnknown
  STDMETHODIMP QueryInterface(REFIID riid, void** ppv) {
    if (!ppv) return E_POINTER;
    if (riid == IID_IUnknown || riid == IID_IPin) *ppv = (IPin*)this;
    else if (riid == IID_IAMStreamConfig)         *ppv = (IAMStreamConfig*)this;
    else if (riid == IID_IKsPropertySet)          *ppv = (IKsPropertySet*)this;
    else if (riid == IID_ISpecifyPropertyPages_VC)*ppv = (ISpecifyPropertyPages*)this;
    else if (riid == IID_IKsControl_VC)           *ppv = (IKsControlVC*)this;
    else if (riid == IID_IKsPin_VC)               *ppv = (IKsPinVC*)this;
    else { *ppv = nullptr; return E_NOINTERFACE; }
    AddRef(); return S_OK;
  }
  STDMETHODIMP_(ULONG) AddRef() { return (ULONG)InterlockedIncrement(&m_cRef); }
  STDMETHODIMP_(ULONG) Release() {
    LONG n = InterlockedDecrement(&m_cRef);
    if (!n) delete this; return (ULONG)n;
  }
  // IPin
  STDMETHODIMP Connect(IPin* pPeer, const AM_MEDIA_TYPE* pmt);
  STDMETHODIMP ReceiveConnection(IPin*, const AM_MEDIA_TYPE*) { return E_UNEXPECTED; }
  STDMETHODIMP Disconnect();
  STDMETHODIMP ConnectedTo(IPin** pp) {
    if (!pp) return E_POINTER;
    m_pFilter->Lock();
    *pp = m_pPeer; if (*pp) (*pp)->AddRef();
    HRESULT hr = *pp ? S_OK : VFW_E_NOT_CONNECTED;
    m_pFilter->Unlock(); return hr;
  }
  STDMETHODIMP ConnectionMediaType(AM_MEDIA_TYPE* p) {
    if (!p) return E_POINTER;
    m_pFilter->Lock();
    HRESULT hr = m_connected ? VCamCopyType(p, &m_mt) : VFW_E_NOT_CONNECTED;
    m_pFilter->Unlock(); return hr;
  }
  STDMETHODIMP QueryPinInfo(PIN_INFO* p) {
    if (!p) return E_POINTER;
    p->pFilter = m_pFilter; m_pFilter->AddRef();
    p->dir = PINDIR_OUTPUT;
    wcscpy_s(p->achName, VCAM_PIN_ID); return S_OK;
  }
  STDMETHODIMP QueryDirection(PIN_DIRECTION* p) { if (!p) return E_POINTER; *p = PINDIR_OUTPUT; return S_OK; }
  STDMETHODIMP QueryId(LPWSTR* p) {
    if (!p) return E_POINTER;
    size_t n = (wcslen(VCAM_PIN_ID) + 1) * sizeof(wchar_t);
    *p = (LPWSTR)CoTaskMemAlloc(n);
    if (!*p) return E_OUTOFMEMORY;
    memcpy(*p, VCAM_PIN_ID, n); return S_OK;
  }
  STDMETHODIMP QueryAccept(const AM_MEDIA_TYPE* p) {
    if (!p) return E_POINTER;
    return VCamTypeMatches(p) ? S_OK : S_FALSE;
  }
  STDMETHODIMP EnumMediaTypes(IEnumMediaTypes** pp) {
    if (!pp) return E_POINTER;
    *pp = new (std::nothrow) CEnumMT();
    return *pp ? S_OK : E_OUTOFMEMORY;
  }
  STDMETHODIMP QueryInternalConnections(IPin**, ULONG*) { return E_NOTIMPL; }
  STDMETHODIMP EndOfStream() { return S_OK; }
  STDMETHODIMP BeginFlush() { return S_OK; }
  STDMETHODIMP EndFlush() { return S_OK; }
  STDMETHODIMP NewSegment(REFERENCE_TIME, REFERENCE_TIME, double) { return S_OK; }
  // IAMStreamConfig — expose 3 caps entries (30/60/240 fps) so clients
  // populate their resolution/fps dropdowns instead of showing "no capabilities".
  // All three use the same 640x640 NV12 format; only AvgTimePerFrame differs.
  static const int kNumCaps = 3;
  static constexpr LONGLONG kFrameIntervals[3] = {
    416666LL,  // ~24fps
    333333LL,  // 30fps
    41666LL,   // 240fps
  };
  STDMETHODIMP GetNumberOfCapabilities(int* piCount, int* piSize) {
    if (!piCount || !piSize) return E_POINTER;
    *piCount = kNumCaps; *piSize = sizeof(VIDEO_STREAM_CONFIG_CAPS); return S_OK;
  }
  STDMETHODIMP SetFormat(AM_MEDIA_TYPE* p) {
    if (!p) return E_POINTER;
    return VCamTypeMatches(p) ? S_OK : VFW_E_TYPE_NOT_ACCEPTED;
  }
  STDMETHODIMP GetFormat(AM_MEDIA_TYPE** pp) {
    if (!pp) return E_POINTER;
    AM_MEDIA_TYPE* t = VCamCreateType();
    if (!t) return E_OUTOFMEMORY;
    *pp = t; return S_OK;
  }
  // dshow, GraphEdit) accept this pin as the capture pin.
  STDMETHODIMP Set(REFGUID, DWORD, LPVOID, DWORD, LPVOID, DWORD) {
    return E_PROP_ID_UNSUPPORTED;
  }
  STDMETHODIMP Get(REFGUID rguid, DWORD dwProp, LPVOID pIn, DWORD cbIn, LPVOID pOut,
                   DWORD cbOut, DWORD* pcb) {
    int match = (memcmp(&rguid, &VCAM_AMPROPSETID_Pin, sizeof(GUID)) == 0 &&
                 dwProp == VCAM_AMPROPERTY_PIN_CATEGORY);
    HRESULT hr = E_PROP_ID_UNSUPPORTED;
    if (match) {
      if (cbOut < sizeof(GUID) || !pOut || !pcb) hr = E_INVALIDARG;
      else {
        memcpy(pOut, &VCAM_PIN_CATEGORY_CAPTURE, sizeof(GUID));
        *pcb = sizeof(GUID);
        hr = S_OK;
      }
    }
    return hr;
  }
  STDMETHODIMP QuerySupported(REFGUID rguid, DWORD dwProp, DWORD* pType) {
    if (!pType) return E_POINTER;
    if (memcmp(&rguid, &VCAM_AMPROPSETID_Pin, sizeof(GUID)) == 0 &&
        dwProp == VCAM_AMPROPERTY_PIN_CATEGORY) {
      *pType = VCAM_KSPROPERTY_SUPPORT_GET;
      return S_OK;
    }
    return E_PROP_ID_UNSUPPORTED;
  }
  // ISpecifyPropertyPages: no property pages (empty CAUUID)
  STDMETHODIMP GetPages(CAUUID* pPages) {
    if (!pPages) return E_POINTER;
    pPages->cElems = 0; pPages->pElems = nullptr; return S_OK;
  }

  // IKsPin: stub — presence is enough; OBS uses QI to classify pin as (video).
  STDMETHODIMP KsQueryMediums(void** p)           { if(p)*p=nullptr; return E_NOTIMPL; }
  STDMETHODIMP KsQueryInterfaces(void** p)        { if(p)*p=nullptr; return E_NOTIMPL; }
  STDMETHODIMP KsCreateSinkPinHandle(ULONG,ULONG) { return E_NOTIMPL; }
  STDMETHODIMP KsGetCurrentCommunication(void*,void*,void*) { return E_NOTIMPL; }
  STDMETHODIMP KsPropagateAcquire()               { return E_NOTIMPL; }
  STDMETHODIMP KsDeliver(void*,ULONG)             { return E_NOTIMPL; }
  STDMETHODIMP KsMediaSamplesCompleted(void*)     { return E_NOTIMPL; }
  STDMETHODIMP_(void*) KsPeekAllocator(ULONG)     { return nullptr; }
  STDMETHODIMP KsReceiveAllocator(void*)          { return E_NOTIMPL; }
  STDMETHODIMP KsRenegotiate()                    { return E_NOTIMPL; }
  STDMETHODIMP KsPassEventToFilter(void*)         { return E_NOTIMPL; }
  STDMETHODIMP KsSetPinFramingCache(void*,void*,ULONG) { return E_NOTIMPL; }

  // IKsControl: stub — return STATUS_NOT_SUPPORTED (0xC00000BB).
  // Presence of this interface is enough for clients that QI-check it.
  STDMETHODIMP_(LONG) KsProperty(void*,ULONG,void*,ULONG,ULONG* b)
    { if(b)*b=0; return (LONG)0xC00000BBL; }
  STDMETHODIMP_(LONG) KsMethod(void*,ULONG,void*,ULONG,ULONG* b)
    { if(b)*b=0; return (LONG)0xC00000BBL; }
  STDMETHODIMP_(LONG) KsEvent(void*,ULONG,void*,ULONG,ULONG* b)
    { if(b)*b=0; return (LONG)0xC00000BBL; }

  // GetStreamCaps: 3 entries (30/60/240fps) so clients populate dropdowns
  STDMETHODIMP GetStreamCaps(int i, AM_MEDIA_TYPE** pp, BYTE* caps) {
    if (i < 0 || i >= kNumCaps || !pp || !caps) return E_INVALIDARG;
    AM_MEDIA_TYPE* t = VCamCreateType();
    if (!t) return E_OUTOFMEMORY;
    // Override AvgTimePerFrame for each capability entry
    VIDEOINFOHEADER* vih = (VIDEOINFOHEADER*)t->pbFormat;
    LONGLONG fi = kFrameIntervals[i];
    vih->AvgTimePerFrame = fi;
    vih->dwBitRate = (DWORD)(VCAM_FRAME_BYTES * 8LL * (10000000LL / fi));
    VIDEO_STREAM_CONFIG_CAPS* c = (VIDEO_STREAM_CONFIG_CAPS*)caps;
    ZeroMemory(c, sizeof(*c));
    c->guid                = FORMAT_VideoInfo;
    c->VideoStandard       = AnalogVideo_None;
    c->InputSize.cx        = VCAM_W; c->InputSize.cy = VCAM_H;
    c->MinCroppingSize.cx  = VCAM_W; c->MinCroppingSize.cy = VCAM_H;
    c->MaxCroppingSize.cx  = VCAM_W; c->MaxCroppingSize.cy = VCAM_H;
    c->CropGranularityX    = 1; c->CropGranularityY = 1;
    c->CropAlignX          = 1; c->CropAlignY = 1;
    c->MinOutputSize.cx    = VCAM_W; c->MinOutputSize.cy = VCAM_H;
    c->MaxOutputSize.cx    = VCAM_W; c->MaxOutputSize.cy = VCAM_H;
    c->OutputGranularityX  = 1; c->OutputGranularityY = 1;
    c->MinFrameInterval    = fi;
    c->MaxFrameInterval    = fi;
    c->MinBitsPerSecond    = (LONG)vih->dwBitRate;
    c->MaxBitsPerSecond    = (LONG)vih->dwBitRate;
    *pp = t; return S_OK;
  }
}; // end class CVCamPin

STDMETHODIMP CEnumPins::Next(ULONG c, IPin** pp, ULONG* pc) {
  if (!pp) return E_POINTER;
  ULONG got = 0;
  if (!m_done && c > 0) {
    pp[0] = m_pin; m_pin->AddRef(); got = 1; m_done = TRUE;
  }
  if (pc) *pc = got;
  return got == c ? S_OK : S_FALSE;
}

// ---------- filter impl ----------
CVCamFilter::CVCamFilter() {
  InitializeCriticalSection(&m_cs);
  InterlockedIncrement(&g_objs);
  m_pPin = new (std::nothrow) CVCamPin(this);
  m_lastGoodFrame = new (std::nothrow) uint8_t[VCAM_FRAME_BYTES];  // D12
}
void CVCamFilter::ShmEnsureOpen() {
  // called with m_cs held; early-out once mapped and valid
  if (m_shmView) return;
  std::wstring name = VCAM_SHM_DEFAULT;
  HKEY k = nullptr;
  if (RegOpenKeyEx(HKEY_CURRENT_USER, VCAM_CLSID_KEY, 0, KEY_READ, &k) == ERROR_SUCCESS) {
    wchar_t v[260]; DWORD cb = sizeof(v), tp = 0;
    if (RegQueryValueEx(k, L"FrameMap", nullptr, &tp, (BYTE*)v, &cb) == ERROR_SUCCESS && tp == REG_SZ)
      name = v;
    // D12 explicit bring-up switch: REG_DWORD Motion=1 -> motion frames when no receiver feed
    DWORD mv = 0; cb = sizeof(mv);
    if (RegQueryValueEx(k, L"Motion", nullptr, &tp, (BYTE*)&mv, &cb) == ERROR_SUCCESS && tp == REG_DWORD)
      m_motionEnable = (mv != 0);
    RegCloseKey(k);
  }
  if (!m_hShmMap) {
    m_hShmMap = OpenFileMappingW(FILE_MAP_READ, FALSE, name.c_str());
    if (!m_hShmMap) return;  // receiver not publishing (yet)
  }
  // a v1 (single-slot, smaller) mapping fails here -> treated as "no feed", retried next tick
  m_shmView = (uint8_t*)MapViewOfFile(m_hShmMap, FILE_MAP_READ, 0, 0, VC_SHM_SIZE);
  vc_shm_hdr_t* h = (vc_shm_hdr_t*)m_shmView;
  if (!m_shmView || h->magic != VC_SHM_MAGIC || h->version != VC_SHM_VERSION ||
      h->slots != VC_SHM_SLOTS || h->frame_bytes != (uint32_t)VC_FRAME_BYTES || h->data_off != VC_SHM_DATA_OFF) {
    ShmClose(); return;
  }
  for (int i = 0; i < 2; i++) {
    std::wstring en = name + (i ? L".evt1" : L".evt0");
    m_hEvt[i] = OpenEventW(SYNCHRONIZE, FALSE, en.c_str());
  }
  if (!m_vstat) {
    std::wstring vn = name + L".vcam";
    m_hVstatMap = CreateFileMappingW(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE, 0, (DWORD)VC_VSTAT_SIZE, vn.c_str());
    if (m_hVstatMap) m_vstat = (uint8_t*)MapViewOfFile(m_hVstatMap, FILE_MAP_WRITE, 0, 0, VC_VSTAT_SIZE);
    if (m_vstat) {
      vc_vstat_hdr_t* vh = (vc_vstat_hdr_t*)m_vstat;
      if (vh->magic != VC_VSTAT_MAGIC) { vh->cap = VC_VSTAT_CAP; MemoryBarrier(); vh->magic = VC_VSTAT_MAGIC; }
    } else if (m_hVstatMap) { CloseHandle(m_hVstatMap); m_hVstatMap = nullptr; }
  }
}
void CVCamFilter::ShmClose() {
  // called with the worker joined, from the worker (m_cs held), or dtor
  if (m_shmView) { UnmapViewOfFile(m_shmView); m_shmView = nullptr; }
  if (m_hShmMap) { CloseHandle(m_hShmMap); m_hShmMap = nullptr; }
  for (auto& e : m_hEvt) if (e) { CloseHandle(e); e = nullptr; }
  if (m_vstat) { UnmapViewOfFile(m_vstat); m_vstat = nullptr; }
  if (m_hVstatMap) { CloseHandle(m_hVstatMap); m_hVstatMap = nullptr; }
}
CVCamFilter::~CVCamFilter() {
  Deactivate();
  ShmClose();
  if (m_lastGoodFrame) { delete[] m_lastGoodFrame; m_lastGoodFrame = nullptr; }  // D12
  if (m_pPin) m_pPin->Release();
  if (m_pClock) m_pClock->Release();
  if (m_pGraph) m_pGraph->Release();
  DeleteCriticalSection(&m_cs);
}
IMemInputPin* CVCamFilter::Input() {
  // called with m_cs held
  return m_pPin ? m_pPin->m_pInput : nullptr;
}
STDMETHODIMP CVCamFilter::EnumPins(IEnumPins** pp) {
  if (!pp) return E_POINTER;
  *pp = new (std::nothrow) CEnumPins(m_pPin);
  return *pp ? S_OK : E_OUTOFMEMORY;
}
STDMETHODIMP CVCamFilter::FindPin(LPCWSTR id, IPin** pp) {
  if (!id || !pp) return E_POINTER;
  if (wcscmp(id, VCAM_PIN_ID) != 0) { *pp = nullptr; return VFW_E_NOT_FOUND; }
  *pp = m_pPin; m_pPin->AddRef(); return S_OK;
}
STDMETHODIMP CVCamFilter::QueryFilterInfo(FILTER_INFO* p) {
  if (!p) return E_POINTER;
  Lock();
  if (m_pGraph) m_pGraph->AddRef();
  p->pGraph = m_pGraph;
  wcscpy_s(p->achName, VCAM_NAME);
  Unlock(); return S_OK;
}
HRESULT CVCamFilter::Activate() {
  Lock();
  if (m_hThread) { Unlock(); return S_OK; }
  IMemAllocator* alloc = m_pPin ? m_pPin->m_pAlloc : nullptr;
  if (alloc) alloc->Commit();
  m_hStop = CreateEvent(nullptr, TRUE, FALSE, nullptr);
  if (!m_hStop) { Unlock(); return E_FAIL; }
  DWORD tid = 0;
  m_hThread = CreateThread(nullptr, 0, ThreadProc, this, 0, &tid);
  if (!m_hThread) { CloseHandle(m_hStop); m_hStop = nullptr; if (alloc) alloc->Decommit(); Unlock(); return E_FAIL; }
  Unlock(); return S_OK;
}
void CVCamFilter::Deactivate() {
  HANDLE th = nullptr, ev = nullptr;
  IMemAllocator* alloc = nullptr;
  Lock();
  th = m_hThread; m_hThread = nullptr;
  ev = m_hStop; m_hStop = nullptr;
  if (m_pPin && m_pPin->m_pAlloc) { alloc = m_pPin->m_pAlloc; alloc->AddRef(); }
  Unlock();
  // Decommit first: unblocks a worker stuck in GetBuffer; join is bounded after that.
  if (alloc) { alloc->Decommit(); alloc->Release(); }
  if (ev) SetEvent(ev);
  if (th) { WaitForSingleObject(th, 2000); CloseHandle(th); }
  if (ev) CloseHandle(ev);
  ShmClose();  // worker joined: drop receiver mapping (re-resolves on next Run)
  m_hasLastGood = false;  // D12: reset per-session last-frame state
}
STDMETHODIMP CVCamFilter::Stop() {
  Deactivate();
  Lock(); m_state = State_Stopped; Unlock(); return S_OK;
}
STDMETHODIMP CVCamFilter::Pause() {
  HRESULT hr = Activate();
  if (FAILED(hr)) return hr;
  Lock(); m_state = State_Paused; Unlock(); return S_OK;
}
STDMETHODIMP CVCamFilter::Run(REFERENCE_TIME tStart) {
  HRESULT hr = Activate();
  if (FAILED(hr)) return hr;
  Lock(); m_tStart = tStart; m_state = State_Running; Unlock(); return S_OK;
}
// 240Hz pacing only for the no-feed path; with a receiver feed the worker is event-driven.
static uint64_t VCamQpcFreq() {
  LARGE_INTEGER f; QueryPerformanceFrequency(&f); return (uint64_t)f.QuadPart;
}
static uint64_t VCamQpcNs(uint64_t freq) {
  LARGE_INTEGER c; QueryPerformanceCounter(&c);
  return vc_qpc_to_ns((uint64_t)c.QuadPart, freq);  // same clock/conversion as receiver qpc_pub_ns
}
// Stable latest publish -> (slot, seq) token. Read-only view: plain aligned loads only (D11).
static bool VCamPeek(uint8_t* view, uint32_t& slot, LONG& seq) {
  vc_shm_hdr_t* h = (vc_shm_hdr_t*)view;
  if (h->magic != VC_SHM_MAGIC) return false;
  LONG i = *(volatile LONG*)&h->latest_slot;
  if (i < 0 || i >= (LONG)VC_SHM_SLOTS) return false;
  LONG s = *(volatile LONG*)&vc_shm_slot(view, (uint32_t)i)->seq;
  if (s & 1) return false;
  slot = (uint32_t)i; seq = s; return true;
}
// Seqlock copy of one slot straight into dst; false = torn (writer reused the slot meanwhile).
static bool VCamCopySlot(uint8_t* view, uint32_t slot, LONG seq, uint8_t* dst, uint32_t& fid, uint64_t& pub) {
  vc_shm_slot_t* sl = vc_shm_slot(view, slot);
  fid = *(volatile uint32_t*)&sl->frame_id; pub = *(volatile uint64_t*)&sl->qpc_pub_ns;
  MemoryBarrier();
  memcpy(dst, vc_shm_data(view, slot), VC_FRAME_BYTES);
  MemoryBarrier();
  return *(volatile LONG*)&sl->seq == seq;
}
void CVCamFilter::VstatPush(uint32_t fid, uint64_t pub, uint64_t del, uint32_t flags) {
  if (!m_vstat) return;
  vc_vstat_hdr_t* h = (vc_vstat_hdr_t*)m_vstat;
  LONG k = InterlockedIncrement((volatile LONG*)&h->widx) - 1;
  vc_vstat_entry_t* e = (vc_vstat_entry_t*)(m_vstat + sizeof(vc_vstat_hdr_t)) + ((uint32_t)k % VC_VSTAT_CAP);
  e->n = 0; MemoryBarrier();
  e->frame_id = fid; e->pub_ns = pub; e->deliver_ns = del; e->flags = flags;
  MemoryBarrier(); e->n = (uint32_t)k + 1;
}
DWORD CVCamFilter::Worker() {
  static uint8_t Y[VC_Y_BYTES], UV[VC_UV_BYTES];  // motion bring-up planes
  uint32_t fid = 0;
  LONGLONG tcount = 0, lastStart = -1;
  BOOL discont = TRUE;
  const uint64_t freq = VCamQpcFreq();
  const uint64_t FRAME_NS = 4166666ULL, FIRST_REPEAT_NS = 6250000ULL;  // repeat only after 1.5 frames of silence
  timeBeginPeriod(1);
  // MMCSS on our own worker thread only (never touch the host process priority class)
  typedef HANDLE (WINAPI *avset_fn)(LPCWSTR, LPDWORD);
  typedef BOOL (WINAPI *avrev_fn)(HANDLE);
  HMODULE av = LoadLibraryW(L"avrt.dll"); HANDLE mm = nullptr; DWORD task = 0;
  if (av) { avset_fn f = (avset_fn)GetProcAddress(av, "AvSetMmThreadCharacteristicsW"); if (f) mm = f(L"Capture", &task); }
  if (!mm) SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
  uint64_t next_ns = VCamQpcNs(freq);   // deadline for a repeat / motion / no-feed tick
  uint64_t lastTok = ~0ULL;              // (slot<<32)|seq of the last delivered publish
  uint32_t lastFid = 0; uint64_t lastPub = 0;
  for (;;) {
    if (WaitForSingleObject(m_hStop, 0) == WAIT_OBJECT_0) break;
    Lock(); ShmEnsureOpen(); uint8_t* view = m_shmView; HANDLE ev0 = m_hEvt[0], ev1 = m_hEvt[1]; Unlock();
    uint32_t slot = 0; LONG seq = 0; bool fresh = false;
    if (view) {
      // event-driven: deliver a new publish immediately, else sleep until publish or repeat deadline
      LONG ls = *(volatile LONG*)&((vc_shm_hdr_t*)view)->latest_seq;
      fresh = VCamPeek(view, slot, seq) && ((((uint64_t)slot) << 32) | (uint32_t)seq) != lastTok;
      if (!fresh) {
        uint64_t now = VCamQpcNs(freq);
        if (now < next_ns) {
          DWORD ms = (DWORD)((next_ns - now + 999999ULL) / 1000000ULL);
          HANDLE ev = (ls & 1) ? ev0 : ev1;    // evt[(ls+1)&1] = the NEXT publish
          HANDLE hs[2] = { m_hStop, ev };
          if (!ev && ms > 1) ms = 1;            // no events (should not happen): 1ms poll
          DWORD w = WaitForMultipleObjects(ev ? 2 : 1, hs, FALSE, ms);
          if (w == WAIT_OBJECT_0) break;
          fresh = VCamPeek(view, slot, seq) && ((((uint64_t)slot) << 32) | (uint32_t)seq) != lastTok;
          if (!fresh && VCamQpcNs(freq) < next_ns) continue;
        }
      }
    } else {
      // no receiver mapping: 240Hz cadence (D12 stall / last-good repeat / explicit motion)
      bool stopReq = false;
      for (;;) {
        uint64_t now = VCamQpcNs(freq);
        if (now >= next_ns) break;
        uint64_t rem_us = (next_ns - now) / 1000ULL;
        if (WaitForSingleObject(m_hStop, rem_us > 2500 ? 2 : (rem_us > 1100 ? 1 : 0)) == WAIT_OBJECT_0) { stopReq = true; break; }
        if (rem_us <= 1100) SwitchToThread();
      }
      if (stopReq) break;
    }
    // D12 policy: fresh frame > repeat last good (freeze) > explicit motion > stall (no output)
    if (!fresh && !m_hasLastGood && !m_motionEnable) {
      tcount += VCAM_FRAME_100NS; discont = TRUE;
      next_ns += FRAME_NS; { uint64_t n = VCamQpcNs(freq); if (n > next_ns + 20000000ULL) next_ns = n; }
      continue;
    }
    IMemInputPin* input = nullptr; IMemAllocator* alloc = nullptr; IReferenceClock* clk = nullptr;
    FILTER_STATE st = State_Stopped; REFERENCE_TIME t0 = 0;
    Lock();
    if (m_pPin && m_pPin->m_pInput) { input = m_pPin->m_pInput; input->AddRef(); }
    if (m_pPin && m_pPin->m_pAlloc) { alloc = m_pPin->m_pAlloc; alloc->AddRef(); }
    if (m_pClock) { clk = m_pClock; clk->AddRef(); }
    st = m_state; t0 = m_tStart;
    Unlock();
    if (!input || !alloc) { if (input) input->Release(); if (alloc) alloc->Release(); if (clk) clk->Release(); break; }
    IMediaSample* s = nullptr;
    HRESULT hr = alloc->GetBuffer(&s, nullptr, nullptr, 0);
    alloc->Release();
    if (FAILED(hr) || !s) { input->Release(); if (clk) clk->Release(); break; }  // decommitted / stopping
    BYTE* buf = nullptr;
    if (s->GetPointer(&buf) != S_OK) buf = nullptr;
    uint32_t outFid = 0; uint64_t outPub = 0, flags = 0;
    bool ok = false;
    if (fresh && buf) {
      ok = VCamCopySlot(view, slot, seq, buf, outFid, outPub);   // shm -> sample, single copy
      if (ok) { lastTok = (((uint64_t)slot) << 32) | (uint32_t)seq; }
    }
    if (!ok && buf) {
      if (m_hasLastGood && m_lastGoodFrame) {
        memcpy(buf, m_lastGoodFrame, VCAM_FRAME_BYTES);           // freeze: repeat, not fake bytes
        outFid = lastFid; outPub = lastPub; flags = VC_VSTAT_FLAG_REPEAT;
      } else if (m_motionEnable) {
        vc_gen_motion(fid++, Y, UV);
        memcpy(buf, Y, VC_Y_BYTES); memcpy(buf + VC_Y_BYTES, UV, VC_UV_BYTES);
        flags = VC_VSTAT_FLAG_REPEAT;  // not a receiver frame: excluded from vseg
      }
    }
    s->SetActualDataLength(VCAM_FRAME_BYTES);
    // timestamps on the graph clock (stream time = now - tStart); counter fallback when paused / no clock
    REFERENCE_TIME ts = tcount;
    if (clk && st == State_Running) { REFERENCE_TIME now = 0; if (SUCCEEDED(clk->GetTime(&now))) ts = now - t0; }
    if (clk) clk->Release();
    if (ts <= lastStart) ts = lastStart + 1;
    REFERENCE_TIME te = ts + VCAM_FRAME_100NS;
    s->SetTime(&ts, &te);
    s->SetSyncPoint(TRUE);
    if (discont) { s->SetDiscontinuity(TRUE); discont = FALSE; }
    lastStart = ts; tcount = te;
    uint64_t del = VCamQpcNs(freq);
    hr = input->Receive(s);
    s->Release(); input->Release();
    VstatPush(outFid, outPub, del, (uint32_t)flags);
    // off the latency path: refresh the freeze cache from the slot (seqlock-checked)
    if (ok && m_lastGoodFrame) {
      uint32_t f2 = 0; uint64_t p2 = 0;
      if (VCamCopySlot(view, slot, seq, m_lastGoodFrame, f2, p2)) { m_hasLastGood = true; lastFid = f2; lastPub = p2; }
    }
    uint64_t n = VCamQpcNs(freq);
    if (ok) next_ns = n + FIRST_REPEAT_NS;
    else { next_ns += FRAME_NS; if (n > next_ns + 20000000ULL) next_ns = n; }  // no catch-up burst
    if (FAILED(hr)) break;
  }
  if (mm && av) { avrev_fn r = (avrev_fn)GetProcAddress(av, "AvRevertMmThreadCharacteristics"); if (r) r(mm); }
  if (av) FreeLibrary(av);
  timeEndPeriod(1);
  return 0;
}

// ---------- pin impl ----------
STDMETHODIMP CVCamPin::Connect(IPin* pPeer, const AM_MEDIA_TYPE* pmt) {
  if (!pPeer) return E_POINTER;
  m_pFilter->Lock();
  if (m_connected || m_pPeer) { m_pFilter->Unlock(); return VFW_E_ALREADY_CONNECTED; }
  PIN_DIRECTION dir = PINDIR_OUTPUT;
  if (FAILED(pPeer->QueryDirection(&dir)) || dir != PINDIR_INPUT) { m_pFilter->Unlock(); return VFW_E_INVALID_DIRECTION; }
  AM_MEDIA_TYPE* use = nullptr;
  AM_MEDIA_TYPE* owned = nullptr;
  if (pmt) {
    if (!VCamTypeMatches(pmt)) { m_pFilter->Unlock(); return VFW_E_TYPE_NOT_ACCEPTED; }
    use = (AM_MEDIA_TYPE*)pmt;
  } else {
    owned = VCamCreateType();
    if (!owned) { m_pFilter->Unlock(); return E_OUTOFMEMORY; }
    use = owned;
  }
  HRESULT hr = pPeer->ReceiveConnection((IPin*)this, use);
  if (owned) VCamFreeType(owned);
  if (FAILED(hr)) { m_pFilter->Unlock(); return hr; }
  IMemInputPin* input = nullptr;
  hr = pPeer->QueryInterface(IID_IMemInputPin, (void**)&input);
  if (FAILED(hr) || !input) { pPeer->Disconnect(); m_pFilter->Unlock(); return VFW_E_NO_TRANSPORT; }
  IMemAllocator* alloc = nullptr;
  hr = input->GetAllocator(&alloc);
  if (FAILED(hr) || !alloc) {
    hr = CoCreateInstance(CLSID_MemoryAllocator, nullptr, CLSCTX_INPROC_SERVER,
                          IID_IMemAllocator, (void**)&alloc);
  }
  if (SUCCEEDED(hr) && alloc) {
    ALLOCATOR_PROPERTIES req = { 2, (long)VCAM_FRAME_BYTES, 1, 0 }  /* 2: bound downstream queueing to 1 frame */, act = { 0 };
    hr = alloc->SetProperties(&req, &act);
    if (SUCCEEDED(hr) && act.cbBuffer < VCAM_FRAME_BYTES) hr = VFW_E_SIZENOTSET;
    if (SUCCEEDED(hr)) hr = input->NotifyAllocator(alloc, FALSE);
  }
  if (FAILED(hr) || !alloc) {
    if (alloc) alloc->Release();
    input->Release(); pPeer->Disconnect(); m_pFilter->Unlock(); return FAILED(hr) ? hr : E_FAIL;
  }
  m_pPeer = pPeer; m_pPeer->AddRef();
  m_pInput = input;
  m_pAlloc = alloc;
  VCamCopyType(&m_mt, use);
  m_connected = TRUE;
  m_pFilter->Unlock(); return S_OK;
}
STDMETHODIMP CVCamPin::Disconnect() {
  // Snapshot under lock, join/decommit outside the lock: the worker briefly takes
  // the filter lock each tick, so joining while holding it could stall the join.
  m_pFilter->Lock();
  if (!m_connected && !m_pPeer) { m_pFilter->Unlock(); return S_FALSE; }
  IPin* peer = m_pPeer; m_pPeer = nullptr;
  IMemInputPin* input = m_pInput; m_pInput = nullptr;
  IMemAllocator* alloc = m_pAlloc; m_pAlloc = nullptr;
  VCamClearType(&m_mt);
  m_connected = FALSE;
  m_pFilter->Unlock();
  m_pFilter->Deactivate();
  if (alloc) alloc->Release();
  if (input) input->Release();
  if (peer) peer->Release();
  return S_OK;
}

// ---------- class factory ----------
class CVCamFactory : public IClassFactory {
  LONG m_cRef = 1;
public:
  STDMETHODIMP QueryInterface(REFIID riid, void** ppv) {
    if (!ppv) return E_POINTER;
    if (riid == IID_IUnknown || riid == IID_IClassFactory) { *ppv = (IClassFactory*)this; AddRef(); return S_OK; }
    *ppv = nullptr; return E_NOINTERFACE;
  }
  STDMETHODIMP_(ULONG) AddRef() { return (ULONG)InterlockedIncrement(&m_cRef); }
  STDMETHODIMP_(ULONG) Release() {
    LONG n = InterlockedDecrement(&m_cRef);
    if (!n) delete this; return (ULONG)n;
  }
  STDMETHODIMP CreateInstance(IUnknown* pUnk, REFIID riid, void** ppv) {
    if (pUnk) return CLASS_E_NOAGGREGATION;
    CVCamFilter* f = new (std::nothrow) CVCamFilter();
    if (!f) return E_OUTOFMEMORY;
    if (!f->HasPin()) { f->Release(); return E_OUTOFMEMORY; }  // Release keeps g_objs balanced
    HRESULT hr = f->QueryInterface(riid, ppv);
    f->Release(); return hr;
  }
  STDMETHODIMP LockServer(BOOL b) {
    if (b) InterlockedIncrement(&g_locks); else InterlockedDecrement(&g_locks);
    return S_OK;
  }
};

// ---------- module exports ----------
STDAPI DllGetClassObject(REFCLSID rclsid, REFIID riid, void** ppv) {
  if (rclsid != CLSID_VirtualCaptureCam) return CLASS_E_CLASSNOTAVAILABLE;
  CVCamFactory* f = new (std::nothrow) CVCamFactory();
  if (!f) return E_OUTOFMEMORY;
  HRESULT hr = f->QueryInterface(riid, ppv);
  f->Release(); return hr;
}
STDAPI DllCanUnloadNow() {
  return (g_objs == 0 && g_locks == 0) ? S_OK : S_FALSE;
}
static const wchar_t* CLSID_STR = L"{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}";
static const wchar_t* CAT_VIDEO_INPUT = L"{860BB310-5D01-11d0-BD3B-00A0C911CE86}";
static LONG RegSetStr(HKEY root, const wchar_t* sub, const wchar_t* val, const wchar_t* data) {
  HKEY k = nullptr; LONG rc = RegCreateKeyEx(root, sub, 0, nullptr, 0, KEY_WRITE, nullptr, &k, nullptr);
  if (rc != ERROR_SUCCESS) return rc;
  rc = RegSetValueEx(k, val, 0, REG_SZ, (const BYTE*)data,
                     (DWORD)((wcslen(data) + 1) * sizeof(wchar_t)));
  RegCloseKey(k); return rc;
}
STDAPI DllRegisterServer() {
  wchar_t path[MAX_PATH];
  if (!GetModuleFileName(g_hMod, path, MAX_PATH)) return SELFREG_E_CLASS;
  wchar_t clsidKey[128], instKey[256], srv[160];
  swprintf_s(clsidKey, L"Software\\Classes\\CLSID\\%s", CLSID_STR);
  swprintf_s(instKey,  L"Software\\Classes\\CLSID\\%s\\Instance\\%s", CAT_VIDEO_INPUT, CLSID_STR);
  swprintf_s(srv,      L"%s\\InprocServer32", clsidKey);

  // REGFILTER2 v2 blob: 1 output pin, 1 media type (vids/NV12), category = PIN_CATEGORY_CAPTURE.
  static const BYTE kFilterData[104] = {
    0x02,0x00,0x00,0x00,0x00,0x00,0x20,0x00,0x01,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    0x30,0x70,0x69,0x33,0x08,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x01,0x00,0x00,0x00,
    0x00,0x00,0x00,0x00,0x58,0x00,0x00,0x00,0x30,0x74,0x79,0x33,0x00,0x00,0x00,0x00,
    0x38,0x00,0x00,0x00,0x48,0x00,0x00,0x00,0x76,0x69,0x64,0x73,0x00,0x00,0x10,0x00,
    0x80,0x00,0x00,0xaa,0x00,0x38,0x9b,0x71,0x4e,0x56,0x31,0x32,0x00,0x00,0x10,0x00,
    0x80,0x00,0x00,0xaa,0x00,0x38,0x9b,0x71,
    0x81,0x42,0x6c,0xfb,0x53,0x03,0xd1,0x11,0x90,0x5f,0x00,0x00,0xc0,0xcc,0x16,0xba,
  };
  static_assert(sizeof(kFilterData)==104,"FilterData");
  DWORD merit = 0x00200000;

  // Helper: write to one hive (HKCU always; HKLM if we have write access)
  auto WriteHive = [&](HKEY root) -> bool {
    if (RegSetStr(root, clsidKey, nullptr, VCAM_NAME) != ERROR_SUCCESS) return false;
    if (RegSetStr(root, srv, nullptr, path)           != ERROR_SUCCESS) return false;
    if (RegSetStr(root, srv, L"ThreadingModel", L"Both") != ERROR_SUCCESS) return false;
    if (RegSetStr(root, instKey, L"FriendlyName", VCAM_NAME) != ERROR_SUCCESS) return false;
    if (RegSetStr(root, instKey, L"CLSID", CLSID_STR)  != ERROR_SUCCESS) return false;
    HKEY k = nullptr;
    if (RegCreateKeyEx(root, instKey, 0, nullptr, 0, KEY_WRITE, nullptr, &k, nullptr) != ERROR_SUCCESS) return false;
    RegSetValueEx(k, L"Merit",      0, REG_DWORD,  (const BYTE*)&merit,      sizeof(merit));
    RegSetValueEx(k, L"FilterData", 0, REG_BINARY, kFilterData, sizeof(kFilterData));
    RegCloseKey(k);
    return true;
  };

  if (!WriteHive(HKEY_CURRENT_USER)) return SELFREG_E_CLASS;
  WriteHive(HKEY_LOCAL_MACHINE);  // best-effort (admin); ignore failure
  return S_OK;
}
STDAPI DllUnregisterServer() {
  wchar_t clsidKey[128], instKey[256];
  swprintf_s(clsidKey, L"Software\\Classes\\CLSID\\%s", CLSID_STR);
  swprintf_s(instKey,  L"Software\\Classes\\CLSID\\%s\\Instance\\%s", CAT_VIDEO_INPUT, CLSID_STR);
  RegDeleteTree(HKEY_CURRENT_USER,   instKey);
  RegDeleteTree(HKEY_CURRENT_USER,   clsidKey);
  RegDeleteTree(HKEY_LOCAL_MACHINE,  instKey);   // best-effort
  RegDeleteTree(HKEY_LOCAL_MACHINE,  clsidKey);  // best-effort
  return S_OK;
}
BOOL APIENTRY DllMain(HMODULE h, DWORD r, LPVOID) {
  if (r == DLL_PROCESS_ATTACH) { g_hMod = h; DisableThreadLibraryCalls(h); }
  return TRUE;
}
