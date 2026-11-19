//! Hand-written DirectShow source-filter COM server.
//!
//! Layout and behaviour follow `vcam/vcam.cpp`:
//! * one `IBaseFilter` + `IAMFilterMiscFlags` object owning one `IPin` object;
//! * the pin exposes `IPin`, `IAMStreamConfig`, `IKsPropertySet`,
//!   `ISpecifyPropertyPages`, `IKsControl` and `IKsPin` as separate interface
//!   tear-offs that all share the filter reference count (no cycles);
//! * a worker thread pulls the latest complete SHM v3 frame and pushes it to
//!   the downstream allocator; before the first verified frame it emits
//!   nothing, after one it repeats only that last good frame.
//!
//! Every interface returns its own vtable pointer, so a tear-off is exactly
//! `{ vtbl, owner }`; the owner of every tear-off (and of the object base) is
//! stored at pointer offset 1, which makes QueryInterface/AddRef/Release
//! independent of which sub-object the caller used.

#![allow(non_snake_case)]

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::DirectShow::{
    ALLOCATOR_PROPERTIES, IAMFilterMiscFlags, IAMStreamConfig, IBaseFilter, IEnumMediaTypes,
    IEnumPins, IMediaFilter, IMemAllocator, IMemInputPin, IPin, PINDIR_INPUT,
};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsPin, IKsPropertySet};
use windows::Win32::Media::MediaFoundation::{AM_MEDIA_TYPE, CLSID_MemoryAllocator};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemAlloc, IClassFactory,
};
use windows::Win32::System::Ole::ISpecifyPropertyPages;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_BINARY, REG_DWORD,
    REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::{GUID, HRESULT, Interface, PCWSTR};

use crate::abi::{
    AM_FILTER_MISC_FLAGS_IS_SOURCE, AMPROPERTY_PIN_CATEGORY, AMPROPSETID_Pin,
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, CLSID_VCAM, CaUuidRaw, E_FAIL, E_INVALIDARG,
    E_NOINTERFACE, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, E_PROP_ID_UNSUPPORTED, E_UNEXPECTED,
    FilterInfoRaw, IAMFilterMiscFlagsVtbl, IAMStreamConfigVtbl, IBaseFilterVtbl, IClassFactoryVtbl,
    IEnumMediaTypesVtbl, IEnumPinsVtbl, IID_IPERSIST, IID_IUNKNOWN, IKsControlVtbl, IKsPinVtbl,
    IKsPropertySetVtbl, IPinVtbl, ISpecifyPropertyPagesVtbl, IUnknownVtbl, KSPROPERTY_SUPPORT_GET,
    MediaType, PIN_CATEGORY_CAPTURE, PINDIR_OUTPUT, PinInfoRaw, S_FALSE, S_OK, SELFREG_E_CLASS,
    STATE_PAUSED, STATE_RUNNING, STATE_STOPPED, STATUS_NOT_SUPPORTED, VFW_E_ALREADY_CONNECTED,
    VFW_E_INVALID_DIRECTION, VFW_E_NO_TRANSPORT, VFW_E_NOT_CONNECTED, VFW_E_NOT_FOUND,
    VFW_E_SIZENOTSET, VFW_E_TYPE_NOT_ACCEPTED, VFW_S_CANT_CUE, VideoStreamConfigCaps, com_addref,
    copy_name, failed, wide, wide_eq,
};
use crate::geometry::StreamGeometry;
use crate::media;
use crate::shm::{self, VSTAT_FLAG_REPEAT};
use crate::win::{self, ShmSet};

// ---------- process-wide COM bookkeeping ----------

static G_OBJS: AtomicI32 = AtomicI32::new(0);
static G_LOCKS: AtomicI32 = AtomicI32::new(0);

// ---------- object layouts ----------

#[repr(C)]
struct FilterTear<V> {
    vtbl: *const V,
    owner: *mut FilterObj,
}

#[repr(C)]
struct PinTear<V> {
    vtbl: *const V,
    owner: *mut PinObj,
}

#[repr(C)]
struct FilterObj {
    vtbl: *const IBaseFilterVtbl,
    owner: *mut FilterObj, // self, also readable through the tear-off slot
    refs: AtomicU32,
    misc: FilterTear<IAMFilterMiscFlagsVtbl>,
    core: Arc<FilterCore>,
    pin: *mut PinObj,
}

#[repr(C)]
struct PinObj {
    vtbl: *const IPinVtbl,
    owner: *mut PinObj, // self
    filter: *mut FilterObj,
    stream_config: PinTear<IAMStreamConfigVtbl>,
    ks_prop: PinTear<IKsPropertySetVtbl>,
    spec_pp: PinTear<ISpecifyPropertyPagesVtbl>,
    ks_ctrl: PinTear<IKsControlVtbl>,
    ks_pin: PinTear<IKsPinVtbl>,
}

/// Every tear-off and every object base stores its owner at offset 1.
unsafe fn owner_slot<T>(this: *mut c_void) -> *mut T {
    if this.is_null() {
        return ptr::null_mut();
    }
    *((this as *mut *mut c_void).add(1)) as *mut T
}

unsafe fn filter_owner(this: *mut c_void) -> *mut FilterObj {
    owner_slot::<FilterObj>(this)
}

unsafe fn pin_owner(this: *mut c_void) -> *mut PinObj {
    owner_slot::<PinObj>(this)
}

// ---------- shared filter state ----------

struct Shared {
    state: i32,
    graph: *mut c_void, // borrowed IFilterGraph; the graph owns its filters
    clock: Option<IReferenceClock>,
    peer: Option<IPin>,
    input: Option<IMemInputPin>,
    alloc: Option<IMemAllocator>,
    connected: bool,
    mt: MediaType,
    shm: Option<ShmSet>,
    stop: Option<HANDLE>,
    worker: Option<JoinHandle<()>>,
    t_start: i64,
}

impl Shared {
    fn new() -> Self {
        Self {
            state: STATE_STOPPED,
            graph: ptr::null_mut(),
            clock: None,
            peer: None,
            input: None,
            alloc: None,
            connected: false,
            mt: MediaType::default(),
            shm: None,
            stop: None,
            worker: None,
            t_start: 0,
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe { media::clear_media_type(&mut self.mt) };
    }
}

pub struct FilterCore {
    geometry: StreamGeometry,
    control: Mutex<()>,
    shared: Mutex<Shared>,
}

// COM objects are apartment-neutral here: all mutable state is behind locks,
// interfaces stored in `Shared` are reference counted, and the worker thread
// only touches state handed to it under those locks.
unsafe impl Send for FilterCore {}
unsafe impl Sync for FilterCore {}

impl FilterCore {
    fn new() -> Self {
        let geometry = win::discover_geometry().unwrap_or_else(win::default_geometry);
        Self {
            geometry,
            control: Mutex::new(()),
            shared: Mutex::new(Shared::new()),
        }
    }

    fn lock_control(&self) -> MutexGuard<'_, ()> {
        match self.control.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn lock_shared(&self) -> MutexGuard<'_, Shared> {
        match self.shared.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn activate_locked(self: &Arc<Self>) -> HRESULT {
        let alloc = {
            let shared = self.lock_shared();
            if shared.stop.is_some() || shared.worker.is_some() {
                return S_OK;
            }
            shared.alloc.clone()
        };
        if let Some(alloc) = &alloc
            && let Err(error) = unsafe { alloc.Commit() }
        {
            return error.code();
        }
        let stop = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
            Ok(handle) => handle,
            Err(_) => {
                if let Some(alloc) = &alloc {
                    let _ = unsafe { alloc.Decommit() };
                }
                return E_FAIL;
            }
        };
        let stop_raw = stop.0 as usize;
        let core = self.clone();
        match std::thread::Builder::new()
            .name("vc-vcam-worker".into())
            .spawn(move || core.worker(stop_raw))
        {
            Ok(worker) => {
                let mut shared = self.lock_shared();
                shared.stop = Some(stop);
                shared.worker = Some(worker);
                S_OK
            }
            Err(_) => {
                unsafe {
                    let _ = CloseHandle(stop);
                }
                if let Some(alloc) = &alloc {
                    let _ = unsafe { alloc.Decommit() };
                }
                E_FAIL
            }
        }
    }

