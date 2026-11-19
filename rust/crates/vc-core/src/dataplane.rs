//! 数据面模块：产品只通过版本化 `vc_stream.dll` ABI 启动采集/接收与枚举网卡。
//!
//! 不存在旧 `vc-sender.exe`/`vc-receiver.exe` 回退，也不会跨发布目录混用模块：
//! [`load`] 按运行布局解析唯一 DLL，`Module` 的加载器校验 ABI 版本/结构，
//! `process::require_capabilities` 再校验能力。非 Windows 目标只提供同形占位，
//! 让控制面仍可编译并明确拒绝。

#[cfg(windows)]
pub use vc_stream::client::{Module, StreamSession};

/// 按当前运行布局装载数据面模块。文件不存在时直接拒绝，不搜索旧程序。
pub fn load() -> Result<Module, String> {
    let path = crate::paths::data_plane_dll();
    if !path.is_file() {
        return Err(format!(
            "找不到数据面模块 vc_stream.dll：{}；请构建并部署 Rust vc-stream（不回退旧 exe）。",
            path.display()
        ));
    }
    Module::load(&path)
}

#[cfg(not(windows))]
mod stub {
    use serde_json::Value;
    use std::path::Path;

    #[derive(Clone, Debug)]
    pub struct Module;

    #[derive(Debug)]
    pub struct StreamSession;

    impl Module {
        pub fn load(path: &Path) -> Result<Self, String> {
            Err(format!("数据面模块仅在 Windows 可用：{}", path.display()))
        }

        pub fn list_adapters(&self) -> Result<Vec<(String, String)>, String> {
            Err("数据面模块仅在 Windows 可用。".into())
        }

        pub fn capabilities(&self) -> Result<Value, String> {
            Err("数据面模块仅在 Windows 可用。".into())
        }

        pub fn start_sender(&self, _request: &Value) -> Result<StreamSession, String> {
            Err("数据面模块仅在 Windows 可用。".into())
        }

        pub fn start_receiver(&self, _request: &Value) -> Result<StreamSession, String> {
            Err("数据面模块仅在 Windows 可用。".into())
        }
    }

    impl StreamSession {
        pub fn stats(&self) -> Result<Value, String> {
            Err("数据面模块仅在 Windows 可用。".into())
        }

        pub fn stop(&mut self) -> Result<bool, String> {
            Err("数据面模块仅在 Windows 可用。".into())
        }
    }
}

#[cfg(not(windows))]
pub use stub::{Module, StreamSession};
