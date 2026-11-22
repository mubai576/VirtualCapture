//! 桌面启动偏好：主机界面默认打开的角色。
//!
//! 偏好保存到独立的 `vc-startup.json`，与 [`crate::config::Config`] 及 REST
//! 配置补丁完全分离：控制面保存配置（包括旧版本程序）不会覆盖该角色。
//!
//! 读取语义：文件缺失表示“尚未选择”，返回 `Ok(None)`；JSON 损坏、字段缺失、
//! 未知 schema 或未知角色必须报错，不能静默回退默认角色；读取绝不写文件。
//! 写入语义：同目录唯一临时文件 + 刷盘 + 原子替换，失败时保留原文件。
//!
//! 优先读取 `ROOT/data/vc-startup.json`，不存在时回退到 `ROOT/vc-startup.json`。
//! Managed 布局永远写 `data/`；退回扁平布局后继续使用已有的 `data/` 偏好，
//! 防止回滚时加载到过时的角色。首次在扁平布局选择才写根目录。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::{self, Layout};

/// 扁平布局首次选择写根目录；Managed 与已有偏好沿用 `data/`。
pub const STARTUP_FILE_NAME: &str = "vc-startup.json";

/// 启动偏好结构版本。无法识别的版本不得被静默信任。
pub const STARTUP_SCHEMA: u32 = 1;

/// 桌面端启动角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesktopRole {
    /// 接收端（主机）界面。
    Receive,
    /// 采集端（被采集机）界面。
    Capture,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct StartupPreference {
    schema: u32,
    role: DesktopRole,
}

/// 当前运行根的偏好文件路径（读取用途；Managed 回退到旧位置时可能不同）。
pub fn path() -> PathBuf {
    path_at(&paths::root())
}

/// 读取当前运行根的偏好；缺失返回 `Ok(None)`。
pub fn load() -> std::io::Result<Option<DesktopRole>> {
    load_at(&paths::root())
}

/// 原子写入当前运行根的偏好。
pub fn save(role: DesktopRole) -> std::io::Result<()> {
    save_at(&paths::root(), role)
}

/// 偏好文件的实际读取位置。
///
/// 优先读取 `ROOT/data/vc-startup.json`，避免版本回滚后丢失已保存的角色；
/// 缺失时回退 `ROOT/vc-startup.json`。两处都不存在时返回当前布局的写入位置。
/// 本函数不写文件、不迁移文件。
pub fn path_at(root: &Path) -> PathBuf {
    let data = managed_path_at(root);
    if data.exists() {
        return data;
    }
    let legacy = legacy_path_at(root);
    if legacy.exists() || paths::layout_at(root) == Layout::Legacy {
        return legacy;
    }
    data
}

