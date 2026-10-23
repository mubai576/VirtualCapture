// vc-vcam.cpp — P4-3 DShow source filter (user-mode COM, no driver signing).
// Push source: 1 output pin, NV12 640x640, AvgTimePerFrame=41666 (240fps).
// Frame source (bring-up): local integer pattern generator (sender/pattern.h motion,
// deterministic in fid). Receiver shared-memory feed replaces FillFrame() at PC2 wiring (P4-4).
// Registration: HKCU-only (no admin): CLSID + VideoInputDeviceCategory instance.
// Hand-rolled COM: no ATL, no strmbase (not in modern Windows SDK). Links strmiids + winmm.
#include <windows.h>
#include <dshow.h>
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

static const wchar_t* VCAM_NAME = L"VirtualCapture 640x640 NV12";
static const wchar_t* VCAM_PIN_ID = L"Out";
static const wchar_t* VCAM_CLSID_KEY =
  L"Software\\Classes\\CLSID\\{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}";
static const wchar_t* VCAM_SHM_DEFAULT = L"Local\\VirtualCaptureVcamFrame";
static const LONG VCAM_W = 640, VCAM_H = 640;
static const GUID VCAM_AMPROPSETID_Pin =
  {0x9b00f101, 0x1567, 0x11d1, {0xb3, 0xa9, 0x00, 0xa0, 0xc9, 0x22, 0x31, 0x96}};
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
  uint8_t* m_shmStage = nullptr;  // VC_FRAME_BYTES staging (torn-read safe copy)
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
class CVCamPin : public IPin, public IAMStreamConfig, public IKsPropertySet {
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
    else if (riid == IID_IAMStreamConfig) *ppv = (IAMStreamConfig*)this;
    else if (riid == IID_IKsPropertySet) *ppv = (IKsPropertySet*)this;
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
  // IAMStreamConfig
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
  STDMETHODIMP GetNumberOfCapabilities(int* piCount, int* piSize) {
    if (!piCount || !piSize) return E_POINTER;
    *piCount = 1; *piSize = sizeof(VIDEO_STREAM_CONFIG_CAPS); return S_OK;
  }
  // IKsPropertySet: advertise PIN_CATEGORY_CAPTURE so stock clients (ffmpeg
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
  STDMETHODIMP GetStreamCaps(int i, AM_MEDIA_TYPE** pp, BYTE* caps) {
    if (i != 0 || !pp || !caps) return E_INVALIDARG;
    AM_MEDIA_TYPE* t = VCamCreateType();
    if (!t) return E_OUTOFMEMORY;
    VIDEO_STREAM_CONFIG_CAPS* c = (VIDEO_STREAM_CONFIG_CAPS*)caps;
    ZeroMemory(c, sizeof(*c));
    c->guid = FORMAT_VideoInfo;
    c->VideoStandard = AnalogVideo_None;
    c->InputSize.cx = VCAM_W; c->InputSize.cy = VCAM_H;
    c->MinCroppingSize.cx = VCAM_W; c->MinCroppingSize.cy = VCAM_H;
    c->MaxCroppingSize.cx = VCAM_W; c->MaxCroppingSize.cy = VCAM_H;
    c->CropGranularityX = 1; c->CropGranularityY = 1;
    c->CropAlignX = 1; c->CropAlignY = 1;
    c->MinOutputSize.cx = VCAM_W; c->MinOutputSize.cy = VCAM_H;
    c->MaxOutputSize.cx = VCAM_W; c->MaxOutputSize.cy = VCAM_H;
    c->OutputGranularityX = 1; c->OutputGranularityY = 1;
    c->StretchTapsX = 0; c->StretchTapsY = 0;
    c->ShrinkTapsX = 0; c->ShrinkTapsY = 0;
    c->MinFrameInterval = VCAM_FRAME_100NS; c->MaxFrameInterval = VCAM_FRAME_100NS;
    c->MinBitsPerSecond = VCAM_FRAME_BYTES * 8 * 240;
    c->MaxBitsPerSecond = VCAM_FRAME_BYTES * 8 * 240;
    *pp = t; return S_OK;
  }
};

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
  m_shmStage = new (std::nothrow) uint8_t[VCAM_FRAME_BYTES];
  m_lastGoodFrame = new (std::nothrow) uint8_t[VCAM_FRAME_BYTES];  // D12
}
void CVCamFilter::ShmEnsureOpen() {
  // called with m_cs held; throttled by design (early-out once mapped)
  if (m_shmView) return;
  if (!m_hShmMap) {
    std::wstring name = VCAM_SHM_DEFAULT;
    HKEY k = nullptr;
    if (RegOpenKeyEx(HKEY_CURRENT_USER, VCAM_CLSID_KEY, 0, KEY_READ, &k) == ERROR_SUCCESS) {
      wchar_t v[260]; DWORD cb = sizeof(v), tp = 0;
      if (RegQueryValueEx(k, L"FrameMap", nullptr, &tp, (BYTE*)v, &cb) == ERROR_SUCCESS && tp == REG_SZ)
        name = v;
      RegCloseKey(k);
    }
    m_hShmMap = OpenFileMappingW(FILE_MAP_READ, FALSE, name.c_str());
    if (!m_hShmMap) return;  // receiver not publishing (yet): motion fallback
  }
  m_shmView = (uint8_t*)MapViewOfFile(m_hShmMap, FILE_MAP_READ, 0, 0, VC_SHM_SIZE);
  if (!m_shmView) { CloseHandle(m_hShmMap); m_hShmMap = nullptr; }
}
void CVCamFilter::ShmClose() {
  // called from Deactivate (worker joined) or dtor: no lock needed
  if (m_shmView) { UnmapViewOfFile(m_shmView); m_shmView = nullptr; }
  if (m_hShmMap) { CloseHandle(m_hShmMap); m_hShmMap = nullptr; }
}
CVCamFilter::~CVCamFilter() {
  Deactivate();
  ShmClose();
  if (m_shmStage) { delete[] m_shmStage; m_shmStage = nullptr; }
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
STDMETHODIMP CVCamFilter::Run(REFERENCE_TIME) {
  HRESULT hr = Activate();
  if (FAILED(hr)) return hr;
  Lock(); m_state = State_Running; Unlock(); return S_OK;
}
// 240Hz frame cadence (QPC): coarse Sleep + spin tail, stop-responsive, no catch-up burst.
static uint64_t VCamQpcFreq() {
  LARGE_INTEGER f; QueryPerformanceFrequency(&f); return (uint64_t)f.QuadPart;
}
static uint64_t VCamQpcNs(uint64_t freq) {
  LARGE_INTEGER c; QueryPerformanceCounter(&c);
  return (uint64_t)c.QuadPart * 1000000000ull / freq;
}
DWORD CVCamFilter::Worker() {
  static uint8_t Y[VC_Y_BYTES], UV[VC_UV_BYTES];  // exact NV12 planes, never sizeof-guessed
  uint32_t fid = 0;
  LONGLONG t = 0;
  BOOL discont = TRUE;
  const uint64_t freq = VCamQpcFreq();
  timeBeginPeriod(1);
  uint64_t next_ns = VCamQpcNs(freq);
  for (;;) {
    next_ns += 4166666ULL;  // 240Hz
    bool stopReq = false;
    for (;;) {
      if (WaitForSingleObject(m_hStop, 0) == WAIT_OBJECT_0) { stopReq = true; break; }
      uint64_t now = VCamQpcNs(freq);
      if (now >= next_ns) break;
      uint64_t rem_us = (next_ns - now) / 1000ULL;
      if (rem_us > 2500) { if (WaitForSingleObject(m_hStop, 2) == WAIT_OBJECT_0) { stopReq = true; break; } }
      else if (rem_us > 100) Sleep(1);
      else SwitchToThread();  // spin tail
    }
    if (stopReq) break;
    if (VCamQpcNs(freq) > next_ns + 20000000ULL) next_ns = VCamQpcNs(freq);  // >20ms behind: resync
    IMemInputPin* input = nullptr;
    IMemAllocator* alloc = nullptr;
    Lock();
    if (m_pPin && m_pPin->m_pInput) { input = m_pPin->m_pInput; input->AddRef(); }
    if (m_pPin && m_pPin->m_pAlloc) { alloc = m_pPin->m_pAlloc; alloc->AddRef(); }
    Unlock();
    if (!input || !alloc) { if (input) input->Release(); if (alloc) alloc->Release(); break; }
    IMediaSample* s = nullptr;
    HRESULT hr = alloc->GetBuffer(&s, nullptr, nullptr, 0);
    alloc->Release();
    if (FAILED(hr) || !s) { input->Release(); break; }  // decommitted / stopping
    // P4-3/D11/D12: shm latest frame first.
    // D12 three-state policy:
    //   clean read           → output frame, update last-good cache
    //   stall/torn + has last-good → repeat last good frame (freeze, no fake bytes)
    //   no mapping + no last-good + motionEnable=false → stall (no output this tick)
    //   motionEnable=true (explicit bring-up) → generate motion pattern
    bool filled = false;
    bool haveView = false;
    uint8_t* view = nullptr;
    Lock();
    ShmEnsureOpen();
    view = m_shmView;
    Unlock();
    haveView = (view != nullptr);
    if (view && m_shmStage) {
      // NOTE: view is mapped READ-ONLY: never issue LOCK-prefixed RMW here
      // (InterlockedOr writes back -> AV on read-only page). Aligned 32-bit
      // plain reads are atomic on x86/x64; writer side keeps Interlocked ops.
      vc_shm_hdr_t* sh = (vc_shm_hdr_t*)view;
      volatile LONG* pseq = (volatile LONG*)&sh->seq;
      LONG s1 = *pseq;
      if (!(s1 & 1) && sh->magic == VC_SHM_MAGIC && sh->size == (uint32_t)VC_FRAME_BYTES) {
        memcpy(m_shmStage, view + sizeof(vc_shm_hdr_t), VC_FRAME_BYTES);
        LONG s2 = *pseq;
        filled = (s1 == s2);
      }
    }
    // D12: update last-good cache on a clean read
    if (filled && m_lastGoodFrame) {
      memcpy(m_lastGoodFrame, m_shmStage, VCAM_FRAME_BYTES);
      m_hasLastGood = true;
    }
    if (!filled) {
      if (m_hasLastGood && m_lastGoodFrame) {
        // Torn read or mapping lost: repeat last good frame (freeze, not fake bytes)
        memcpy(m_shmStage, m_lastGoodFrame, VCAM_FRAME_BYTES);
        filled = true;
        // no discont: repeated frame is a valid continuation, not a gap
      } else if (m_motionEnable) {
        vc_gen_motion(fid++, Y, UV);  // explicit bring-up mode: no receiver needed
      } else {
        // D12: no mapping, no last-good frame, motion disabled → stall (no output)
        t += VCAM_FRAME_100NS;
        discont = TRUE;
        s->Release(); input->Release();
        continue;
      }
    }
    BYTE* buf = nullptr;
    if (s->GetPointer(&buf) == S_OK && buf) {
      if (filled) memcpy(buf, m_shmStage, VC_FRAME_BYTES);
      else { memcpy(buf, Y, VC_Y_BYTES); memcpy(buf + VC_Y_BYTES, UV, VC_UV_BYTES); }
    }
    s->SetActualDataLength(VCAM_FRAME_BYTES);
    LONGLONG t1 = t + VCAM_FRAME_100NS;
    s->SetTime(&t, &t1);
    s->SetSyncPoint(TRUE);
    if (discont) { s->SetDiscontinuity(TRUE); discont = FALSE; }
    t = t1;
    hr = input->Receive(s);
    s->Release(); input->Release();
    if (FAILED(hr)) break;
  }
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
    ALLOCATOR_PROPERTIES req = { 3, (long)VCAM_FRAME_BYTES, 1, 0 }, act = { 0 };
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
  wchar_t clsidKey[128], instKey[256];
  swprintf_s(clsidKey, L"Software\\Classes\\CLSID\\%s", CLSID_STR);
  swprintf_s(instKey, L"Software\\Classes\\CLSID\\%s\\Instance\\%s", CAT_VIDEO_INPUT, CLSID_STR);
  if (RegSetStr(HKEY_CURRENT_USER, clsidKey, nullptr, VCAM_NAME) != ERROR_SUCCESS) return SELFREG_E_CLASS;
  wchar_t srv[160];
  swprintf_s(srv, L"%s\\InprocServer32", clsidKey);
  if (RegSetStr(HKEY_CURRENT_USER, srv, nullptr, path) != ERROR_SUCCESS) return SELFREG_E_CLASS;
  if (RegSetStr(HKEY_CURRENT_USER, srv, L"ThreadingModel", L"Both") != ERROR_SUCCESS) return SELFREG_E_CLASS;
  if (RegSetStr(HKEY_CURRENT_USER, instKey, L"FriendlyName", VCAM_NAME) != ERROR_SUCCESS) return SELFREG_E_CLASS;
  if (RegSetStr(HKEY_CURRENT_USER, instKey, L"CLSID", CLSID_STR) != ERROR_SUCCESS) return SELFREG_E_CLASS;
  HKEY k = nullptr;
  if (RegCreateKeyEx(HKEY_CURRENT_USER, instKey, 0, nullptr, 0, KEY_WRITE, nullptr, &k, nullptr) != ERROR_SUCCESS)
    return SELFREG_E_CLASS;
  DWORD merit = 0x00200000;
  RegSetValueEx(k, L"Merit", 0, REG_DWORD, (const BYTE*)&merit, sizeof(merit));
  // FilterData binary blob: version=2, merit=0x200000, 1 output pin, MEDIATYPE_Video + MEDIASUBTYPE_NV12.
  // Serialized format matches DShow FilterMapper internal representation (offset-based tags).
  // Verified against OBS Virtual Camera FilterData (same 88-byte layout).
  // FilterData: 88 bytes, exactly matching OBS Virtual Camera format.
  // version=2, merit=0x00200000, 1 output pin (MEDIATYPE_Video + MEDIASUBTYPE_NV12).
  static const BYTE kFilterData[88] = {
    0x02,0x00,0x00,0x00,0x00,0x00,0x20,0x00,0x01,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    0x30,0x70,0x69,0x33,0x08,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x01,0x00,0x00,0x00,
    0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x30,0x74,0x79,0x33,0x00,0x00,0x00,0x00,
    0x38,0x00,0x00,0x00,0x48,0x00,0x00,0x00,0x76,0x69,0x64,0x73,0x00,0x00,0x10,0x00,
    0x80,0x00,0x00,0xaa,0x00,0x38,0x9b,0x71,0x4e,0x56,0x31,0x32,0x00,0x00,0x10,0x00,
    0x80,0x00,0x00,0xaa,0x00,0x38,0x9b,0x71,
  };
  static_assert(sizeof(kFilterData) == 88, "FilterData must be 88 bytes");

  RegSetValueEx(k, L"FilterData", 0, REG_BINARY, kFilterData, sizeof(kFilterData));
  RegCloseKey(k);
  return S_OK;
}
STDAPI DllUnregisterServer() {
  wchar_t clsidKey[128], instKey[256];
  swprintf_s(clsidKey, L"Software\\Classes\\CLSID\\%s", CLSID_STR);
  swprintf_s(instKey, L"Software\\Classes\\CLSID\\%s\\Instance\\%s", CAT_VIDEO_INPUT, CLSID_STR);
  RegDeleteTree(HKEY_CURRENT_USER, instKey);
  RegDeleteTree(HKEY_CURRENT_USER, clsidKey);
  return S_OK;
}
BOOL APIENTRY DllMain(HMODULE h, DWORD r, LPVOID) {
  if (r == DLL_PROCESS_ATTACH) { g_hMod = h; DisableThreadLibraryCalls(h); }
  return TRUE;
}