    fn deactivate_locked(&self) {
        let (stop, worker, alloc, peer) = {
            let mut shared = self.lock_shared();
            (
                shared.stop.take(),
                shared.worker.take(),
                shared.alloc.clone(),
                shared.peer.clone(),
            )
        };
        if let Some(stop) = stop {
            unsafe {
                let _ = SetEvent(stop);
            }
        }
        if let Some(peer) = &peer {
            let _ = unsafe { peer.BeginFlush() };
        }
        if let Some(alloc) = &alloc {
            let _ = unsafe { alloc.Decommit() };
        }
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        if let Some(peer) = &peer {
            let _ = unsafe { peer.EndFlush() };
        }
        if let Some(stop) = stop {
            unsafe {
                let _ = CloseHandle(stop);
            }
        }
        // The worker is gone, so the read-only view can be dropped safely.
        self.lock_shared().shm = None;
    }

    fn deactivate(self: &Arc<Self>) {
        let _control = self.lock_control();
        self.deactivate_locked();
    }

    fn stop(&self) -> HRESULT {
        let _control = self.lock_control();
        self.deactivate_locked();
        self.lock_shared().state = STATE_STOPPED;
        S_OK
    }

    fn pause(self: &Arc<Self>) -> HRESULT {
        let _control = self.lock_control();
        let hr = self.activate_locked();
        if failed(hr) {
            return hr;
        }
        self.lock_shared().state = STATE_PAUSED;
        S_OK
    }

    fn run(self: &Arc<Self>, t_start: i64) -> HRESULT {
        let _control = self.lock_control();
        let hr = self.activate_locked();
        if failed(hr) {
            return hr;
        }
        {
            let mut shared = self.lock_shared();
            shared.t_start = t_start;
            shared.state = STATE_RUNNING;
        }
        S_OK
    }

    // ---------- worker ----------

    fn worker(self: Arc<Self>, stop_raw: usize) {
        // Legacy worker policy (`vcam.cpp:715-721`): 1 ms timer period plus
        // MMCSS "Capture" / HIGHEST fallback on this thread.  Reverted when
        // this function returns, on every exit path.
        let _scheduling = win::WorkerScheduling::raise();
        let stop = HANDLE(stop_raw as *mut c_void);
        let geometry = self.geometry;
        let frame_bytes = geometry.frame_bytes() as usize;
        let frame_100ns = geometry.frame_100ns();
        let frame_ns = geometry.frame_ns();
        let first_repeat_ns = frame_ns * 3 / 2;

        let mut last_good = vec![0u8; frame_bytes];
        let mut has_last_good = false;
        let mut last_fid = 0u32;
        let mut last_pub = 0u64;
        let mut last_tok = u64::MAX;
        let mut tcount: i64 = 0;
        let mut last_start: i64 = -1;
        let mut discont = true;
        let mut next_ns = win::qpc_ns();

        'outer: loop {
            if unsafe { WaitForSingleObject(stop, 0) } == WAIT_OBJECT_0 {
                break;
            }

            let (view, ev0, ev1, vstat) = {
                let mut shared = self.lock_shared();
                if shared.shm.is_none() {
                    shared.shm = ShmSet::open(&win::frame_map_name(), geometry);
                }
                match &shared.shm {
                    Some(set) => (set.view(), set.event(0), set.event(1), set.vstat()),
                    None => (
                        ptr::null_mut(),
                        HANDLE::default(),
                        HANDLE::default(),
                        ptr::null_mut(),
                    ),
                }
            };

            let mut fresh = false;
            let mut slot = 0u32;
            let mut seq = 0i32;
            if !view.is_null() {
                let latest = unsafe { shm::latest_seq(view) };
                if let Some((peek_slot, peek_seq)) = unsafe { shm::peek(view) } {
                    let token = ((peek_slot as u64) << 32) | (peek_seq as u32 as u64);
                    if token != last_tok {
                        fresh = true;
                        slot = peek_slot;
                        seq = peek_seq;
                    }
                }
                if !fresh {
                    let now = win::qpc_ns();
                    if now < next_ns {
                        let mut ms = (next_ns - now).div_ceil(1_000_000) as u32;
                        let event = if latest & 1 != 0 { ev0 } else { ev1 };
                        let handles = [stop, event];
                        let count = if event.is_invalid() { 1 } else { 2 };
                        if event.is_invalid() && ms > 1 {
                            ms = 1;
                        }
                        let waited =
                            unsafe { WaitForMultipleObjects(&handles[..count], false, ms) };
                        if waited == WAIT_OBJECT_0 {
                            break;
                        }
                        if let Some((peek_slot, peek_seq)) = unsafe { shm::peek(view) } {
                            let token = ((peek_slot as u64) << 32) | (peek_seq as u32 as u64);
                            if token != last_tok {
                                fresh = true;
                                slot = peek_slot;
                                seq = peek_seq;
                            }
                        }
                        if !fresh && win::qpc_ns() < next_ns {
                            continue;
                        }
                    }
                }
            } else {
                // No receiver mapping: keep the 240 Hz cadence, but never
                // fabricate a frame; only the last good frame may repeat.
                loop {
                    let now = win::qpc_ns();
                    if now >= next_ns {
                        break;
                    }
                    let remaining_us = (next_ns - now) / 1000;
                    let wait_ms = if remaining_us > 2500 {
                        2
                    } else if remaining_us > 1100 {
                        1
                    } else {
                        0
                    };
                    if unsafe { WaitForSingleObject(stop, wait_ms) } == WAIT_OBJECT_0 {
                        break 'outer;
                    }
                    if remaining_us <= 1100 {
                        std::thread::yield_now();
                    }
                }
            }

            // No fresh frame, no verified frame yet: emit nothing at all.
            if !fresh && !has_last_good {
                tcount = tcount.saturating_add(frame_100ns);
                discont = true;
                next_ns = next_ns.saturating_add(frame_ns);
                let now = win::qpc_ns();
                if now > next_ns.saturating_add(20_000_000) {
                    next_ns = now;
                }
                continue;
            }

            let (input, alloc, clock, state, t0) = {
                let shared = self.lock_shared();
                (
                    shared.input.clone(),
                    shared.alloc.clone(),
                    shared.clock.clone(),
                    shared.state,
                    shared.t_start,
                )
            };
            let (Some(input), Some(alloc)) = (input, alloc) else {
                break;
            };

            let mut sample = None;
            if unsafe { alloc.GetBuffer(&mut sample, None, None, 0) }.is_err() {
                break; // decommitted / stopping
            }
            let Some(sample) = sample else {
                break;
            };
            let buf = unsafe { sample.GetPointer() }.unwrap_or(ptr::null_mut());

            let mut out_fid = 0u32;
            let mut out_pub = 0u64;
            let mut flags = 0u32;
            let mut ok = false;
            if fresh
                && !buf.is_null()
                && let Some(copy) = unsafe { shm::copy_slot(view, slot, seq, buf) }
            {
                ok = true;
                out_fid = copy.frame_id;
                out_pub = copy.qpc_pub_ns;
                last_tok = ((slot as u64) << 32) | (seq as u32 as u64);
            }
            if !ok && !buf.is_null() && has_last_good {
                unsafe {
                    ptr::copy_nonoverlapping(last_good.as_ptr(), buf, frame_bytes);
                }
                out_fid = last_fid;
                out_pub = last_pub;
                flags = VSTAT_FLAG_REPEAT;
            }
            if !ok && !has_last_good {
                drop(sample);
                discont = true;
                next_ns = win::qpc_ns().saturating_add(frame_ns);
                continue;
            }
            if ok {
                unsafe {
                    ptr::copy_nonoverlapping(buf, last_good.as_mut_ptr(), frame_bytes);
                }
                has_last_good = true;
                last_fid = out_fid;
                last_pub = out_pub;
            }

            let _ = unsafe { sample.SetActualDataLength(frame_bytes as i32) };
            let mut ts = tcount;
            if state == STATE_RUNNING
                && let Some(clock) = &clock
                && let Ok(now) = unsafe { clock.GetTime() }
            {
                ts = now - t0;
            }
            if ts <= last_start {
                ts = last_start.saturating_add(1);
            }
            let te = ts.saturating_add(frame_100ns);
            let _ = unsafe { sample.SetTime(Some(&ts as *const i64), Some(&te as *const i64)) };
            let _ = unsafe { sample.SetSyncPoint(true) };
            if discont {
                let _ = unsafe { sample.SetDiscontinuity(true) };
                discont = false;
            }
            last_start = ts;
            tcount = te;

            let deliver = win::qpc_ns();
            let received = unsafe { input.Receive(&sample) };
            if !vstat.is_null() {
                unsafe {
                    shm::vstat_push(vstat, out_fid, out_pub, deliver, flags);
                }
            }
            let now = win::qpc_ns();
            if ok {
                next_ns = now.saturating_add(first_repeat_ns);
            } else {
                next_ns = next_ns.saturating_add(frame_ns);
                if now > next_ns.saturating_add(20_000_000) {
                    next_ns = now;
                }
            }
            if received.is_err() {
                break;
            }
        }
    }
}

