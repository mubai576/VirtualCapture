//! 运行库定位与安全加载。
//!
//! 产品运行目录约定：编解码 DLL 与模块/可执行文件同目录（当前只有 PyroWave
//! 的 `libpyrowave-shared-1.dll`；MJPEG/TurboJPEG 已移除）。默认从"当前模块
//! 所在目录"解析绝对路径；测试或
//! 嵌宿主可用显式 [`DllDirectory::new`]。加载使用 `LoadLibraryExW` 的安全搜索
//! 标志，只允许依赖目录、System32 与默认目录，不接受相对路径/当前目录注入。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use libloading::os::windows::{
    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LOAD_LIBRARY_SEARCH_SYSTEM32, Library,
};

const SEARCH_FLAGS: u32 = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
    | LOAD_LIBRARY_SEARCH_SYSTEM32
    | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;

/// 已验证的绝对 DLL 依赖目录。
#[derive(Clone, Debug)]
pub struct DllDirectory {
    path: PathBuf,
}

/// 取本模块自身路径，而不是宿主进程的 `current_exe`。
///
/// `GetModuleHandleExW(FROM_ADDRESS)` 用本模块内静态变量的地址反查模块：
/// 静态链接时返回宿主 EXE，作为版本化 DLL 加载时返回 DLL 自身。
fn current_module_path() -> Result<PathBuf> {
    use std::ffi::{OsString, c_void};
    use std::os::windows::ffi::OsStringExt;

    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x0000_0002;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleExW(flags: u32, module_name: *const u16, module: *mut *mut c_void)
        -> i32;
        fn GetModuleFileNameW(module: *mut c_void, filename: *mut u16, size: u32) -> u32;
    }

    static ANCHOR: u8 = 0;
    let mut module: *mut c_void = std::ptr::null_mut();
    // SAFETY: ANCHOR 的地址必然落在本模块映像内；UNCHANGED_REFCOUNT 不增加引用计数。
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            (&ANCHOR as *const u8).cast::<u16>(),
            &mut module,
        )
    };
    ensure!(
        ok != 0 && !module.is_null(),
        "GetModuleHandleExW 无法定位当前模块"
    );
    let mut buffer = vec![0u16; 32 * 1024];
    // SAFETY: 缓冲区长度以 u32 传入；模块句柄来自上一步。
    let len = unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) };
    ensure!(
        len > 0 && (len as usize) < buffer.len(),
        "GetModuleFileNameW 无法读取当前模块路径（截断或失败）"
    );
    Ok(PathBuf::from(OsString::from_wide(&buffer[..len as usize])))
}

impl DllDirectory {
    /// 只接受已存在的绝对目录；保存规范化后的路径。
    pub fn new(directory: &Path) -> Result<Self> {
        ensure!(
            directory.is_absolute(),
            "编解码依赖目录必须是绝对路径（当前 {}）",
            directory.display()
        );
        let path = directory
            .canonicalize()
            .with_context(|| format!("编解码依赖目录不可访问：{}", directory.display()))?;
        ensure!(path.is_dir(), "编解码依赖目录不是目录：{}", path.display());
        Ok(Self { path })
    }

    /// 当前模块（静态链接时即宿主 EXE，动态加载时即本 crate 所在 DLL）的目录。
    pub fn default_for_current_module() -> Result<Self> {
        let module = current_module_path()
            .or_else(|_| std::env::current_exe().context("无法确定当前模块路径"))?;
        let directory = module
            .parent()
            .context("当前模块没有父目录，无法定位编解码依赖")?;
        Self::new(directory)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 按候选文件名加载；文件必须真实存在且不越出依赖目录。
    pub fn load(&self, candidates: &[&str], label: &str) -> Result<Library> {
        let mut tried = Vec::with_capacity(candidates.len());
        for name in candidates {
            let path = self.path.join(name);
            if !path.is_file() {
                tried.push(format!("{}（不存在）", path.display()));
                continue;
            }
            let resolved = path
                .canonicalize()
                .with_context(|| format!("无法解析 {label} 路径：{}", path.display()))?;
            ensure!(
                resolved.parent() == Some(self.path.as_path()),
                "{label} 解析后越出依赖目录：{}",
                resolved.display()
            );
            // SAFETY: 路径来自已验证的绝对目录；搜索标志限制依赖解析范围。
            let load_path = crate::load_library_path(&resolved);
            let library = unsafe { Library::load_with_flags(load_path.as_ref(), SEARCH_FLAGS) }
                .with_context(|| format!("加载 {label} 失败：{}", resolved.display()))?;
            return Ok(library);
        }
        bail!(
            "依赖目录 {} 中找不到 {label}；候选：{}。请把运行库放在模块/可执行文件同目录。",
            self.path.display(),
            tried.join("，")
        )
    }
}