/// 读取偏好。文件缺失返回 `Ok(None)`；JSON 损坏/字段缺失/未知 schema/未知角色
/// 返回 `InvalidData`；其他 I/O 错误原样返回。不会创建、迁移或改写任何文件。
pub fn load_at(root: &Path) -> std::io::Result<Option<DesktopRole>> {
    let path = path_at(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let preference: StartupPreference = serde_json::from_str(&text).map_err(|error| {
        invalid_data(format!("启动偏好文件 {} 无法解析：{error}", path.display()))
    })?;
    if preference.schema != STARTUP_SCHEMA {
        return Err(invalid_data(format!(
            "启动偏好文件 {} 的 schema 为 {}，本程序只支持 schema {}；不会猜测未知字段的含义。",
            path.display(),
            preference.schema,
            STARTUP_SCHEMA
        )));
    }
    Ok(Some(preference.role))
}

/// 原子保存偏好。Managed 布局写 `ROOT/data/vc-startup.json`；已有 data 偏好
/// 的扁平布局也继续使用该文件；否则写入 `ROOT/vc-startup.json`。
pub fn save_at(root: &Path, role: DesktopRole) -> std::io::Result<()> {
    let path = write_path_at(root);
    let preference = StartupPreference {
        schema: STARTUP_SCHEMA,
        role,
    };
    let text = serde_json::to_string_pretty(&preference)
        .map_err(|error| invalid_data(format!("启动偏好无法序列化：{error}")))?;
    crate::config::write_text_atomic(&path, &text)
}

fn managed_path_at(root: &Path) -> PathBuf {
    root.join(paths::DATA_DIR).join(STARTUP_FILE_NAME)
}

fn legacy_path_at(root: &Path) -> PathBuf {
    root.join(STARTUP_FILE_NAME)
}

/// 写入位置：Managed 永远写 `data/`，不因旧位置文件而改变。
fn write_path_at(root: &Path) -> PathBuf {
    if paths::layout_at(root) == Layout::Managed || managed_path_at(root).exists() {
        managed_path_at(root)
    } else {
        legacy_path_at(root)
    }
}

fn invalid_data(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "vc-core-startup-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_managed(root: &Path) {
        fs::create_dir_all(root.join(paths::APP_DIR)).unwrap();
        fs::write(
            root.join(paths::APP_DIR).join(paths::CURRENT_FILE_NAME),
            r#"{"schema":1,"build_id":"b1"}"#,
        )
        .unwrap();
    }

    fn managed_startup(root: &Path) -> PathBuf {
        root.join(paths::DATA_DIR).join(STARTUP_FILE_NAME)
    }

    fn legacy_startup(root: &Path) -> PathBuf {
        root.join(STARTUP_FILE_NAME)
    }

    fn temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    fn role_in_file(path: &Path) -> Value {
        let text = fs::read_to_string(path).unwrap();
        serde_json::from_str::<Value>(&text).unwrap()["role"].clone()
    }

    #[test]
    fn missing_file_returns_none_without_writing() {
        let root = temp_root("missing");
        assert_eq!(path_at(&root), legacy_startup(&root));
        assert_eq!(load_at(&root).unwrap(), None);
        assert!(!legacy_startup(&root).exists());
        assert!(
            fs::read_dir(&root).unwrap().next().is_none(),
            "读取缺失文件不得创建任何文件"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_missing_preference_is_read_only() {
        let root = temp_root("managed-missing");
        make_managed(&root);
        assert_eq!(path_at(&root), managed_startup(&root));
        assert_eq!(load_at(&root).unwrap(), None);
        assert!(!root.join(paths::DATA_DIR).exists());
        assert!(!managed_startup(&root).exists());
        assert!(!legacy_startup(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn roles_roundtrip_and_replace() {
        let root = temp_root("roundtrip");
        assert_eq!(
            serde_json::to_string(&DesktopRole::Receive).unwrap(),
            r#""receive""#
        );
        assert_eq!(
            serde_json::to_string(&DesktopRole::Capture).unwrap(),
            r#""capture""#
        );

        save_at(&root, DesktopRole::Receive).unwrap();
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Receive));
        assert_eq!(role_in_file(&legacy_startup(&root)), "receive");
        let value: Value =
            serde_json::from_str(&fs::read_to_string(legacy_startup(&root)).unwrap()).unwrap();
        assert_eq!(value["schema"], 1);

        save_at(&root, DesktopRole::Capture).unwrap();
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Capture));
        assert_eq!(role_in_file(&legacy_startup(&root)), "capture");

        save_at(&root, DesktopRole::Receive).unwrap();
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Receive));
        assert!(temp_files(&root).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_or_unknown_startup_errors_without_rewrite() {
        let root = temp_root("corrupt");
        let path = legacy_startup(&root);
        for (label, bytes) in [
            ("malformed", "{ not json"),
            ("unknown-role", r#"{"schema":1,"role":"admin"}"#),
            ("role-case", r#"{"schema":1,"role":"Receive"}"#),
            ("missing-role", r#"{"schema":1}"#),
            ("missing-schema", r#"{"role":"capture"}"#),
            ("unknown-schema", r#"{"schema":2,"role":"capture"}"#),
            ("schema-type", r#"{"schema":"1","role":"capture"}"#),
        ] {
            fs::write(&path, bytes).unwrap();
            let error = load_at(&root).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{label}");
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                bytes,
                "{label} 读取不得改写文件"
            );
        }
        assert!(temp_files(&root).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_layout_falls_back_to_legacy_read_but_writes_data() {
        let root = temp_root("managed-fallback");
        make_managed(&root);
        let legacy = legacy_startup(&root);
        let legacy_bytes = r#"{"schema":1,"role":"receive"}"#;

        // 只有旧位置文件：可以读取，但不迁移、不创建 data 目录。
        fs::write(&legacy, legacy_bytes).unwrap();
        assert_eq!(path_at(&root), legacy);
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Receive));
        assert!(!root.join(paths::DATA_DIR).exists());

        // 写入始终落在 Managed 数据目录，旧文件保持原文。
        save_at(&root, DesktopRole::Capture).unwrap();
        assert!(managed_startup(&root).is_file());
        assert_eq!(path_at(&root), managed_startup(&root));
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Capture));
        assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_bytes);

        // 再次保存仍稳定在同一位置，不写回旧位置。
        save_at(&root, DesktopRole::Receive).unwrap();
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Receive));
        assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_bytes);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn persisted_role_survives_managed_to_legacy_rollback() {
        let root = temp_root("rollback-role");
        make_managed(&root);
        save_at(&root, DesktopRole::Capture).unwrap();
        let data = managed_startup(&root);

        fs::remove_file(root.join(paths::APP_DIR).join(paths::CURRENT_FILE_NAME)).unwrap();
        assert_eq!(path_at(&root), data);
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Capture));
        save_at(&root, DesktopRole::Receive).unwrap();
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Receive));
        assert_eq!(role_in_file(&data), "receive");
        assert!(!legacy_startup(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_file_takes_priority_over_legacy() {
        let root = temp_root("managed-priority");
        make_managed(&root);
        let managed = managed_startup(&root);
        let legacy = legacy_startup(&root);
        fs::create_dir_all(root.join(paths::DATA_DIR)).unwrap();
        fs::write(&managed, r#"{"schema":1,"role":"capture"}"#).unwrap();
        fs::write(&legacy, r#"{"schema":1,"role":"receive"}"#).unwrap();

        assert_eq!(path_at(&root), managed);
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Capture));

        // Managed 文件损坏时直接报错，不能悄悄回退到旧位置。
        fs::write(&managed, r#"{"schema":1,"role":"admin"}"#).unwrap();
        let error = load_at(&root).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(&managed).unwrap(),
            r#"{"schema":1,"role":"admin"}"#
        );
        assert_eq!(
            fs::read_to_string(&legacy).unwrap(),
            r#"{"schema":1,"role":"receive"}"#
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unknown_schema_error_mentions_schema_and_path() {
        let root = temp_root("schema-message");
        let path = legacy_startup(&root);
        fs::write(&path, r#"{"schema":3,"role":"capture"}"#).unwrap();
        let error = load_at(&root).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let message = error.to_string();
        assert!(message.contains("schema"), "{message}");
        assert!(message.contains("vc-startup.json"), "{message}");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn failed_atomic_replace_preserves_original_file() {
        use std::os::windows::fs::OpenOptionsExt;

        let root = temp_root("atomic-lock");
        save_at(&root, DesktopRole::Receive).unwrap();
        let path = legacy_startup(&root);
        let before = fs::read(&path).unwrap();

        // 独占打开目标文件（FILE_SHARE_DELETE 未授予）使 rename 失败。
        let guard = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let result = save_at(&root, DesktopRole::Capture);
        drop(guard);

        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Receive));
        assert!(temp_files(&root).is_empty(), "失败后必须清理临时文件");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_atomic_write_cleans_temp_and_leaves_target() {
        let root = temp_root("atomic-dir");
        let path = legacy_startup(&root);
        // 目标被占用为目录：rename 必然失败；目录内容代表原状态。
        fs::create_dir_all(path.join("sentinel")).unwrap();
        fs::write(path.join("sentinel").join("keep.txt"), "keep").unwrap();

        assert!(save_at(&root, DesktopRole::Capture).is_err());
        assert!(path.is_dir());
        assert_eq!(
            fs::read_to_string(path.join("sentinel").join("keep.txt")).unwrap(),
            "keep"
        );
        assert!(temp_files(&root).is_empty(), "失败后必须清理临时文件");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_persistence_is_independent_of_control_config() {
        let root = temp_root("independent");
        let config_path = root.join(paths::CONFIG_FILE_NAME);
        let mut config = crate::config::Config::default();
        config.capture.width = 1280;
        config.capture.height = 720;
        config.save(&config_path).unwrap();
        let config_before = fs::read(&config_path).unwrap();

        save_at(&root, DesktopRole::Receive).unwrap();
        save_at(&root, DesktopRole::Capture).unwrap();
        assert_eq!(fs::read(&config_path).unwrap(), config_before);
        assert_eq!(crate::config::Config::load(&config_path).unwrap(), config);

        // 控制面 REST 补丁保存配置后，启动偏好保持原样。
        let startup_before = fs::read(legacy_startup(&root)).unwrap();
        let mut patched = crate::config::Config::load(&config_path).unwrap();
        let errors = patched.apply_patch(&serde_json::json!({
            "capture": {"width": 416, "height": 416}
        }));
        assert!(errors.is_empty(), "{errors:?}");
        patched.save(&config_path).unwrap();

        assert_eq!(fs::read(legacy_startup(&root)).unwrap(), startup_before);
        assert_eq!(load_at(&root).unwrap(), Some(DesktopRole::Capture));
        assert_eq!(
            crate::config::Config::load(&config_path)
                .unwrap()
                .capture
                .width,
            416
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn root_path_wrapper_matches_root_resolution() {
        assert_eq!(path(), path_at(&paths::root()));
    }
}
