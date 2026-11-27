//! 原生库加载前的路径拼写修正。

use std::borrow::Cow;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

/// `LoadLibraryExW` 的 `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR` 不接受
/// `\\?\UNC\server\share` 形式的 verbatim 网络路径，Windows 会返回
/// ERROR_INVALID_PARAMETER。`canonicalize` 会把共享路径改写成该形式；
/// 加载前还原成等价的 `\\server\share` 拼写，其他路径保持原样。
pub fn load_library_path(path: &Path) -> Cow<'_, Path> {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::path::{Component, Prefix};

        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Cow::Borrowed(path);
        };
        let (server, share) = match prefix.kind() {
            Prefix::VerbatimUNC(server, share) => (server, share),
            Prefix::Verbatim(name) if name.eq_ignore_ascii_case("unc") => {
                // std 只把大写 UNC 识别为 VerbatimUNC；小写/混合拼写在 Windows
                // 仍是同一 UNC 命名空间，需自行取后面的 server 与 share。
                if !matches!(components.next(), Some(Component::RootDir)) {
                    return Cow::Borrowed(path);
                }
                let (Some(Component::Normal(server)), Some(Component::Normal(share))) =
                    (components.next(), components.next())
                else {
                    return Cow::Borrowed(path);
                };
                (server, share)
            }
            _ => return Cow::Borrowed(path),
        };
        let mut plain = OsString::from(r"\\");
        plain.push(server);
        plain.push(r"\");
        plain.push(share);
        for component in components {
            // 普通 UNC 拼写由这里补分隔符；单独的根组件不再重复写入。
            if matches!(component, Component::RootDir) {
                continue;
            }
            plain.push(r"\");
            plain.push(component.as_os_str());
        }
        Cow::Owned(PathBuf::from(plain))
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_and_plain_network_paths_are_unchanged() {
        for path in [
            Path::new(r"D:\Share\VirtualCapture\vc_stream.dll"),
            Path::new(r"\\?\D:\Share\VirtualCapture\vc_stream.dll"),
            Path::new(r"\\server\share\VirtualCapture\vc_stream.dll"),
        ] {
            assert_eq!(load_library_path(path).as_ref(), path);
        }
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_unc_is_rewritten_to_plain_unc() {
        for path in [
            Path::new(r"\\?\UNC\server\share\VirtualCapture\vc_stream.dll"),
            Path::new(r"\\?\unc\server\share\VirtualCapture\vc_stream.dll"),
            Path::new(r"\\?\Unc\server\share\VirtualCapture\vc_stream.dll"),
        ] {
            assert_eq!(
                load_library_path(path).as_ref(),
                Path::new(r"\\server\share\VirtualCapture\vc_stream.dll")
            );
        }
    }
}