// ---------- object construction / refcounts ----------

unsafe fn create_filter() -> *mut FilterObj {
    G_OBJS.fetch_add(1, Ordering::SeqCst);
    let mut boxed = Box::new(FilterObj {
        vtbl: &FILTER_VTBL,
        owner: ptr::null_mut(),
        refs: AtomicU32::new(1),
        misc: FilterTear {
            vtbl: &FILTER_MISC_VTBL,
            owner: ptr::null_mut(),
        },
        core: Arc::new(FilterCore::new()),
        pin: ptr::null_mut(),
    });
    let filter: *mut FilterObj = &mut *boxed;
    (*filter).owner = filter;
    (*filter).misc.owner = filter;
    let pin = create_pin(filter);
    (*filter).pin = pin;
    Box::into_raw(boxed)
}

unsafe fn create_pin(filter: *mut FilterObj) -> *mut PinObj {
    let mut boxed = Box::new(PinObj {
        vtbl: &PIN_VTBL,
        owner: ptr::null_mut(),
        filter,
        stream_config: PinTear {
            vtbl: &PIN_STREAM_CONFIG_VTBL,
            owner: ptr::null_mut(),
        },
        ks_prop: PinTear {
            vtbl: &PIN_KS_PROPERTY_SET_VTBL,
            owner: ptr::null_mut(),
        },
        spec_pp: PinTear {
            vtbl: &PIN_SPECIFY_PAGES_VTBL,
            owner: ptr::null_mut(),
        },
        ks_ctrl: PinTear {
            vtbl: &PIN_KS_CONTROL_VTBL,
            owner: ptr::null_mut(),
        },
        ks_pin: PinTear {
            vtbl: &PIN_KS_PIN_VTBL,
            owner: ptr::null_mut(),
        },
    });
    let pin: *mut PinObj = &mut *boxed;
    (*pin).owner = pin;
    (*pin).stream_config.owner = pin;
    (*pin).ks_prop.owner = pin;
    (*pin).spec_pp.owner = pin;
    (*pin).ks_ctrl.owner = pin;
    (*pin).ks_pin.owner = pin;
    Box::into_raw(boxed)
}

impl Drop for FilterObj {
    fn drop(&mut self) {
        FilterCore::deactivate(&self.core);
        unsafe {
            if !self.pin.is_null() {
                drop(Box::from_raw(self.pin));
                self.pin = ptr::null_mut();
            }
        }
        G_OBJS.fetch_sub(1, Ordering::SeqCst);
    }
}

unsafe fn filter_addref_impl(filter: *mut FilterObj) -> u32 {
    (*filter).refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe fn filter_release_impl(filter: *mut FilterObj) -> u32 {
    let previous = (*filter).refs.fetch_sub(1, Ordering::AcqRel);
    let now = previous.saturating_sub(1);
    if previous == 1 {
        drop(Box::from_raw(filter));
    }
    now
}

unsafe fn filter_qi_impl(
    filter: *mut FilterObj,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    if riid.is_null() || filter.is_null() {
        *ppv = ptr::null_mut();
        return E_POINTER;
    }
    let iid = *riid;
    let out = if iid == IID_IUNKNOWN
        || iid == IID_IPERSIST
        || iid == <IMediaFilter as Interface>::IID
        || iid == <IBaseFilter as Interface>::IID
    {
        filter as *mut c_void
    } else if iid == <IAMFilterMiscFlags as Interface>::IID {
        &(*filter).misc as *const _ as *mut c_void
    } else {
        *ppv = ptr::null_mut();
        return E_NOINTERFACE;
    };
    *ppv = out;
    filter_addref_impl(filter);
    S_OK
}

unsafe fn pin_addref_impl(pin: *mut PinObj) -> u32 {
    filter_addref_impl((*pin).filter)
}

unsafe fn pin_release_impl(pin: *mut PinObj) -> u32 {
    filter_release_impl((*pin).filter)
}

unsafe fn pin_qi_impl(pin: *mut PinObj, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    if riid.is_null() || pin.is_null() {
        *ppv = ptr::null_mut();
        return E_POINTER;
    }
    let iid = *riid;
    let out = if iid == IID_IUNKNOWN || iid == <IPin as Interface>::IID {
        pin as *mut c_void
    } else if iid == <IAMStreamConfig as Interface>::IID {
        &(*pin).stream_config as *const _ as *mut c_void
    } else if iid == <IKsPropertySet as Interface>::IID {
        &(*pin).ks_prop as *const _ as *mut c_void
    } else if iid == <ISpecifyPropertyPages as Interface>::IID {
        &(*pin).spec_pp as *const _ as *mut c_void
    } else if iid == <IKsControl as Interface>::IID {
        &(*pin).ks_ctrl as *const _ as *mut c_void
    } else if iid == <IKsPin as Interface>::IID {
        &(*pin).ks_pin as *const _ as *mut c_void
    } else {
        *ppv = ptr::null_mut();
        return E_NOINTERFACE;
    };
    *ppv = out;
    pin_addref_impl(pin);
    S_OK
}

unsafe extern "system" fn filter_addref_any(this: *mut c_void) -> u32 {
    let filter = filter_owner(this);
    if filter.is_null() {
        return 0;
    }
    filter_addref_impl(filter)
}

unsafe extern "system" fn filter_release_any(this: *mut c_void) -> u32 {
    let filter = filter_owner(this);
    if filter.is_null() {
        return 0;
    }
    filter_release_impl(filter)
}

unsafe extern "system" fn filter_qi_any(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() {
        return E_POINTER;
    }
    filter_qi_impl(filter, riid, ppv)
}

unsafe extern "system" fn pin_addref_any(this: *mut c_void) -> u32 {
    let pin = pin_owner(this);
    if pin.is_null() {
        return 0;
    }
    pin_addref_impl(pin)
}

unsafe extern "system" fn pin_release_any(this: *mut c_void) -> u32 {
    let pin = pin_owner(this);
    if pin.is_null() {
        return 0;
    }
    pin_release_impl(pin)
}

unsafe extern "system" fn pin_qi_any(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() {
        return E_POINTER;
    }
    pin_qi_impl(pin, riid, ppv)
}

// ---------- IBaseFilter / IAMFilterMiscFlags ----------

unsafe extern "system" fn filter_get_class_id(this: *mut c_void, clsid: *mut GUID) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() || clsid.is_null() {
        return E_POINTER;
    }
    *clsid = CLSID_VCAM;
    S_OK
}

