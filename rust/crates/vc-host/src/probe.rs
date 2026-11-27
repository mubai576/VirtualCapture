//! 仅在隔离子进程中读取实际虚拟卡 DLL 的能力导出。

use std::path::Path;

#[cfg(windows)]
pub(crate) fn run(dll: &Path) -> anyhow::Result<()> {
    use std::ffi::{CStr, c_char};
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        LoadLibraryExW,
    };

    struct Module(HMODULE);
    impl Drop for Module {
        fn drop(&mut self) {
            // 安全：只释放本进程由 LoadLibraryExW 获得的一枚引用。
            unsafe { FreeLibrary(self.0) };
        }
    }

    let dll = std::fs::canonicalize(dll)?;
    let load_path = vc_media::load_library_path(&dll);
    let wide: Vec<_> = load_path.as_os_str().encode_wide().chain(Some(0)).collect();
    // 安全：传入绝对路径的 NUL 结尾 UTF-16 缓冲区。不搜索当前工作目录。
    let handle = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
    };
    if handle.is_null() {
        anyhow::bail!(
            "无法加载 {}：{}",
            dll.display(),
            std::io::Error::last_os_error()
        );
    }
    let module = Module(handle);
    // 安全：符号名与 VCStreamCapabilities 的固定 C ABI 对应；DLL 在读取期间保持加载。
    // DLL 失常只会影响这个有界子进程，不在 GUI 内调用导出。
    let get = unsafe { GetProcAddress(module.0, c"VCStreamCapabilities".as_ptr().cast()) }
        .ok_or_else(|| anyhow::anyhow!("DLL 缺少 VCStreamCapabilities 导出"))?;
    let get: unsafe extern "C" fn() -> *const c_char = unsafe { std::mem::transmute(get) };
    let text = unsafe { get() };
    anyhow::ensure!(!text.is_null(), "虚拟卡能力导出返回空指针");
    println!("{}", unsafe { CStr::from_ptr(text) }.to_str()?);
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn run(_: &Path) -> anyhow::Result<()> {
    anyhow::bail!("虚拟卡能力探测仅支持 Windows")
}
