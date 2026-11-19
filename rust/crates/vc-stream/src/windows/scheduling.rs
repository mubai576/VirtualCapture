//! 线程级采集调度，供发送端、接收端与解码器共用。
//!
//! 该 guard 必须留在注册它的线程上，并在每条退出路径上恢复线程调度状态。
//! 加载数据面 DLL 绝不会提升宿主进程优先级。
use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentThread,
    GetThreadPriority, SetThreadPriority, SetWaitableTimer, THREAD_PRIORITY_TIME_CRITICAL,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};

/// 每个发送线程一个私有定时器：不改变全局定时器分辨率，也不改变进程优先级。
/// 未命名自动重置定时器不会干扰其他 DLL 会话。调用方负责给等待设界，并保留
/// 一小段 QPC 精度尾部。
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
        // 失败只影响唤醒精度；外层 QPC 检查仍会强制执行精确截止时间，且
        // 绝不提前发送。
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

// GetThreadPriority 失败时返回 MAXLONG（THREAD_PRIORITY_ERROR_RETURN）。
const PRIORITY_ERROR: i32 = i32::MAX;

pub(super) struct ThreadPriority {
    mmcss: *mut c_void,
    old: i32,
    raised: bool,
    // MMCSS 注册属于调用线程；即使本结构体的原生字段以后改成实现了
    // Send/Sync 的类型，也不能让它跨线程。
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
