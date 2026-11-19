//! Thread-local capture scheduling, shared by sender, receiver and decoder.
//!
//! The guard must stay on the registering thread and restores its scheduling on
//! every exit path. Loading a data-plane DLL never raises host process priority.
use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentThread,
    GetThreadPriority, SetThreadPriority, SetWaitableTimer, THREAD_PRIORITY_TIME_CRITICAL,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};

/// One private timer per sending thread: no global timer-resolution or process
/// priority changes. An unnamed auto-reset timer cannot interfere with another
/// DLL session. The caller bounds waits and keeps a short QPC precision tail.
struct FrameTimer(HANDLE);

impl FrameTimer {
    fn new() -> Self {
        Self(unsafe {
            CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        })
    }

    fn wait(&self, ns: u64) {
        let due = -(ns.div_ceil(100) as i64);
        if !self.0.is_null()
            && unsafe {
                SetWaitableTimer(self.0, &due, 0, None, std::ptr::null(), 0) != 0
                    && WaitForSingleObject(self.0, 20) == WAIT_OBJECT_0
            }
        {
            return;
        }
        // Failure only changes wakeup precision; the outer QPC check still
        // enforces the exact deadline and never sends early.
        std::thread::sleep(std::time::Duration::from_nanos(ns));
    }
}

impl Drop for FrameTimer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

pub(super) fn wait_for(ns: u64) {
    thread_local! {
        static TIMER: FrameTimer = FrameTimer::new();
    }
    TIMER.with(|timer| timer.wait(ns));
}

// GetThreadPriority returns MAXLONG on failure (THREAD_PRIORITY_ERROR_RETURN).
const PRIORITY_ERROR: i32 = i32::MAX;

pub(super) struct ThreadPriority {
    mmcss: *mut c_void,
    old: i32,
    raised: bool,
    // MMCSS registration belongs to the calling thread, even if this struct's
    // native fields later change to types which implement Send/Sync.
    _same_thread: PhantomData<Rc<()>>,
}

#[link(name = "avrt")]
unsafe extern "system" {
    fn AvSetMmThreadCharacteristicsW(task: *const u16, index: *mut u32) -> *mut c_void;
    fn AvRevertMmThreadCharacteristics(handle: *mut c_void) -> i32;
}

impl ThreadPriority {
    pub(super) fn capture() -> Self {
        let old = unsafe { GetThreadPriority(GetCurrentThread()) };
        let mut index = 0;
        let name: Vec<u16> = "Capture\0".encode_utf16().collect();
        let mmcss = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut index) };
        let raised = mmcss.is_null()
            && old != PRIORITY_ERROR
            && unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) } != 0;
        Self {
            mmcss,
            old,
            raised,
            _same_thread: PhantomData,
        }
    }

    pub(super) fn mode(&self) -> &'static str {
        if !self.mmcss.is_null() {
            "mmcss_capture"
        } else if self.raised {
            "thread_time_critical"
        } else {
            "unchanged"
        }
    }
}

impl Drop for ThreadPriority {
    fn drop(&mut self) {
        unsafe {
            if !self.mmcss.is_null() {
                AvRevertMmThreadCharacteristics(self.mmcss);
            } else if self.raised {
                SetThreadPriority(GetCurrentThread(), self.old);
            }
        }
    }
}
