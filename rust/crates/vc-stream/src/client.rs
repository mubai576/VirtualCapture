//! 版本化 C ABI 的自有加载器。会话一直保持其 DLL 处于加载状态，直到 join 完成。
use std::ffi::{CString, c_char, c_void};
use std::path::Path;
use std::ptr;
use std::sync::Arc;

use crate::abi::{ABI_VERSION, VcStreamApiV1};
use libloading::os::windows::{
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, Library,
};
use serde_json::Value;

const ERROR_CAPACITY: usize = 2048;
const TEXT_LIMIT: usize = 4 * 1024 * 1024;
type GetApi = unsafe extern "C" fn(u32) -> *const VcStreamApiV1;
struct LoadedModule {
    _library: Library,
    api: VcStreamApiV1,
}
#[derive(Clone)]
pub struct Module {
    loaded: Arc<LoadedModule>,
}

impl Module {
    pub fn load(path: &Path) -> Result<Self, String> {
        let path = std::fs::canonicalize(path)
            .map_err(|e| format!("data-plane DLL {}: {e}", path.display()))?;
        let load_path = vc_media::load_library_path(&path);
        let library = unsafe {
            Library::load_with_flags(
                load_path.as_ref(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
        .map_err(|e| format!("load data-plane DLL {}: {e}", path.display()))?;
        let get_api = unsafe { library.get::<GetApi>(b"vc_module_get_api\0") }
            .map(|symbol| *symbol)
            .map_err(|e| format!("{} has no vc_module_get_api: {e}", path.display()))?;
        let pointer = unsafe { get_api(ABI_VERSION) };
        if pointer.is_null() {
            return Err(format!(
                "{} does not implement data-plane ABI v{ABI_VERSION}",
                path.display()
            ));
        }
        // 只读取强制的前两个 u32 前缀，之后才拷贝任何函数指针。
        let prefix = pointer.cast::<u32>();
        let (version, size) = unsafe { (ptr::read(prefix), ptr::read(prefix.add(1))) };
        if version != ABI_VERSION || size < std::mem::size_of::<VcStreamApiV1>() as u32 {
            return Err(format!(
                "{} returned incompatible data-plane ABI version/size",
                path.display()
            ));
        }
        let api = unsafe { ptr::read(pointer) };
        Ok(Self {
            loaded: Arc::new(LoadedModule {
                _library: library,
                api,
            }),
        })
    }
    pub fn list_adapters(&self) -> Result<Vec<(String, String)>, String> {
        let text = read_text(|out, cap, written, err, err_cap, err_written| unsafe {
            (self.loaded.api.list_adapters)(out, cap, written, err, err_cap, err_written)
        })?;
        serde_json::from_str(&text).map_err(|e| format!("invalid DLL adapter list: {e}"))
    }
    pub fn capabilities(&self) -> Result<Value, String> {
        let text = read_text(|out, cap, written, err, err_cap, err_written| unsafe {
            (self.loaded.api.capabilities)(out, cap, written, err, err_cap, err_written)
        })?;
        serde_json::from_str(&text).map_err(|e| format!("invalid DLL capabilities: {e}"))
    }
    pub fn start_sender(&self, request: &Value) -> Result<StreamSession, String> {
        self.start(1, request)
    }
    pub fn start_receiver(&self, request: &Value) -> Result<StreamSession, String> {
        self.start(2, request)
    }
    fn start(&self, role: u32, request: &Value) -> Result<StreamSession, String> {
        let request = CString::new(serde_json::to_string(request).map_err(|e| e.to_string())?)
            .map_err(|_| "request contains NUL")?;
        let mut handle = ptr::null_mut();
        let mut error = [0i8; ERROR_CAPACITY];
        let mut written = 0;
        let code = unsafe {
            (self.loaded.api.start_session)(
                role,
                request.as_ptr(),
                &mut handle,
                error.as_mut_ptr(),
                ERROR_CAPACITY as u32,
                &mut written,
            )
        };
        if code != 0 {
            return Err(error_text(&error, "start session failed"));
        }
        if handle.is_null() {
            return Err("DLL returned a null session handle".into());
        }
        Ok(StreamSession {
            loaded: self.loaded.clone(),
            handle,
        })
    }
}

pub struct StreamSession {
    loaded: Arc<LoadedModule>,
    handle: *mut c_void,
}
// 句柄由持有它的控制器串行化；没有任何 Rust 引用跨越 DLL 边界。
unsafe impl Send for StreamSession {}
impl StreamSession {
    pub fn stats(&self) -> Result<Value, String> {
        let text = read_text(|out, cap, written, err, err_cap, err_written| unsafe {
            (self.loaded.api.session_status)(
                self.handle,
                out,
                cap,
                written,
                err,
                err_cap,
                err_written,
            )
        })?;
        serde_json::from_str(&text).map_err(|e| format!("invalid DLL status JSON: {e}"))
    }
    /// 请求取消。返回 false 表示 worker 仍持有其资源。
    pub fn stop(&mut self) -> Result<bool, String> {
        let mut error = [0i8; ERROR_CAPACITY];
        let mut written = 0;
        let code = unsafe {
            (self.loaded.api.stop_session)(
                self.handle,
                error.as_mut_ptr(),
                ERROR_CAPACITY as u32,
                &mut written,
            )
        };
        match code {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(error_text(&error, "stop session failed")),
        }
    }
}
impl Drop for StreamSession {
    fn drop(&mut self) {
        let handle = self.handle;
        match classify_destroy(unsafe { (self.loaded.api.destroy_session)(handle) }) {
            DestroyOutcome::Released => return,
            // destroy_session 自身 panic：句柄状态未知，继续调用不安全；有意保留
            // DLL 映射与句柄直到进程退出。
            DestroyOutcome::Unknown => {
                std::mem::forget(self.loaded.clone());
                return;
            }
            DestroyOutcome::Pending => {}
        }
        // 正常关闭会保留句柄，直到 stop 确认清理完成。如果调用方中途放弃，
        // 只要驱动调用或 worker 尚未结束，就继续保留 DLL 映像。
        let address = handle as usize;
        let loaded = self.loaded.clone();
        if std::thread::Builder::new()
            .name("vc-stream-reaper".into())
            .spawn(move || reap_until_released(loaded, address))
            .is_err()
        {
            // 原生调用若已挂起，没有安全的同步兜底方案。
            std::mem::forget(self.loaded.clone());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DestroyOutcome {
    /// 工作线程已 join，句柄已释放。
    Released,
    /// 工作线程仍在收尾，句柄仍有效，调用方应稍后重试。
    Pending,
    /// `destroy_session` 自身 panic（-1）：句柄状态未知，不得再调用。
    Unknown,
}

fn classify_destroy(code: i32) -> DestroyOutcome {
    match code {
        0 => DestroyOutcome::Released,
        1 => DestroyOutcome::Pending,
        _ => DestroyOutcome::Unknown,
    }
}

fn reap_until_released(loaded: Arc<LoadedModule>, address: usize) {
    loop {
        match classify_destroy(unsafe { (loaded.api.destroy_session)(address as *mut c_void) }) {
            DestroyOutcome::Released => return,
            DestroyOutcome::Pending => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            DestroyOutcome::Unknown => {
                // 不再调用可能已损坏的句柄，保留 DLL 到进程退出。
                std::mem::forget(loaded);
                return;
            }
        }
    }
}

fn read_text(
    call: impl Fn(*mut c_char, u32, *mut u32, *mut c_char, u32, *mut u32) -> i32,
) -> Result<String, String> {
    let mut required = 0;
    let mut error = [0i8; ERROR_CAPACITY];
    let mut error_written = 0;
    let first = call(
        ptr::null_mut(),
        0,
        &mut required,
        error.as_mut_ptr(),
        ERROR_CAPACITY as u32,
        &mut error_written,
    );
    if first < 0 {
        return Err(error_text(&error, "data-plane query failed"));
    }
    // 运行期间状态会变化；尊重缓冲区过小的返回值，并在有限次数内重试。
    for _ in 0..8 {
        let capacity = (required as usize).saturating_add(1024).min(TEXT_LIMIT);
        if required == 0 || required as usize > TEXT_LIMIT {
            return Err("DLL text result length is invalid".into());
        }
        let mut output = vec![0i8; capacity];
        let mut written = 0;
        let code = call(
            output.as_mut_ptr(),
            capacity as u32,
            &mut written,
            error.as_mut_ptr(),
            ERROR_CAPACITY as u32,
            &mut error_written,
        );
        if code == 1 {
            required = written;
            continue;
        }
        if code < 0 {
            return Err(error_text(&error, "data-plane query failed"));
        }
        if code != 0 || written == 0 || written as usize > capacity {
            return Err("DLL returned an invalid text size".into());
        }
        let bytes: Vec<u8> = output[..written as usize]
            .iter()
            .map(|v| *v as u8)
            .collect();
        let text = std::ffi::CStr::from_bytes_with_nul(&bytes)
            .map_err(|_| "DLL returned invalid NUL-terminated text")?;
        return text
            .to_str()
            .map(str::to_owned)
            .map_err(|e| format!("DLL result is not UTF-8: {e}"));
    }
    Err("DLL status changed too quickly to read".into())
}
fn error_text(buffer: &[c_char], fallback: &str) -> String {
    let bytes: Vec<u8> = buffer
        .iter()
        .take_while(|v| **v != 0)
        .map(|v| *v as u8)
        .collect();
    if bytes.is_empty() {
        fallback.into()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{DestroyOutcome, classify_destroy};

    #[test]
    fn destroy_codes_distinguish_pending_from_panic() {
        assert_eq!(classify_destroy(0), DestroyOutcome::Released);
        assert_eq!(classify_destroy(1), DestroyOutcome::Pending);
        // -1 是 destroy_session 自身 panic，不能按 pending 无限重试。
        assert_eq!(classify_destroy(-1), DestroyOutcome::Unknown);
        assert_eq!(classify_destroy(2), DestroyOutcome::Unknown);
    }
}