unsafe extern "system" fn filter_stop(this: *mut c_void) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() {
        return E_POINTER;
    }
    (*filter).core.stop()
}

unsafe extern "system" fn filter_pause(this: *mut c_void) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() {
        return E_POINTER;
    }
    (*filter).core.pause()
}

unsafe extern "system" fn filter_run(this: *mut c_void, t_start: i64) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() {
        return E_POINTER;
    }
    (*filter).core.run(t_start)
}

unsafe extern "system" fn filter_get_state(
    this: *mut c_void,
    _timeout_ms: u32,
    state: *mut i32,
) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() || state.is_null() {
        return E_POINTER;
    }
    let shared = (*filter).core.lock_shared();
    *state = shared.state;
    if shared.state == STATE_PAUSED {
        VFW_S_CANT_CUE
    } else {
        S_OK
    }
}

unsafe extern "system" fn filter_set_sync_source(this: *mut c_void, clock: *mut c_void) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() {
        return E_POINTER;
    }
    if !clock.is_null() {
        com_addref(clock);
    }
    let mut shared = (*filter).core.lock_shared();
    shared.clock = if clock.is_null() {
        None
    } else {
        Some(IReferenceClock::from_raw(clock))
    };
    S_OK
}

unsafe extern "system" fn filter_get_sync_source(
    this: *mut c_void,
    clock: *mut *mut c_void,
) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() || clock.is_null() {
        return E_POINTER;
    }
    let shared = (*filter).core.lock_shared();
    match &shared.clock {
        Some(clock_ref) => {
            let raw = clock_ref.as_raw();
            com_addref(raw);
            *clock = raw;
        }
        None => *clock = ptr::null_mut(),
    }
    S_OK
}

unsafe extern "system" fn filter_enum_pins(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() || out.is_null() {
        return E_POINTER;
    }
    let enumerator = create_enum_pins((*filter).pin);
    *out = enumerator as *mut c_void;
    S_OK
}

unsafe extern "system" fn filter_find_pin(
    this: *mut c_void,
    id: *const u16,
    out: *mut *mut c_void,
) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() || id.is_null() || out.is_null() {
        return E_POINTER;
    }
    if !wide_eq(id, &wide(win::PIN_ID)) {
        *out = ptr::null_mut();
        return VFW_E_NOT_FOUND;
    }
    let pin = (*filter).pin;
    *out = pin as *mut c_void;
    pin_addref_impl(pin);
    S_OK
}

unsafe extern "system" fn filter_query_filter_info(
    this: *mut c_void,
    info: *mut FilterInfoRaw,
) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() || info.is_null() {
        return E_POINTER;
    }
    let shared = (*filter).core.lock_shared();
    if !shared.graph.is_null() {
        com_addref(shared.graph);
    }
    (*info).pGraph = shared.graph;
    copy_name(&mut (*info).achName, &wide(win::VCAM_NAME));
    S_OK
}

unsafe extern "system" fn filter_join_filter_graph(
    this: *mut c_void,
    graph: *mut c_void,
    _name: *const u16,
) -> HRESULT {
    let filter = filter_owner(this);
    if filter.is_null() {
        return E_POINTER;
    }
    (*filter).core.lock_shared().graph = graph;
    S_OK
}

unsafe extern "system" fn filter_query_pin_version(
    _this: *mut c_void,
    _version: *mut i32,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn filter_query_vendor_info(
    _this: *mut c_void,
    _info: *mut *mut u16,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn filter_get_misc_flags(_this: *mut c_void) -> u32 {
    AM_FILTER_MISC_FLAGS_IS_SOURCE
}

// ---------- IPin ----------

unsafe extern "system" fn pin_connect(
    this: *mut c_void,
    peer_ptr: *mut c_void,
    media_type: *const MediaType,
) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || peer_ptr.is_null() {
        return E_POINTER;
    }
    let filter = (*pin).filter;
    let core = &(*filter).core;
    let geometry = core.geometry;
    let _control = core.lock_control();
    {
        let shared = core.lock_shared();
        if shared.connected || shared.peer.is_some() {
            return VFW_E_ALREADY_CONNECTED;
        }
    }

    let owned_type = if media_type.is_null() {
        let allocated = media::alloc_media_type(geometry);
        if allocated.is_null() {
            return E_OUTOFMEMORY;
        }
        Some(allocated)
    } else {
        None
    };
    let use_type = owned_type.unwrap_or(media_type as *mut MediaType);

    let result = (|| -> HRESULT {
        if !media::matches_type(use_type, geometry) {
            return VFW_E_TYPE_NOT_ACCEPTED;
        }
        let peer_view = std::mem::ManuallyDrop::new(IPin::from_raw(peer_ptr));
        let direction = match peer_view.QueryDirection() {
            Ok(direction) => direction,
            Err(_) => return VFW_E_INVALID_DIRECTION,
        };
        if direction != PINDIR_INPUT {
            return VFW_E_INVALID_DIRECTION;
        }
        let self_pin = std::mem::ManuallyDrop::new(IPin::from_raw(pin as *mut c_void));
        if let Err(error) =
            peer_view.ReceiveConnection(&*self_pin, use_type as *const AM_MEDIA_TYPE)
        {
            return error.code();
        }

        let input = match peer_view.cast::<IMemInputPin>() {
            Ok(input) => input,
            Err(_) => {
                let _ = peer_view.Disconnect();
                return VFW_E_NO_TRANSPORT;
            }
        };
        let alloc: IMemAllocator = match input.GetAllocator() {
            Ok(allocator) => allocator,
            Err(_) => match CoCreateInstance(
                &CLSID_MemoryAllocator,
                None::<&windows::core::IUnknown>,
                CLSCTX_INPROC_SERVER,
            ) {
                Ok(allocator) => allocator,
                Err(error) => {
                    let _ = peer_view.Disconnect();
                    return error.code();
                }
            },
        };

        let properties = ALLOCATOR_PROPERTIES {
            cBuffers: 2,
            cbBuffer: geometry.frame_bytes() as i32,
            cbAlign: 1,
            cbPrefix: 0,
        };
        let actual = match alloc.SetProperties(&properties) {
            Ok(actual) => actual,
            Err(error) => {
                let _ = peer_view.Disconnect();
                return error.code();
            }
        };
        if actual.cbBuffer < geometry.frame_bytes() as i32 {
            let _ = peer_view.Disconnect();
            return VFW_E_SIZENOTSET;
        }
        if let Err(error) = input.NotifyAllocator(&alloc, false) {
            let _ = peer_view.Disconnect();
            return error.code();
        }

        {
            let mut shared = core.lock_shared();
            let hr = media::copy_media_type(&mut shared.mt, use_type);
            if failed(hr) {
                let _ = peer_view.Disconnect();
                return hr;
            }
            com_addref(peer_ptr);
            shared.peer = Some(IPin::from_raw(peer_ptr));
            shared.input = Some(input);
            shared.alloc = Some(alloc);
            shared.connected = true;
        }
        S_OK
    })();

    if let Some(owned) = owned_type {
        media::free_media_type(owned);
    }
    result
}

unsafe extern "system" fn pin_receive_connection(
    _this: *mut c_void,
    _peer: *mut c_void,
    _media_type: *const MediaType,
) -> HRESULT {
    E_UNEXPECTED
}

unsafe extern "system" fn pin_disconnect(this: *mut c_void) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() {
        return E_POINTER;
    }
    let filter = (*pin).filter;
    let core = &(*filter).core;
    let _control = core.lock_control();
    core.deactivate_locked();
    let (peer, input, alloc) = {
        let mut shared = core.lock_shared();
        if !shared.connected && shared.peer.is_none() {
            return S_FALSE;
        }
        let peer = shared.peer.take();
        let input = shared.input.take();
        let alloc = shared.alloc.take();
        media::clear_media_type(&mut shared.mt);
        shared.connected = false;
        (peer, input, alloc)
    };
    drop(alloc);
    drop(input);
    drop(peer);
    S_OK
}

