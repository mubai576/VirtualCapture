use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static ROOT_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();
static NATIVE_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// CLI `--root` 覆盖（优先于环境变量与自动探测）。
pub fn set_root_override(path: impl Into<PathBuf>) {
    let _ = ROOT_OVERRIDE.set(path.into());
}

/// 数据面/虚拟卡 DLL 目录覆盖（开发时可指向构建输出，不回退旧 exe）。
pub fn set_native_dir_override(path: impl Into<PathBuf>) {
    let _ = NATIVE_DIR_OVERRIDE.set(path.into());
}

/// 运行根目录。优先级：`--root` → `VC_ROOT` → 从 exe 向上找仓库标记 → exe 所在目录。
pub fn root() -> PathBuf {
    if let Some(path) = ROOT_OVERRIDE.get() {
        return path.clone();
    }
    if let Ok(value) = std::env::var("VC_ROOT") {
        let path = PathBuf::from(value);
        if path.is_dir() {
            return path;
        }
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let start = dir.clone();
    loop {
        if dir.join("version.json").exists() || dir.join("AGENTS.md").exists() {
            return dir;
        }
        if !dir.pop() {
            return start;
        }
    }
}

/// 运行数据布局。
///
/// * `Managed`：存在 `ROOT/app/current.json`（版本化发布布局）。程序在
///   `app/releases/<build_id>/`，配置与证据在 `data/`。
/// * `Legacy`：现用扁平便携目录与开发根目录，保持原有 `ROOT/配置`、`ROOT/run/`。
///
/// 只有真正安装过版本化发布（或激活过一次 OTA）才会进入 `Managed`，
/// 因此现有测试、扁平包与开发目录行为不变。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Managed,
    Legacy,
}

pub const CONFIG_FILE_NAME: &str = "vc-rust-config.json";
pub const UPDATE_TOKEN_FILE_NAME: &str = "update-token.json";
pub const PEER_UPDATE_TOKEN_FILE_NAME: &str = "update-peer-token.json";
pub const APP_DIR: &str = "app";
pub const CURRENT_FILE_NAME: &str = "current.json";
pub const DATA_DIR: &str = "data";
pub const UPDATES_DIR: &str = "updates";

pub fn layout_at(root: &Path) -> Layout {
    if root.join(APP_DIR).join(CURRENT_FILE_NAME).is_file() {
        Layout::Managed
    } else {
        Layout::Legacy
    }
}

pub fn layout() -> Layout {
    layout_at(&root())
}

pub fn app_dir() -> PathBuf {
    root().join(APP_DIR)
}

pub fn releases_dir() -> PathBuf {
    app_dir().join("releases")
}

pub fn current_pointer_path() -> PathBuf {
    app_dir().join(CURRENT_FILE_NAME)
}

pub fn data_dir() -> PathBuf {
    root().join(DATA_DIR)
}

pub fn updates_dir() -> PathBuf {
    root().join(UPDATES_DIR)
}

pub fn staged_root() -> PathBuf {
    updates_dir().join("staged")
}

pub fn staged_dir(build_id: &str) -> PathBuf {
    staged_root().join(build_id)
}

pub fn staged_meta_path(build_id: &str) -> PathBuf {
    staged_root().join(format!("{build_id}.json"))
}

pub fn worker_dir() -> PathBuf {
    updates_dir().join("worker")
}

pub fn incoming_dir() -> PathBuf {
    updates_dir().join("incoming")
}

pub fn history_path() -> PathBuf {
    updates_dir().join("history.jsonl")
}

pub fn run_dir() -> PathBuf {
    match layout() {
        Layout::Managed => data_dir().join("run"),
        Layout::Legacy => root().join("run"),
    }
}

/// 更新令牌只保护 `/update/*`。Managed 放在 `data/`，Legacy 保持根目录。
pub fn update_token_path() -> PathBuf {
    update_token_path_at(&root())
}

