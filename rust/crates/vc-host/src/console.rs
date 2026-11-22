//! GUI 子系统中的 CLI 附着父控制台；不为 GUI、探测和后台新建控制台。

#[cfg(windows)]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
#[cfg(windows)]
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetConsoleCP, GetStdHandle, STD_ERROR_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler, SetStdHandle,
};

#[cfg(windows)]
pub(crate) fn attach() -> bool {
    // 安全：仅在入口单线程执行。保留原有重定向句柄，AttachConsole 可补齐终端句柄。
    unsafe {
        if GetConsoleCP() != 0 {
            return true;
        }
        let original = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .map(|id| (id, GetStdHandle(id)));
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return false;
        }
        for (id, handle) in original {
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetStdHandle(id, handle);
            }
        }
        true
    }
}

#[cfg(not(windows))]
pub(crate) fn attach() -> bool {
    true
}

pub(crate) fn run_child(
    mut command: std::process::Command,
    has_console: bool,
) -> std::io::Result<i32> {
    // 先安装引导进程的 handler；实际采集进程负责处理同一控制台的 Ctrl-C/Break。
    #[cfg(windows)]
    if has_console {
        use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
        unsafe extern "system" fn ignore_ctrl(event: u32) -> i32 {
            i32::from(matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT))
        }
        // 安全：handler 不访问外部状态，引导进程在等待结束后即退出。
        if unsafe { SetConsoleCtrlHandler(Some(ignore_ctrl), 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(windows))]
    let _ = has_console;
    Ok(command.spawn()?.wait()?.code().unwrap_or(1))
}

pub(crate) fn redirect_if_missing(stderr: bool, file: std::fs::File) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::IntoRawHandle;
        let id = if stderr {
            STD_ERROR_HANDLE
        } else {
            STD_OUTPUT_HANDLE
        };
        // 安全：仅在入口执行。将文件句柄交给本进程的标准流，存活至退出。
        unsafe {
            let old = GetStdHandle(id);
            if old.is_null() || old == INVALID_HANDLE_VALUE {
                use std::os::windows::io::AsRawHandle;
                if SetStdHandle(id, file.as_raw_handle()) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let _ = file.into_raw_handle();
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (stderr, file);
    Ok(())
}

#[cfg(windows)]
pub(crate) fn has_stderr() -> bool {
    // 安全：只查询本进程标准错误句柄，不转移所有权。
    let handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

#[cfg(not(windows))]
pub(crate) fn has_stderr() -> bool {
    true
}