unsafe extern "system" fn pin_connected_to(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || out.is_null() {
        return E_POINTER;
    }
    let filter = (*pin).filter;
    let shared = (*filter).core.lock_shared();
    match &shared.peer {
        Some(peer) => {
            let raw = peer.as_raw();
            com_addref(raw);
            *out = raw;
            S_OK
        }
        None => {
            *out = ptr::null_mut();
            VFW_E_NOT_CONNECTED
        }
    }
}

unsafe extern "system" fn pin_connection_media_type(
    this: *mut c_void,
    out: *mut MediaType,
) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || out.is_null() {
        return E_POINTER;
    }
    let filter = (*pin).filter;
    let shared = (*filter).core.lock_shared();
    if !shared.connected {
        return VFW_E_NOT_CONNECTED;
    }
    media::copy_media_type(out, &shared.mt)
}

unsafe extern "system" fn pin_query_pin_info(this: *mut c_void, info: *mut PinInfoRaw) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || info.is_null() {
        return E_POINTER;
    }
    let filter = (*pin).filter;
    (*info).pFilter = filter as *mut c_void;
    filter_addref_impl(filter);
    (*info).dir = PINDIR_OUTPUT;
    copy_name(&mut (*info).achName, &wide(win::PIN_ID));
    S_OK
}

unsafe extern "system" fn pin_query_direction(this: *mut c_void, direction: *mut i32) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || direction.is_null() {
        return E_POINTER;
    }
    *direction = PINDIR_OUTPUT;
    S_OK
}

unsafe extern "system" fn pin_query_id(this: *mut c_void, out: *mut *mut u16) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || out.is_null() {
        return E_POINTER;
    }
    let id = wide(win::PIN_ID);
    let bytes = id.len() * size_of::<u16>();
    let memory = CoTaskMemAlloc(bytes) as *mut u16;
    if memory.is_null() {
        return E_OUTOFMEMORY;
    }
    ptr::copy_nonoverlapping(id.as_ptr(), memory, id.len());
    *out = memory;
    S_OK
}

unsafe extern "system" fn pin_query_accept(
    this: *mut c_void,
    media_type: *const MediaType,
) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || media_type.is_null() {
        return E_POINTER;
    }
    let filter = (*pin).filter;
    let geometry = unsafe { &(*filter).core }.geometry;
    if media::matches_type(media_type, geometry) {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe extern "system" fn pin_enum_media_types(
    this: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || out.is_null() {
        return E_POINTER;
    }
    let filter = (*pin).filter;
    let geometry = unsafe { &(*filter).core }.geometry;
    let enumerator = create_enum_media_types(geometry);
    *out = enumerator as *mut c_void;
    S_OK
}

unsafe extern "system" fn pin_query_internal_connections(
    _this: *mut c_void,
    _pins: *mut *mut c_void,
    _count: *mut u32,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn pin_end_of_stream(_this: *mut c_void) -> HRESULT {
    S_OK
}

unsafe extern "system" fn pin_begin_flush(_this: *mut c_void) -> HRESULT {
    S_OK
}

unsafe extern "system" fn pin_end_flush(_this: *mut c_void) -> HRESULT {
    S_OK
}

unsafe extern "system" fn pin_new_segment(
    _this: *mut c_void,
    _start: i64,
    _stop: i64,
    _rate: f64,
) -> HRESULT {
    S_OK
}

// ---------- IAMStreamConfig ----------

unsafe extern "system" fn stream_set_format(
    this: *mut c_void,
    media_type: *const MediaType,
) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || media_type.is_null() {
        return E_POINTER;
    }
    let filter = (*pin).filter;
    let core = &(*filter).core;
    if core.lock_shared().connected {
        return VFW_E_ALREADY_CONNECTED;
    }
    if media::matches_type(media_type, core.geometry) {
        S_OK
    } else {
        VFW_E_TYPE_NOT_ACCEPTED
    }
}

unsafe extern "system" fn stream_get_format(
    this: *mut c_void,
    out: *mut *mut MediaType,
) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || out.is_null() {
        return E_POINTER;
    }
    let filter = (*pin).filter;
    let geometry = unsafe { &(*filter).core }.geometry;
    let media_type = media::alloc_media_type(geometry);
    if media_type.is_null() {
        return E_OUTOFMEMORY;
    }
    *out = media_type;
    S_OK
}

unsafe extern "system" fn stream_get_number_of_capabilities(
    this: *mut c_void,
    count: *mut i32,
    size: *mut i32,
) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || count.is_null() || size.is_null() {
        return E_POINTER;
    }
    *count = 1;
    *size = size_of::<VideoStreamConfigCaps>() as i32;
    S_OK
}

unsafe extern "system" fn stream_get_stream_caps(
    this: *mut c_void,
    index: i32,
    out: *mut *mut MediaType,
    caps: *mut u8,
) -> HRESULT {
    let pin = pin_owner(this);
    if pin.is_null() || !(0..1).contains(&index) || out.is_null() || caps.is_null() {
        return E_INVALIDARG;
    }
    let filter = (*pin).filter;
    let geometry = unsafe { &(*filter).core }.geometry;
    let media_type = media::alloc_media_type(geometry);
    if media_type.is_null() {
        return E_OUTOFMEMORY;
    }
    media::fill_stream_caps(geometry, caps);
    *out = media_type;
    S_OK
}

// ---------- IKsPropertySet ----------

unsafe extern "system" fn ks_prop_set(
    _this: *mut c_void,
    _guid: *const GUID,
    _property: u32,
    _in_data: *const c_void,
    _in_size: u32,
    _out_data: *const c_void,
    _out_size: u32,
) -> HRESULT {
    E_PROP_ID_UNSUPPORTED
}

unsafe extern "system" fn ks_prop_get(
    this: *mut c_void,
    guid: *const GUID,
    property: u32,
    _in_data: *const c_void,
    _in_size: u32,
    out_data: *mut c_void,
    out_size: u32,
    bytes: *mut u32,
) -> HRESULT {
    if pin_owner(this).is_null() || guid.is_null() {
        return E_POINTER;
    }
    if *guid != AMPROPSETID_Pin || property != AMPROPERTY_PIN_CATEGORY {
        return E_PROP_ID_UNSUPPORTED;
    }
    if out_size < size_of::<GUID>() as u32 || out_data.is_null() || bytes.is_null() {
        return E_INVALIDARG;
    }
    ptr::copy_nonoverlapping(
        &PIN_CATEGORY_CAPTURE as *const GUID as *const u8,
        out_data as *mut u8,
        size_of::<GUID>(),
    );
    *bytes = size_of::<GUID>() as u32;
    S_OK
}

unsafe extern "system" fn ks_prop_query_supported(
    this: *mut c_void,
    guid: *const GUID,
    property: u32,
    support: *mut u32,
) -> HRESULT {
    if pin_owner(this).is_null() || support.is_null() {
        return E_POINTER;
    }
    if guid.is_null() || *guid != AMPROPSETID_Pin || property != AMPROPERTY_PIN_CATEGORY {
        return E_PROP_ID_UNSUPPORTED;
    }
    *support = KSPROPERTY_SUPPORT_GET;
    S_OK
}

// ---------- ISpecifyPropertyPages ----------

unsafe extern "system" fn specify_pages_get_pages(
    this: *mut c_void,
    pages: *mut CaUuidRaw,
) -> HRESULT {
    if pin_owner(this).is_null() || pages.is_null() {
        return E_POINTER;
    }
    (*pages).cElems = 0;
    (*pages).pElems = ptr::null_mut();
    S_OK
}

// ---------- IKsControl ----------

unsafe extern "system" fn ks_control_property(
    _this: *mut c_void,
    _property: *mut c_void,
    _property_size: u32,
    _data: *mut c_void,
    _data_size: u32,
    bytes: *mut u32,
) -> HRESULT {
    if !bytes.is_null() {
        *bytes = 0;
    }
    STATUS_NOT_SUPPORTED
}