pub fn update_token_path_at(root: &Path) -> PathBuf {
    match layout_at(root) {
        Layout::Managed => root.join(DATA_DIR).join(UPDATE_TOKEN_FILE_NAME),
        Layout::Legacy => root.join(UPDATE_TOKEN_FILE_NAME),
    }
}

/// 主机保存“被采集机更新令牌”的独立文件；不写入配置、不记录日志。
pub fn peer_update_token_path() -> PathBuf {
    peer_update_token_path_at(&root())
}

pub fn peer_update_token_path_at(root: &Path) -> PathBuf {
    match layout_at(root) {
        Layout::Managed => root.join(DATA_DIR).join(PEER_UPDATE_TOKEN_FILE_NAME),
        Layout::Legacy => root.join(PEER_UPDATE_TOKEN_FILE_NAME),
    }
}

/// Rust 控制面独立配置文件名。旧 Python agent 的 `vc-config.json` 字段不完整
/// （含 `role`/`gop_frames` 等），不得由 Rust 程序覆盖或复用。
pub fn config_path() -> PathBuf {
    config_path_at(&root())
}

pub fn config_path_at(root: &Path) -> PathBuf {
    match layout_at(root) {
        Layout::Managed => {
            let managed = root.join(DATA_DIR).join(CONFIG_FILE_NAME);
            if managed.exists() {
                return managed;
            }
            // 首次激活后的过渡期：读取旧扁平配置，避免把用户设置重置为默认值。
            let legacy = root.join(CONFIG_FILE_NAME);
            if legacy.exists() {
                return legacy;
            }
            managed
        }
        Layout::Legacy => root.join(CONFIG_FILE_NAME),
    }
}

/// 已激活版本的 build id（`app/current.json`）；没有版本化发布时为 None。
pub fn current_build_id() -> Option<String> {
    current_build_id_at(&root())
}

/// build_id 直接作为目录名：除字符集外，拒绝 `.`/`..`、结尾点与 Windows 保留名。
pub fn valid_build_id(id: &str) -> bool {
    if id.is_empty()
        || id.len() > 64
        || id == "."
        || id == ".."
        || id.ends_with('.')
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return false;
    }
    // 目录名的设备名判定：取第一个点之前的部分，忽略大小写。
    let stem = id.split('.').next().unwrap_or(id).to_ascii_uppercase();
    !(matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()))
}

pub fn current_build_id_at(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(APP_DIR).join(CURRENT_FILE_NAME)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("build_id")?
        .as_str()
        .filter(|id| valid_build_id(id))
        .map(str::to_owned)
}

fn is_plain_file(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return false; // reparse point（junction/符号链接）
        }
    }
    metadata.is_file()
}