unsafe extern "system" fn ks_control_method(
    _this: *mut c_void,
    _method: *mut c_void,
    _method_size: u32,
    _data: *mut c_void,
    _data_size: u32,
    bytes: *mut u32,
) -> HRESULT {
    if !bytes.is_null() {
        *bytes = 0;
    }
    STATUS_NOT_SUPPORTED
}

unsafe extern "system" fn ks_control_event(
    _this: *mut c_void,
    _event: *mut c_void,
    _event_size: u32,
    _data: *mut c_void,
    _data_size: u32,
    bytes: *mut u32,
) -> HRESULT {
    if !bytes.is_null() {
        *bytes = 0;
    }
    STATUS_NOT_SUPPORTED
}

// ---------- IKsPin stubs ----------

unsafe extern "system" fn ks_pin_query_mediums(
    _this: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    if !out.is_null() {
        *out = ptr::null_mut();
    }
    E_NOTIMPL
}

unsafe extern "system" fn ks_pin_query_interfaces(
    _this: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    if !out.is_null() {
        *out = ptr::null_mut();
    }
    E_NOTIMPL
}

unsafe extern "system" fn ks_pin_create_sink_pin_handle(
    _this: *mut c_void,
    _a: u32,
    _b: u32,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn ks_pin_get_current_communication(
    _this: *mut c_void,
    _a: *mut c_void,
    _b: *mut c_void,
    _c: *mut c_void,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn ks_pin_propagate_acquire(_this: *mut c_void) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn ks_pin_deliver(
    _this: *mut c_void,
    _data: *mut c_void,
    _size: u32,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn ks_pin_media_samples_completed(
    _this: *mut c_void,
    _data: *mut c_void,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn ks_pin_peek_allocator(_this: *mut c_void, _index: u32) -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "system" fn ks_pin_receive_allocator(
    _this: *mut c_void,
    _allocator: *mut c_void,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn ks_pin_renegotiate(_this: *mut c_void) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn ks_pin_pass_event_to_filter(
    _this: *mut c_void,
    _event: *mut c_void,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn ks_pin_set_pin_framing_cache(
    _this: *mut c_void,
    _a: *mut c_void,
    _b: *mut c_void,
    _c: u32,
) -> HRESULT {
    E_NOTIMPL
}

// ---------- vtables ----------

static FILTER_VTBL: IBaseFilterVtbl = IBaseFilterVtbl {
    base__: IUnknownVtbl {
        QueryInterface: filter_qi_any,
        AddRef: filter_addref_any,
        Release: filter_release_any,
    },
    GetClassID: filter_get_class_id,
    Stop: filter_stop,
    Pause: filter_pause,
    Run: filter_run,
    GetState: filter_get_state,
    SetSyncSource: filter_set_sync_source,
    GetSyncSource: filter_get_sync_source,
    EnumPins: filter_enum_pins,
    FindPin: filter_find_pin,
    QueryFilterInfo: filter_query_filter_info,
    JoinFilterGraph: filter_join_filter_graph,
    QueryPinVersion: filter_query_pin_version,
    QueryVendorInfo: filter_query_vendor_info,
};

static FILTER_MISC_VTBL: IAMFilterMiscFlagsVtbl = IAMFilterMiscFlagsVtbl {
    base__: IUnknownVtbl {
        QueryInterface: filter_qi_any,
        AddRef: filter_addref_any,
        Release: filter_release_any,
    },
    GetMiscFlags: filter_get_misc_flags,
};

static PIN_VTBL: IPinVtbl = IPinVtbl {
    base__: IUnknownVtbl {
        QueryInterface: pin_qi_any,
        AddRef: pin_addref_any,
        Release: pin_release_any,
    },
    Connect: pin_connect,
    ReceiveConnection: pin_receive_connection,
    Disconnect: pin_disconnect,
    ConnectedTo: pin_connected_to,
    ConnectionMediaType: pin_connection_media_type,
    QueryPinInfo: pin_query_pin_info,
    QueryDirection: pin_query_direction,
    QueryId: pin_query_id,
    QueryAccept: pin_query_accept,
    EnumMediaTypes: pin_enum_media_types,
    QueryInternalConnections: pin_query_internal_connections,
    EndOfStream: pin_end_of_stream,
    BeginFlush: pin_begin_flush,
    EndFlush: pin_end_flush,
    NewSegment: pin_new_segment,
};

static PIN_STREAM_CONFIG_VTBL: IAMStreamConfigVtbl = IAMStreamConfigVtbl {
    base__: IUnknownVtbl {
        QueryInterface: pin_qi_any,
        AddRef: pin_addref_any,
        Release: pin_release_any,
    },
    SetFormat: stream_set_format,
    GetFormat: stream_get_format,
    GetNumberOfCapabilities: stream_get_number_of_capabilities,
    GetStreamCaps: stream_get_stream_caps,
};

static PIN_KS_PROPERTY_SET_VTBL: IKsPropertySetVtbl = IKsPropertySetVtbl {
    base__: IUnknownVtbl {
        QueryInterface: pin_qi_any,
        AddRef: pin_addref_any,
        Release: pin_release_any,
    },
    Set: ks_prop_set,
    Get: ks_prop_get,
    QuerySupported: ks_prop_query_supported,
};

static PIN_SPECIFY_PAGES_VTBL: ISpecifyPropertyPagesVtbl = ISpecifyPropertyPagesVtbl {
    base__: IUnknownVtbl {
        QueryInterface: pin_qi_any,
        AddRef: pin_addref_any,
        Release: pin_release_any,
    },
    GetPages: specify_pages_get_pages,
};

static PIN_KS_CONTROL_VTBL: IKsControlVtbl = IKsControlVtbl {
    base__: IUnknownVtbl {
        QueryInterface: pin_qi_any,
        AddRef: pin_addref_any,
        Release: pin_release_any,
    },
    KsProperty: ks_control_property,
    KsMethod: ks_control_method,
    KsEvent: ks_control_event,
};

static PIN_KS_PIN_VTBL: IKsPinVtbl = IKsPinVtbl {
    base__: IUnknownVtbl {
        QueryInterface: pin_qi_any,
        AddRef: pin_addref_any,
        Release: pin_release_any,
    },
    KsQueryMediums: ks_pin_query_mediums,
    KsQueryInterfaces: ks_pin_query_interfaces,
    KsCreateSinkPinHandle: ks_pin_create_sink_pin_handle,
    KsGetCurrentCommunication: ks_pin_get_current_communication,
    KsPropagateAcquire: ks_pin_propagate_acquire,
    KsDeliver: ks_pin_deliver,
    KsMediaSamplesCompleted: ks_pin_media_samples_completed,
    KsPeekAllocator: ks_pin_peek_allocator,
    KsReceiveAllocator: ks_pin_receive_allocator,
    KsRenegotiate: ks_pin_renegotiate,
    KsPassEventToFilter: ks_pin_pass_event_to_filter,
    KsSetPinFramingCache: ks_pin_set_pin_framing_cache,
};

// ---------- IEnumPins ----------

#[repr(C)]
struct EnumPinsObj {
    vtbl: *const IEnumPinsVtbl,
    refs: AtomicU32,
    pin: *mut PinObj,
    done: AtomicU32,
}

static ENUM_PINS_VTBL: IEnumPinsVtbl = IEnumPinsVtbl {
    base__: IUnknownVtbl {
        QueryInterface: enum_pins_qi,
        AddRef: enum_pins_addref,
        Release: enum_pins_release,
    },
    Next: enum_pins_next,
    Skip: enum_pins_skip,
    Reset: enum_pins_reset,
    Clone: enum_pins_clone,
};

unsafe fn create_enum_pins(pin: *mut PinObj) -> *mut EnumPinsObj {
    let boxed = Box::new(EnumPinsObj {
        vtbl: &ENUM_PINS_VTBL,
        refs: AtomicU32::new(1),
        pin,
        done: AtomicU32::new(0),
    });
    pin_addref_impl(pin);
    Box::into_raw(boxed)
}

impl Drop for EnumPinsObj {
    fn drop(&mut self) {
        unsafe { pin_release_impl(self.pin) };
    }
}

unsafe extern "system" fn enum_pins_qi(
    this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    if riid.is_null() {
        *out = ptr::null_mut();
        return E_POINTER;
    }
    if *riid != IID_IUNKNOWN && *riid != <IEnumPins as Interface>::IID {
        *out = ptr::null_mut();
        return E_NOINTERFACE;
    }
    *out = this;
    enum_pins_addref(this);
    S_OK
}

unsafe extern "system" fn enum_pins_addref(this: *mut c_void) -> u32 {
    let object = &*(this as *const EnumPinsObj);
    object.refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn enum_pins_release(this: *mut c_void) -> u32 {
    let object = &*(this as *const EnumPinsObj);
    let previous = object.refs.fetch_sub(1, Ordering::AcqRel);
    let now = previous.saturating_sub(1);
    if previous == 1 {
        drop(Box::from_raw(this as *mut EnumPinsObj));
    }
    now
}

unsafe extern "system" fn enum_pins_next(
    this: *mut c_void,
    count: u32,
    out: *mut *mut c_void,
    fetched: *mut u32,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let object = &*(this as *const EnumPinsObj);
    let mut got = 0u32;
    if count > 0 && object.done.swap(1, Ordering::AcqRel) == 0 {
        *out = object.pin as *mut c_void;
        pin_addref_impl(object.pin);
        got = 1;
    }
    if !fetched.is_null() {
        *fetched = got;
    }
    if got == count { S_OK } else { S_FALSE }
}

unsafe extern "system" fn enum_pins_skip(this: *mut c_void, count: u32) -> HRESULT {
    if count == 0 {
        return S_OK;
    }
    let object = &*(this as *const EnumPinsObj);
    if count == 1 && object.done.swap(1, Ordering::AcqRel) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe extern "system" fn enum_pins_reset(this: *mut c_void) -> HRESULT {
    let object = &*(this as *const EnumPinsObj);
    object.done.store(0, Ordering::Release);
    S_OK
}

unsafe extern "system" fn enum_pins_clone(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let object = &*(this as *const EnumPinsObj);
    let clone = create_enum_pins(object.pin);
    (*clone)
        .done
        .store(object.done.load(Ordering::Acquire), Ordering::Release);
    *out = clone as *mut c_void;
    S_OK
}

// ---------- IEnumMediaTypes ----------

#[repr(C)]
struct EnumMediaTypesObj {
    vtbl: *const IEnumMediaTypesVtbl,
    refs: AtomicU32,
    geometry: StreamGeometry,
    done: AtomicU32,
}

static ENUM_MEDIA_TYPES_VTBL: IEnumMediaTypesVtbl = IEnumMediaTypesVtbl {
    base__: IUnknownVtbl {
        QueryInterface: enum_mt_qi,
        AddRef: enum_mt_addref,
        Release: enum_mt_release,
    },
    Next: enum_mt_next,
    Skip: enum_mt_skip,
    Reset: enum_mt_reset,
    Clone: enum_mt_clone,
};

unsafe fn create_enum_media_types(geometry: StreamGeometry) -> *mut EnumMediaTypesObj {
    G_OBJS.fetch_add(1, Ordering::SeqCst);
    let boxed = Box::new(EnumMediaTypesObj {
        vtbl: &ENUM_MEDIA_TYPES_VTBL,
        refs: AtomicU32::new(1),
        geometry,
        done: AtomicU32::new(0),
    });
    Box::into_raw(boxed)
}

impl Drop for EnumMediaTypesObj {
    fn drop(&mut self) {
        G_OBJS.fetch_sub(1, Ordering::SeqCst);
    }
}

unsafe extern "system" fn enum_mt_qi(
    this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    if riid.is_null() {
        *out = ptr::null_mut();
        return E_POINTER;
    }
    if *riid != IID_IUNKNOWN && *riid != <IEnumMediaTypes as Interface>::IID {
        *out = ptr::null_mut();
        return E_NOINTERFACE;
    }
    *out = this;
    enum_mt_addref(this);
    S_OK
}

unsafe extern "system" fn enum_mt_addref(this: *mut c_void) -> u32 {
    let object = &*(this as *const EnumMediaTypesObj);
    object.refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn enum_mt_release(this: *mut c_void) -> u32 {
    let object = &*(this as *const EnumMediaTypesObj);
    let previous = object.refs.fetch_sub(1, Ordering::AcqRel);
    let now = previous.saturating_sub(1);
    if previous == 1 {
        drop(Box::from_raw(this as *mut EnumMediaTypesObj));
    }
    now
}

unsafe extern "system" fn enum_mt_next(
    this: *mut c_void,
    count: u32,
    out: *mut *mut MediaType,
    fetched: *mut u32,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let object = &*(this as *const EnumMediaTypesObj);
    let mut got = 0u32;
    while got < count && object.done.load(Ordering::Acquire) == 0 {
        let media_type = media::alloc_media_type(object.geometry);
        if media_type.is_null() {
            for index in 0..got as usize {
                media::free_media_type(*out.add(index));
            }
            return E_OUTOFMEMORY;
        }
        *out.add(got as usize) = media_type;
        got += 1;
        object.done.store(1, Ordering::Release);
    }
    if !fetched.is_null() {
        *fetched = got;
    }
    if got == count { S_OK } else { S_FALSE }
}

unsafe extern "system" fn enum_mt_skip(this: *mut c_void, count: u32) -> HRESULT {
    if count == 0 {
        return S_OK;
    }
    let object = &*(this as *const EnumMediaTypesObj);
    if count == 1 && object.done.swap(1, Ordering::AcqRel) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe extern "system" fn enum_mt_reset(this: *mut c_void) -> HRESULT {
    let object = &*(this as *const EnumMediaTypesObj);
    object.done.store(0, Ordering::Release);
    S_OK
}

unsafe extern "system" fn enum_mt_clone(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let object = &*(this as *const EnumMediaTypesObj);
    let clone = create_enum_media_types(object.geometry);
    (*clone)
        .done
        .store(object.done.load(Ordering::Acquire), Ordering::Release);
    *out = clone as *mut c_void;
    S_OK
}

// ---------- IClassFactory ----------

#[repr(C)]
struct FactoryObj {
    vtbl: *const IClassFactoryVtbl,
    refs: AtomicU32,
}

static FACTORY_VTBL: IClassFactoryVtbl = IClassFactoryVtbl {
    base__: IUnknownVtbl {
        QueryInterface: factory_qi,
        AddRef: factory_addref,
        Release: factory_release,
    },
    CreateInstance: factory_create_instance,
    LockServer: factory_lock_server,
};

unsafe fn create_factory() -> *mut FactoryObj {
    G_OBJS.fetch_add(1, Ordering::SeqCst);
    let boxed = Box::new(FactoryObj {
        vtbl: &FACTORY_VTBL,
        refs: AtomicU32::new(1),
    });
    Box::into_raw(boxed)
}

impl Drop for FactoryObj {
    fn drop(&mut self) {
        G_OBJS.fetch_sub(1, Ordering::SeqCst);
    }
}

unsafe extern "system" fn factory_qi(
    this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    if riid.is_null() {
        *out = ptr::null_mut();
        return E_POINTER;
    }
    if *riid != IID_IUNKNOWN && *riid != <IClassFactory as Interface>::IID {
        *out = ptr::null_mut();
        return E_NOINTERFACE;
    }
    *out = this;
    factory_addref(this);
    S_OK
}

unsafe extern "system" fn factory_addref(this: *mut c_void) -> u32 {
    let object = &*(this as *const FactoryObj);
    object.refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn factory_release(this: *mut c_void) -> u32 {
    let object = &*(this as *const FactoryObj);
    let previous = object.refs.fetch_sub(1, Ordering::AcqRel);
    let now = previous.saturating_sub(1);
    if previous == 1 {
        drop(Box::from_raw(this as *mut FactoryObj));
    }
    now
}

unsafe extern "system" fn factory_create_instance(
    _this: *mut c_void,
    outer: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    if out.is_null() {
        return E_POINTER;
    }
    *out = ptr::null_mut();
    let filter = create_filter();
    if filter.is_null() {
        return E_OUTOFMEMORY;
    }
    let hr = filter_qi_impl(filter, riid, out);
    filter_release_impl(filter);
    hr
}

unsafe extern "system" fn factory_lock_server(_this: *mut c_void, lock: i32) -> HRESULT {
    if lock != 0 {
        G_LOCKS.fetch_add(1, Ordering::SeqCst);
    } else {
        G_LOCKS.fetch_sub(1, Ordering::SeqCst);
    }
    S_OK
}

// ---------- DLL exports (called from lib.rs) ----------

/// DllGetClassObject entry point (called by `lib.rs`).
///
/// # Safety
///
/// Non-null `class_id`/`interface_id` must point to readable GUIDs and a
/// non-null `out` must point to a writable pointer slot, all valid for the
/// call.  On `S_OK` the caller receives one owned interface reference and
/// must release it through COM.
pub unsafe fn dll_get_class_object(
    class_id: *const GUID,
    interface_id: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if class_id.is_null() {
        return E_POINTER;
    }
    if *class_id != CLSID_VCAM {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory = create_factory();
    let hr = factory_qi(factory as *mut c_void, interface_id, out);
    factory_release(factory as *mut c_void);
    hr
}

pub fn dll_can_unload_now() -> HRESULT {
    if G_OBJS.load(Ordering::SeqCst) == 0 && G_LOCKS.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

// ---------- registration (HKCU required, HKLM best effort) ----------

/// REGFILTER2 v2 blob: 1 output pin, 1 media type (vids/NV12), pin category
/// PIN_CATEGORY_CAPTURE.  Must stay byte-identical to the C++ filter.
pub const FILTER_DATA: [u8; 104] = [
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x30, 0x70, 0x69, 0x33, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x58, 0x00, 0x00, 0x00, 0x30, 0x74, 0x79, 0x33, 0x00, 0x00, 0x00, 0x00,
    0x38, 0x00, 0x00, 0x00, 0x48, 0x00, 0x00, 0x00, 0x76, 0x69, 0x64, 0x73, 0x00, 0x00, 0x10, 0x00,
    0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71, 0x4e, 0x56, 0x31, 0x32, 0x00, 0x00, 0x10, 0x00,
    0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71, 0x81, 0x42, 0x6c, 0xfb, 0x53, 0x03, 0xd1, 0x11,
    0x90, 0x5f, 0x00, 0x00, 0xc0, 0xcc, 0x16, 0xba,
];

unsafe fn reg_set_string(
    root: HKEY,
    subkey: &[u16],
    value_name: Option<&str>,
    data: &[u16],
) -> bool {
    let mut key = HKEY::default();
    if RegCreateKeyExW(
        root,
        PCWSTR(subkey.as_ptr()),
        None,
        PCWSTR::null(),
        REG_OPTION_NON_VOLATILE,
        KEY_WRITE,
        None,
        &mut key,
        None,
    ) != ERROR_SUCCESS
    {
        return false;
    }
    let name = value_name.map(wide);
    let name_ptr = match &name {
        Some(name) => PCWSTR(name.as_ptr()),
        None => PCWSTR::null(),
    };
    let bytes = core::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
    let rc = RegSetValueExW(key, name_ptr, None, REG_SZ, Some(bytes));
    let _ = RegCloseKey(key);
    rc == ERROR_SUCCESS
}

unsafe fn write_hive(
    root: HKEY,
    clsid_key: &[u16],
    server_key: &[u16],
    instance_key: &[u16],
    module_path: &[u16],
) -> bool {
    let name = wide(win::VCAM_NAME);
    if !reg_set_string(root, clsid_key, None, &name) {
        return false;
    }
    if !reg_set_string(root, server_key, None, module_path) {
        return false;
    }
    if !reg_set_string(root, server_key, Some("ThreadingModel"), &wide("Both")) {
        return false;
    }
    if !reg_set_string(root, instance_key, Some("FriendlyName"), &name) {
        return false;
    }
    if !reg_set_string(root, instance_key, Some("CLSID"), &wide(win::CLSID_STR)) {
        return false;
    }
    let mut key = HKEY::default();
    if RegCreateKeyExW(
        root,
        PCWSTR(instance_key.as_ptr()),
        None,
        PCWSTR::null(),
        REG_OPTION_NON_VOLATILE,
        KEY_WRITE,
        None,
        &mut key,
        None,
    ) != ERROR_SUCCESS
    {
        return false;
    }
    let merit: u32 = 0x0020_0000;
    let _ = RegSetValueExW(
        key,
        PCWSTR(wide("Merit").as_ptr()),
        None,
        REG_DWORD,
        Some(&merit.to_le_bytes()),
    );
    let _ = RegSetValueExW(
        key,
        PCWSTR(wide("FilterData").as_ptr()),
        None,
        REG_BINARY,
        Some(&FILTER_DATA),
    );
    let _ = RegCloseKey(key);
    true
}

pub fn register_server() -> HRESULT {
    let Some(module_path) = win::module_path() else {
        return SELFREG_E_CLASS;
    };
    let clsid_key = wide(win::CLSID_KEY);
    let server_key = wide(&format!("{}\\InprocServer32", win::CLSID_KEY));
    let instance_key = wide(win::INSTANCE_KEY);
    unsafe {
        if !write_hive(
            HKEY_CURRENT_USER,
            &clsid_key,
            &server_key,
            &instance_key,
            &module_path,
        ) {
            return SELFREG_E_CLASS;
        }
        // Best effort like the C++ filter: an unelevated install only needs HKCU.
        let _ = write_hive(
            HKEY_LOCAL_MACHINE,
            &clsid_key,
            &server_key,
            &instance_key,
            &module_path,
        );
    }
    S_OK
}

pub fn unregister_server() -> HRESULT {
    let clsid_key = wide(win::CLSID_KEY);
    let instance_key = wide(win::INSTANCE_KEY);
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(instance_key.as_ptr()));
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(clsid_key.as_ptr()));
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(instance_key.as_ptr()));
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(clsid_key.as_ptr()));
    }
    S_OK
}

// ---------- make sure the helper is used (and keep it public for the diag) ----------

pub fn capabilities_json() -> &'static str {
    crate::capabilities::CAPABILITIES_JSON
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_data_blob_is_the_expected_regfilter2() {
        assert_eq!(FILTER_DATA.len(), 104);
        assert_eq!(&FILTER_DATA[0..4], &[0x02, 0x00, 0x00, 0x00]);
        // Pin category GUID (last 16 bytes) is PIN_CATEGORY_CAPTURE.
        let category = unsafe {
            core::slice::from_raw_parts(&PIN_CATEGORY_CAPTURE as *const GUID as *const u8, 16)
        };
        assert_eq!(&FILTER_DATA[88..104], category);
    }

    #[test]
    fn registration_keys_match_the_public_clsid() {
        assert!(win::CLSID_KEY.contains(win::CLSID_STR));
        assert!(win::INSTANCE_KEY.contains(win::CLSID_STR));
        assert!(win::INSTANCE_KEY.contains(win::VIDEO_INPUT_CATEGORY));
        assert_eq!(win::VCAM_NAME, "VirtualCapture");
        assert_eq!(win::PIN_ID, "Capture");
    }

    #[test]
    fn can_unload_when_no_objects_exist() {
        // Unit tests do not create COM objects; this only asserts the counter
        // gate itself.  A verifier checks the real graph lifetime in-process.
        assert_eq!(dll_can_unload_now(), S_OK);
    }

    #[test]
    fn capabilities_export_payload_is_a_json_object() {
        let json = capabilities_json();
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains("\"shm_version\":3"));
    }
}