/// 启动前的版本指针体检：无指针视为扁平布局；有指针则必须是普通文件、
/// schema/build_id 有效，且对应 release 真实位于 root 下。供 capture/host CLI 在
/// 设置 root/native 后调用，避免损坏指针静默退回扁平布局。
pub fn validate_current_at(root: &Path) -> Result<(), String> {
    let path = root.join(APP_DIR).join(CURRENT_FILE_NAME);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("无法读取版本指针 {}：{error}", path.display())),
    };
    if !is_plain_file(&metadata) {
        return Err(format!(
            "版本指针 {} 不是普通文件（符号链接/重解析点/目录）；拒绝启动。",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("无法读取版本指针 {}：{error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("版本指针 {} 不是有效 JSON：{error}", path.display()))?;
    if value.get("schema").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(format!("版本指针 {} 的 schema 无效。", path.display()));
    }
    let build = value
        .get("build_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| valid_build_id(id))
        .ok_or_else(|| format!("版本指针 {} 缺少有效 build_id。", path.display()))?;
    let release = root.join(APP_DIR).join("releases").join(build);
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|e| format!("无法解析根目录 {}：{e}", root.display()))?;
    let canonical_release = std::fs::canonicalize(&release)
        .map_err(|e| format!("当前版本 {} 的目录不存在：{e}", release.display()))?;
    if !canonical_release.starts_with(&canonical_root) {
        return Err(format!(
            "当前版本目录 {} 超出根目录 {}；拒绝启动。",
            canonical_release.display(),
            canonical_root.display()
        ));
    }
    if !canonical_release.is_dir() {
        return Err(format!(
            "当前版本 {} 不是目录。",
            canonical_release.display()
        ));
    }
    Ok(())
}

pub fn active_release_dir() -> Option<PathBuf> {
    active_release_dir_at(&root())
}

pub fn active_release_dir_at(root: &Path) -> Option<PathBuf> {
    if layout_at(root) != Layout::Managed {
        return None;
    }
    let id = current_build_id_at(root)?;
    let dir = root.join(APP_DIR).join("releases").join(id);
    dir.is_dir().then_some(dir)
}

/// 把旧扁平配置复制到 `data/` 作为升级过渡；原文件保留不动，不覆盖已有目标。
/// 返回是否实际复制。
pub fn migrate_legacy_config(root: &Path) -> std::io::Result<bool> {
    if layout_at(root) != Layout::Managed {
        return Ok(false);
    }
    let legacy = root.join(CONFIG_FILE_NAME);
    let managed = root.join(DATA_DIR).join(CONFIG_FILE_NAME);
    if managed.exists() || !legacy.is_file() {
        return Ok(false);
    }
    std::fs::create_dir_all(root.join(DATA_DIR))?;
    let bytes = std::fs::read(&legacy)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&managed)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(true)
}

/// 数据面/虚拟卡 DLL 查找顺序：显式目录 → Managed 当前 release → build → ROOT → exe 目录。
///
/// Managed 布局下优先使用 `app/current.json` 指向的 release，使仍在运行的旧主机进程
/// 在激活后也能装载新版本的 `vc_stream.dll`/`vc_vcam.dll`。
pub fn native_exe(name: &str) -> PathBuf {
    if let Some(dir) = NATIVE_DIR_OVERRIDE.get() {
        return dir.join(name);
    }
    if let Ok(dir) = std::env::var("VC_NATIVE_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir).join(name);
    }
    if let Some(release) = active_release_dir() {
        let candidate = release.join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    let root = root();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.clone());
    let candidates = [
        root.join("build").join(name),
        root.join("build").join("Release").join(name),
        root.join("build").join("Debug").join(name),
        root.join(name),
        exe_dir.join(name),
    ];
    candidates
        .into_iter()
        .find(|path| path.exists())
        .unwrap_or_else(|| root.join("build").join(name))
}

pub fn data_plane_dll() -> PathBuf {
    native_exe("vc_stream.dll")
}

pub fn vcam_dll() -> PathBuf {
    native_exe("vc_vcam.dll")
}

/// 运行 DLL 与主程序同目录；开发布局在 build，便携包在 ROOT。
pub fn runtime_dll(name: &str) -> PathBuf {
    native_exe(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_dll_names_are_versioned_artifacts() {
        assert_eq!(
            data_plane_dll().file_name().and_then(|name| name.to_str()),
            Some("vc_stream.dll")
        );
        assert_eq!(
            vcam_dll().file_name().and_then(|name| name.to_str()),
            Some("vc_vcam.dll")
        );
    }

    #[test]
    fn rust_config_uses_isolated_file_name() {
        let path = config_path();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(CONFIG_FILE_NAME)
        );
        assert_eq!(CONFIG_FILE_NAME, "vc-rust-config.json");
    }

    #[test]
    fn run_dir_is_under_root() {
        assert_eq!(run_dir(), root().join("run"));
    }

    #[test]
    fn layout_is_managed_only_with_current_pointer() {
        let dir = std::env::temp_dir().join(format!(
            "vc-layout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(layout_at(&dir), Layout::Legacy);
        assert_eq!(
            config_path_at(&dir),
            dir.join(CONFIG_FILE_NAME),
            "扁平目录必须保持原配置位置"
        );
        std::fs::create_dir_all(dir.join(APP_DIR)).unwrap();
        std::fs::write(
            dir.join(APP_DIR).join(CURRENT_FILE_NAME),
            r#"{"schema":1,"build_id":"b1"}"#,
        )
        .unwrap();
        assert_eq!(layout_at(&dir), Layout::Managed);
        assert_eq!(
            config_path_at(&dir),
            dir.join(DATA_DIR).join(CONFIG_FILE_NAME)
        );
        assert_eq!(current_build_id_at(&dir).as_deref(), Some("b1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migration_copies_legacy_config_without_overwriting() {
        let dir = std::env::temp_dir().join(format!(
            "vc-migrate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(APP_DIR)).unwrap();
        std::fs::write(
            dir.join(APP_DIR).join(CURRENT_FILE_NAME),
            r#"{"schema":1,"build_id":"b1"}"#,
        )
        .unwrap();
        std::fs::write(dir.join(CONFIG_FILE_NAME), "legacy").unwrap();
        assert!(migrate_legacy_config(&dir).unwrap());
        let copied = std::fs::read_to_string(dir.join(DATA_DIR).join(CONFIG_FILE_NAME)).unwrap();
        assert_eq!(copied, "legacy");
        assert_eq!(
            std::fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap(),
            "legacy"
        );
        // 再次迁移不覆盖已存在的 data 配置。
        std::fs::write(dir.join(DATA_DIR).join(CONFIG_FILE_NAME), "newer").unwrap();
        assert!(!migrate_legacy_config(&dir).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join(DATA_DIR).join(CONFIG_FILE_NAME)).unwrap(),
            "newer"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn build_ids_reject_dot_and_device_names() {
        assert!(valid_build_id("20261114-124010-20e160"));
        assert!(valid_build_id("a.b_c"));
        for invalid in [
            ".", "..", "a.", "CON", "con.txt", "PRN", "AUX", "NUL", "COM1", "lpt9.x", "", "../b1",
            "a/b",
        ] {
            assert!(!valid_build_id(invalid), "{invalid:?} should be rejected");
        }
    }

    #[test]
    fn validate_current_rejects_broken_or_out_of_root_pointer() {
        let dir = std::env::temp_dir().join(format!(
            "vc-paths-current-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(APP_DIR).join("releases").join("b1")).unwrap();
        assert!(validate_current_at(&dir).is_ok());
        let current = dir.join(APP_DIR).join(CURRENT_FILE_NAME);
        std::fs::write(&current, r#"{"schema":1,"build_id":"b1"}"#).unwrap();
        assert!(validate_current_at(&dir).is_ok());
        std::fs::write(&current, r#"{"schema":2,"build_id":"b1"}"#).unwrap();
        assert!(validate_current_at(&dir).is_err());
        std::fs::write(&current, r#"{"schema":1,"build_id":"../b1"}"#).unwrap();
        assert!(validate_current_at(&dir).is_err());
        std::fs::write(&current, r#"{"schema":1,"build_id":".."}"#).unwrap();
        assert!(validate_current_at(&dir).is_err());
        std::fs::write(&current, r#"{"schema":1,"build_id":"."}"#).unwrap();
        assert!(validate_current_at(&dir).is_err());
        std::fs::write(&current, r#"{"schema":1,"build_id":"CON"}"#).unwrap();
        assert!(validate_current_at(&dir).is_err());
        std::fs::write(&current, r#"{"schema":1,"build_id":"missing"}"#).unwrap();
        assert!(validate_current_at(&dir).is_err());
        std::fs::write(&current, "not json").unwrap();
        assert!(validate_current_at(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
