//! 版本化发布、离线包校验、OTA 激活与回滚。UI 无关，主机与被采集机共用。
//!
//! 目录（见 [`crate::paths`]）：
//! * `app/releases/<build_id>/`：已安装的只读程序；
//! * `app/current.json`：原子切换的当前版本指针（存在即进入 Managed 布局）；
//! * `updates/staged/<build_id>/`：已校验、未激活的发布；
//! * `updates/worker/`：复制的 helper、job 与日志；
//! * `updates/history.jsonl`：激活/回滚事件。
//!
//! 上传包必须是本项目的便携 ZIP：内含 `package-manifest.json`，逐文件记录
//! SHA-256 与字节数。任何不一致、路径越界、重复、符号链接或不认识的发布都拒绝部署。

use crate::paths;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PRODUCT: &str = "VirtualCapture-Rust-portable";
pub const MANIFEST_NAME: &str = "package-manifest.json";
pub const UPDATE_SCHEMA: u32 = 1;
/// 上传 ZIP 上限。便携包约 17 MiB，留足余量同时避免异常请求耗尽磁盘。
pub const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const READY_POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Capture,
    Host,
}

impl Role {
    fn exe(self) -> &'static str {
        match self {
            Role::Capture => "vc-capture.exe",
            Role::Host => "vc-host.exe",
        }
    }
}

/// 回滚探测本版运行时 DLL 的名字；必须与 `paths::data_plane_dll`/`paths::vcam_dll` 同名。
/// 旧 C++ 的 vc-sender.exe / vc-vcam.dll 已不在发布包内，用它们探测会静默落到 build 目录，
/// 回滚启动时就会拿到错误（或缺失）的 DLL 目录。
fn native_probe(role: Role) -> &'static str {
    match role {
        Role::Capture => "vc_stream.dll",
        Role::Host => "vc_vcam.dll",
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PackageManifest {
    pub product: String,
    pub schema: u32,
    pub version: String,
    pub build_id: String,
    pub files: Vec<UpdateFile>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StagedMeta {
    pub schema: u32,
    pub build_id: String,
    pub version: String,
    pub zip_sha256: String,
    pub bytes: u64,
    pub files: Vec<UpdateFile>,
    pub staged_utc: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CurrentPointer {
    pub schema: u32,
    pub build_id: String,
    pub version: String,
    pub generation: u64,
    pub activated_utc: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub build_id: String,
    pub version: String,
    pub state: String,
    pub at_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub const LEGACY_BASELINE_FILE: &str = "legacy-baseline.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaselineEntry {
    pub name: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// 扁平部署首次升级前记录的运行时基线：只登记文件名/大小/哈希，
/// 不复制、不移动 config、data、run 与令牌，回滚时移除指针即可恢复旧布局。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LegacyBaseline {
    pub schema: u32,
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_dir: Option<String>,
    pub created_utc: String,
    pub entries: Vec<BaselineEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivationRequest {
    pub role: Role,
    pub target: String,
    pub parent_pid: u32,
    pub port: u16,
    pub bind: String,
    pub wait_ms: u64,
    pub ready_ms: u64,
    pub require_ready: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Activation {
    pub helper_pid: u32,
    pub job_path: PathBuf,
    pub ready_path: Option<PathBuf>,
    pub log_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivationJob {
    pub schema: u32,
    pub root: String,
    pub role: Role,
    pub target: String,
    pub previous: Option<String>,
    pub previous_exe: Option<String>,
    pub previous_native_dir: Option<String>,
    pub parent_pid: u32,
    pub port: u16,
    pub bind: String,
    pub wait_ms: u64,
    pub ready_ms: u64,
    pub ready_path: Option<String>,
    pub created_utc: String,
}

#[derive(Clone, Debug)]
pub struct ActivationError {
    pub code: i32,
    pub message: String,
}

impl ActivationError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn error<T>(message: impl Into<String>) -> Result<T, String> {
    Err(message.into())
}

// ---------------------------------------------------------------- 通用工具

pub fn iso8601(secs: u64) -> String {
    // Howard Hinnant 的 civil-from-days；只用于显示/审计，不参与校验。
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

pub fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    iso8601(secs)
}

#[cfg(windows)]
pub fn random_hex(bytes: usize) -> String {
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    let mut buffer = vec![0u8; bytes];
    // 安全：buffer 可写且长度与传入的字节数一致；系统 RNG 不需要额外上下文。
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        return hex::encode(buffer);
    }
    weak_random_hex(bytes)
}

#[cfg(not(windows))]
pub fn random_hex(bytes: usize) -> String {
    weak_random_hex(bytes)
}

fn weak_random_hex(bytes: usize) -> String {
    // 仅在系统 CSPRNG 不可用时兜底；令牌仍绑定短生命周期与本地文件。
    let mut hasher = Sha256::new();
    hasher.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    hasher.update(std::process::id().to_le_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..bytes.min(digest.len())])
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("无法读取 {}：{e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// build_id 规则统一由 [`paths::valid_build_id`] 维护（含 `.`/`..` 与 Windows 保留名）。
fn valid_build_id(id: &str) -> bool {
    paths::valid_build_id(id)
}

fn valid_hash(text: &str) -> bool {
    text.len() == 64 && text.chars().all(|ch| ch.is_ascii_hexdigit())
}

/// 校验 ZIP 内的相对路径，返回无顶层目录的规范相对路径。
fn validate_relative(name: &str) -> Result<String, String> {
    if name.is_empty() || name.len() > 1024 {
        return error(format!("包内路径长度无效：{name:?}"));
    }
    if name.contains('\\') {
        return error(format!("包内路径不得使用反斜杠：{name}"));
    }
    if name.starts_with('/') || name.contains(':') {
        return error(format!("包内路径不得为绝对路径：{name}"));
    }
    if name.chars().any(|ch| ch.is_control()) {
        return error(format!("包内路径含控制字符：{name:?}"));
    }
    for component in name.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return error(format!("包内路径含非法片段：{name}"));
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return error(format!("包内路径片段以点或空格结尾：{name}"));
        }
        let stem = component.split('.').next().unwrap_or(component);
        let upper = stem.to_ascii_uppercase();
        if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (upper.len() == 4
                && (upper.starts_with("COM") || upper.starts_with("LPT"))
                && upper.as_bytes()[3].is_ascii_digit())
        {
            return error(format!("包内路径使用 Windows 保留名：{name}"));
        }
    }
    Ok(name.to_string())
}

// ---------------------------------------------------------------- 令牌与配对

#[derive(Serialize, Deserialize)]
struct TokenFile {
    schema: u32,
    token: String,
    created_utc: String,
}

const TOKEN_FILE_MAX_BYTES: u64 = 4096;

/// 配对令牌统一格式：64 位十六进制（32 字节随机数的十六进制表示）。
pub fn valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn read_token_file(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut text = String::new();
    file.take(TOKEN_FILE_MAX_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let value: TokenFile = serde_json::from_str(&text).ok()?;
    (value.schema == 1 && valid_token(&value.token)).then_some(value.token)
}

fn create_token_file(path: &Path) -> Result<String, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("无法创建令牌目录 {}：{e}", parent.display()))?;
    }
    let token = random_hex(32);
    let body = serde_json::to_vec(&TokenFile {
        schema: 1,
        token: token.clone(),
        created_utc: now_utc(),
    })
    .map_err(|e| e.to_string())?;
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(&body).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_token_file(path)
            .ok_or_else(|| "令牌文件存在但无法解析；请检查后删除再生成。".into()),
        Err(e) => error(format!("无法创建更新令牌 {}：{e}", path.display())),
    }
}

pub fn token_at(root: &Path) -> Result<String, String> {
    let path = paths::update_token_path_at(root);
    if let Some(token) = read_token_file(&path) {
        return Ok(token);
    }
    create_token_file(&path)
}

/// 当前 ROOT 的更新令牌；不存在则创建。绝不写入日志或状态响应。
pub fn token() -> Result<String, String> {
    token_at(&paths::root())
}

pub fn peek_token_at(root: &Path) -> Option<String> {
    read_token_file(&paths::update_token_path_at(root))
}

pub fn peek_token() -> Option<String> {
    peek_token_at(&paths::root())
}

/// 常量时间比较，避免按字节短路泄漏令牌长度/前缀。
pub fn token_matches(expected: &str, supplied: &str) -> bool {
    let (a, b) = (expected.as_bytes(), supplied.as_bytes());
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn load_peer_token_at(root: &Path) -> Option<String> {
    read_token_file(&paths::peer_update_token_path_at(root))
}

pub fn load_peer_token() -> Option<String> {
    load_peer_token_at(&paths::root())
}

pub fn save_peer_token_at(root: &Path, token: &str) -> Result<(), String> {
    if !valid_token(token) {
        return error("更新令牌格式无效：需要 64 位十六进制字符串。");
    }
    let path = paths::peer_update_token_path_at(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_vec(&TokenFile {
        schema: 1,
        token: token.to_string(),
        created_utc: now_utc(),
    })
    .map_err(|e| e.to_string())?;
    let temp = path.with_extension("json.tmp");
    {
        let mut file = File::create(&temp).map_err(|e| e.to_string())?;
        file.write_all(&body).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&temp, &path).map_err(|e| e.to_string())
}

pub fn save_peer_token(token: &str) -> Result<(), String> {
    save_peer_token_at(&paths::root(), token)
}

/// 主机首个窗口渲染后写更新就绪回执；`create_new` 保证不会被旧文件冒充。
pub fn write_ready(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let root = fs::canonicalize(paths::root()).unwrap_or_else(|_| paths::root());
    let body = serde_json::to_vec(&json!({
        "schema": 1,
        "pid": std::process::id(),
        "build": paths::current_build_id(),
        "root": root.to_string_lossy(),
        "ready_utc": now_utc(),
    }))
    .map_err(|e| e.to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("无法写更新就绪回执 {}：{e}", path.display()))?;
    file.write_all(&body).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- 包扫描与暂存

struct Scan {
    manifest: PackageManifest,
    /// ZIP 内统一的顶层目录（可为空）。
    prefix: String,
    zip_sha256: String,
}

fn open_manifest(archive: &mut zip::ZipArchive<File>) -> Result<(String, PackageManifest), String> {
    let mut found = None;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|e| format!("读取压缩包条目失败：{e}"))?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        let normalized = validate_relative(&name)?;
        if normalized == MANIFEST_NAME || normalized.ends_with(&format!("/{MANIFEST_NAME}")) {
            if found.is_some() {
                return error("压缩包含多个 package-manifest.json。");
            }
            if entry.size() > MAX_MANIFEST_BYTES {
                return error("package-manifest.json 过大。");
            }
            found = Some((normalized, index));
        }
    }
    let (normalized, index) = found.ok_or("压缩包缺少 package-manifest.json。")?;
    let entry = archive
        .by_index(index)
        .map_err(|e| format!("读取清单失败：{e}"))?;
    let mut text = String::new();
    entry
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("package-manifest.json 不是 UTF-8 文本：{e}"))?;
    let manifest: PackageManifest =
        serde_json::from_str(&text).map_err(|e| format!("package-manifest.json 无效：{e}"))?;
    Ok((normalized, manifest))
}

fn scan_zip(zip_path: &Path) -> Result<Scan, String> {
    let size = fs::metadata(zip_path)
        .map_err(|e| format!("无法读取上传文件：{e}"))?
        .len();
    if size == 0 {
        return error("上传文件为空。");
    }
    if size > MAX_UPLOAD_BYTES {
        return error(format!(
            "上传文件超过 {} MiB 上限。",
            MAX_UPLOAD_BYTES / (1024 * 1024)
        ));
    }
    let file = File::open(zip_path).map_err(|e| format!("无法打开压缩包：{e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("不是有效的 ZIP 压缩包：{e}"))?;
    let (manifest_name, manifest) = open_manifest(&mut archive)?;
    validate_manifest(&manifest)?;
    let prefix = manifest_name
        .strip_suffix(MANIFEST_NAME)
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    let manifest_relative = if prefix.is_empty() {
        MANIFEST_NAME.to_string()
    } else {
        format!("{prefix}/{MANIFEST_NAME}")
    };
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|e| format!("读取压缩包条目失败：{e}"))?;
        let raw = entry.name().to_string();
        if raw.ends_with('/') {
            continue;
        }
        if let Some(mode) = entry.unix_mode()
            && mode & 0o170_000 == 0o120_000
        {
            return error(format!("压缩包含符号链接，拒绝部署：{raw}"));
        }
        let relative = validate_relative(&raw)?;
        if relative == manifest_relative {
            continue;
        }
        let relative = if prefix.is_empty() {
            relative
        } else {
            let stripped = relative
                .strip_prefix(prefix.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .ok_or_else(|| format!("压缩包条目不在统一顶层目录 {prefix}/ 下：{raw}"))?;
            stripped.to_string()
        };
        if !seen.insert(relative.to_ascii_lowercase()) {
            return error(format!("压缩包含重复路径：{relative}"));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_TOTAL_BYTES {
            return error("压缩包解压总大小超过上限。");
        }
        entries.push((relative, entry.size(), raw));
        if entries.len() > MAX_FILES {
            return error("压缩包文件数超过上限。");
        }
    }
    let listed: std::collections::HashSet<String> = manifest
        .files
        .iter()
        .map(|file| file.path.to_ascii_lowercase())
        .collect();
    for (relative, _, _) in &entries {
        if !listed.contains(&relative.to_ascii_lowercase()) {
            return error(format!("压缩包条目未在清单中声明：{relative}"));
        }
    }
    if entries.len() != manifest.files.len() {
        return error("压缩包条目数量与清单不一致。");
    }
    Ok(Scan {
        manifest,
        prefix,
        zip_sha256: sha256_file(zip_path)?,
    })
}

fn validate_manifest(manifest: &PackageManifest) -> Result<(), String> {
    if manifest.product != PRODUCT {
        return error(format!(
            "发布包产品不符：{}（需要 {PRODUCT}）。",
            manifest.product
        ));
    }
    if manifest.schema != UPDATE_SCHEMA {
        return error(format!("不支持的发布包 schema：{}。", manifest.schema));
    }
    if manifest.version.trim().is_empty() || manifest.version.len() > 64 {
        return error("发布包版本号无效。");
    }
    if !valid_build_id(&manifest.build_id) {
        return error("发布包 build_id 无效。");
    }
    if manifest.files.is_empty() || manifest.files.len() > MAX_FILES {
        return error("发布包文件清单为空或过多。");
    }
    let mut seen = std::collections::HashSet::new();
    let mut total = 0u64;
    for file in &manifest.files {
        let normalized = validate_relative(&file.path)?;
        if normalized != file.path {
            return error(format!("清单路径不规范：{}", file.path));
        }
        if !seen.insert(file.path.to_ascii_lowercase()) {
            return error(format!("清单含重复路径：{}", file.path));
        }
        if file.bytes > MAX_FILE_BYTES {
            return error(format!("清单文件过大：{}", file.path));
        }
        if !valid_hash(&file.sha256) {
            return error(format!("清单哈希无效：{}", file.path));
        }
        total = total.saturating_add(file.bytes);
        if total > MAX_TOTAL_BYTES {
            return error("清单总大小超过上限。");
        }
    }
    Ok(())
}

/// 只读预检：在不上传、不落盘的情况下返回包摘要；主机可先本地校验再上传。
pub fn inspect_zip(zip_path: &Path) -> Result<Value, String> {
    let scan = scan_zip(zip_path)?;
    let bytes = scan.manifest.files.iter().map(|f| f.bytes).sum::<u64>();
    Ok(json!({
        "build_id": scan.manifest.build_id,
        "version": scan.manifest.version,
        "files": scan.manifest.files.len(),
        "bytes": bytes,
        "zip_sha256": scan.zip_sha256,
    }))
}

fn staged_meta_at(root: &Path, build_id: &str) -> Option<StagedMeta> {
    let text = fs::read_to_string(
        root.join(paths::UPDATES_DIR)
            .join("staged")
            .join(format!("{build_id}.json")),
    )
    .ok()?;
    serde_json::from_str(&text).ok()
}

/// 解包到 `updates/staged/<build_id>/`；校验清单中的每个文件字节数与 SHA-256。
pub fn stage_file(root: &Path, zip_path: &Path) -> Result<StagedMeta, String> {
    let scan = scan_zip(zip_path)?;
    let build_id = scan.manifest.build_id.clone();
    let staged_root = root.join(paths::UPDATES_DIR).join("staged");
    fs::create_dir_all(&staged_root).map_err(|e| format!("无法创建暂存目录：{e}"))?;
    let target = staged_root.join(&build_id);
    if target.exists() {
        if let Some(meta) = staged_meta_at(root, &build_id)
            && meta.zip_sha256 == scan.zip_sha256
        {
            // 同内容幂等，但仍要复核目录内容，防止暂存后被塞入清单外文件。
            return verify_staged_at(root, &build_id).map_err(|e| {
                format!("版本 {build_id} 已暂存但校验失败：{e}；请清理旧暂存后重新上传。")
            });
        }
        return error(format!(
            "版本 {build_id} 已暂存且内容不同；先清理旧暂存再上传。"
        ));
    }
    let nonce = random_hex(6);
    let temp = root
        .join(paths::UPDATES_DIR)
        .join("incoming")
        .join(format!("stage-{build_id}-{nonce}"));
    if let Some(parent) = temp.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&temp).map_err(|e| format!("无法创建暂存目录：{e}"))?;
    let result = extract_zip(zip_path, &temp, &scan);
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&temp);
        return Err(e);
    }
    // 清单本身也保留，便于审计；不参与哈希清单。
    let zip_file = File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(zip_file).map_err(|e| e.to_string())?;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        let normalized = validate_relative(&name)?;
        if normalized == MANIFEST_NAME || normalized.ends_with(&format!("/{MANIFEST_NAME}")) {
            let mut bytes = Vec::new();
            entry
                .take(MAX_MANIFEST_BYTES)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            fs::write(temp.join(MANIFEST_NAME), &bytes).map_err(|e| e.to_string())?;
            break;
        }
    }
    match fs::rename(&temp, &target) {
        Ok(()) => {}
        Err(e) if target.exists() => {
            let _ = fs::remove_dir_all(&temp);
            let meta = staged_meta_at(root, &build_id)
                .ok_or_else(|| format!("暂存目录已存在但元数据缺失：{e}"))?;
            if meta.zip_sha256 != scan.zip_sha256 {
                return error(format!("版本 {build_id} 已暂存且内容不同。"));
            }
            return verify_staged_at(root, &build_id).map_err(|e| {
                format!("版本 {build_id} 已暂存但校验失败：{e}；请清理旧暂存后重新上传。")
            });
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&temp);
            return error(format!("无法提交暂存目录：{e}"));
        }
    }
    let meta = StagedMeta {
        schema: UPDATE_SCHEMA,
        build_id,
        version: scan.manifest.version.clone(),
        zip_sha256: scan.zip_sha256.clone(),
        bytes: scan.manifest.files.iter().map(|f| f.bytes).sum(),
        files: scan.manifest.files.clone(),
        staged_utc: now_utc(),
    };
    let text = serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?;
    let meta_path = staged_root.join(format!("{}.json", meta.build_id));
    let temp_meta = staged_root.join(format!(".{}.json.tmp", meta.build_id));
    {
        let mut file = File::create(&temp_meta).map_err(|e| e.to_string())?;
        file.write_all(&text).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&temp_meta, &meta_path).map_err(|e| e.to_string())?;
    Ok(meta)
}

fn extract_zip(zip_path: &Path, temp: &Path, scan: &Scan) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let declared: std::collections::HashMap<String, &UpdateFile> = scan
        .manifest
        .files
        .iter()
        .map(|f| (f.path.to_ascii_lowercase(), f))
        .collect();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
        let raw = entry.name().to_string();
        if raw.ends_with('/') {
            continue;
        }
        let relative = validate_relative(&raw)?;
        let relative = if scan.prefix.is_empty() {
            relative
        } else {
            relative
                .strip_prefix(scan.prefix.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .ok_or_else(|| format!("压缩包条目不在统一顶层目录下：{raw}"))?
                .to_string()
        };
        if relative == MANIFEST_NAME {
            continue;
        }
        let Some(file_meta) = declared.get(&relative.to_ascii_lowercase()) else {
            return error(format!("压缩包条目未在清单中声明：{relative}"));
        };
        let destination = temp.join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|e| format!("无法写入暂存文件 {relative}：{e}"))?;
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        let mut buffer = vec![0u8; 256 * 1024];
        loop {
            let read = entry
                .read(&mut buffer)
                .map_err(|e| format!("解压 {relative} 失败：{e}"))?;
            if read == 0 {
                break;
            }
            written = written.saturating_add(read as u64);
            if written > file_meta.bytes {
                return error(format!("解压 {relative} 超过清单字节数。"));
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|e| format!("写入 {relative} 失败：{e}"))?;
        }
        output.sync_all().map_err(|e| e.to_string())?;
        if written != file_meta.bytes {
            return error(format!("文件字节数与清单不一致：{relative}"));
        }
        let digest = hex::encode(hasher.finalize());
        if !digest.eq_ignore_ascii_case(&file_meta.sha256) {
            return error(format!("文件哈希与清单不一致：{relative}"));
        }
    }
    Ok(())
}

/// 目录项是否为符号链接或 Windows 重解析点（junction/挂载点），一律拒绝部署。
fn is_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x0000_0400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// 校验 meta 内路径并返回允许出现的文件集合（含 manifest 自身）。
fn checked_meta_files(meta: &StagedMeta) -> Result<std::collections::HashSet<String>, String> {
    let mut allowed = std::collections::HashSet::new();
    allowed.insert(MANIFEST_NAME.to_string());
    for file in &meta.files {
        validate_relative(&file.path)?;
        if !allowed.insert(file.path.to_ascii_lowercase()) {
            return error(format!("清单含重复路径：{}", file.path));
        }
    }
    Ok(allowed)
}

fn verify_files(dir: &Path, files: &[UpdateFile], label: &str) -> Result<(), String> {
    for file in files {
        validate_relative(&file.path)?;
        let path = dir.join(&file.path);
        let metadata =
            fs::symlink_metadata(&path).map_err(|e| format!("{label}缺失 {}：{e}", file.path))?;
        if is_reparse(&metadata) || !metadata.is_file() {
            return error(format!("{label}不是常规文件：{}", file.path));
        }
        if metadata.len() != file.bytes {
            return error(format!("{label}大小不符：{}", file.path));
        }
        if !sha256_file(&path)?.eq_ignore_ascii_case(&file.sha256) {
            return error(format!("{label}哈希不符：{}", file.path));
        }
    }
    Ok(())
}

/// 拒绝清单外的文件与目录项，防止向 release 夹带 DLL 或重解析点。
fn verify_no_extras(
    dir: &Path,
    allowed: &std::collections::HashSet<String>,
    label: &str,
) -> Result<(), String> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current).map_err(|e| format!("{label}不可读：{e}"))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if is_reparse(&metadata) {
                return error(format!("{label}含符号链接/重解析点：{}", path.display()));
            }
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !metadata.is_file() {
                return error(format!("{label}含非常规文件：{}", path.display()));
            }
            let relative = path
                .strip_prefix(dir)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            if !allowed.contains(&relative.to_ascii_lowercase()) {
                return error(format!("{label}含清单外文件：{relative}"));
            }
        }
    }
    Ok(())
}

/// 重算暂存文件哈希，确认与清单一致；激活前必须通过。
pub fn verify_staged_at(root: &Path, build_id: &str) -> Result<StagedMeta, String> {
    if !valid_build_id(build_id) {
        return error("build_id 无效。");
    }
    let meta =
        staged_meta_at(root, build_id).ok_or_else(|| format!("版本 {build_id} 尚未暂存。"))?;
    let allowed = checked_meta_files(&meta)?;
    let dir = root.join(paths::UPDATES_DIR).join("staged").join(build_id);
    verify_files(&dir, &meta.files, "暂存文件")?;
    verify_no_extras(&dir, &allowed, "暂存目录")?;
    Ok(meta)
}

pub fn verify_staged(build_id: &str) -> Result<StagedMeta, String> {
    verify_staged_at(&paths::root(), build_id)
}

// ---------------------------------------------------------------- 状态与历史

fn read_pointer_at(root: &Path) -> Option<CurrentPointer> {
    let text = fs::read_to_string(root.join(paths::APP_DIR).join(paths::CURRENT_FILE_NAME)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn append_history_at(root: &Path, entry: &HistoryEntry) {
    let path = root.join(paths::UPDATES_DIR).join("history.jsonl");
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path)
        && let Ok(mut line) = serde_json::to_vec(entry)
    {
        line.push(b'\n');
        let _ = file.write_all(&line);
        let _ = file.sync_data();
    }
}

pub fn append_history(entry: &HistoryEntry) {
    append_history_at(&paths::root(), entry);
}

fn read_history_at(root: &Path, limit: usize) -> Vec<HistoryEntry> {
    let Ok(text) = fs::read_to_string(root.join(paths::UPDATES_DIR).join("history.jsonl")) else {
        return Vec::new();
    };
    let mut entries: Vec<HistoryEntry> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if entries.len() > limit {
        entries.drain(..entries.len() - limit);
    }
    entries
}

fn newest_staged_at(root: &Path) -> Option<StagedMeta> {
    let dir = root.join(paths::UPDATES_DIR).join("staged");
    let mut best: Option<(SystemTime, StagedMeta)> = None;
    for entry in fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<StagedMeta>(&text) else {
            continue;
        };
        let time = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        if best.as_ref().is_none_or(|(best_time, _)| time > *best_time) {
            best = Some((time, meta));
        }
    }
    best.map(|(_, meta)| meta)
}

pub fn status_at(root: &Path) -> Result<Value, String> {
    let layout = match paths::layout_at(root) {
        paths::Layout::Managed => "managed",
        paths::Layout::Legacy => "legacy",
    };
    Ok(json!({
        "ok": true,
        "layout": layout,
        "active": read_pointer_at(root),
        "staged": newest_staged_at(root),
        "history": read_history_at(root, 5),
        "token_present": paths::update_token_path_at(root).is_file(),
    }))
}

pub fn status() -> Result<Value, String> {
    status_at(&paths::root())
}

pub fn verify_release_at(root: &Path, build_id: &str) -> Result<Value, String> {
    if !valid_build_id(build_id) {
        return error("build_id 无效。");
    }
    let dir = root.join(paths::APP_DIR).join("releases").join(build_id);
    let meta =
        staged_meta_at(root, build_id).ok_or_else(|| format!("找不到版本 {build_id} 的清单。"))?;
    let allowed = checked_meta_files(&meta)?;
    verify_files(&dir, &meta.files, "已安装文件")?;
    verify_no_extras(&dir, &allowed, "版本目录")?;
    Ok(json!({"build_id":build_id,"version":meta.version,"files":meta.files.len()}))
}

pub fn verify_release(root: &Path, build_id: &str) -> Result<Value, String> {
    verify_release_at(root, build_id)
}

// ---------------------------------------------------------------- 安装与激活

/// 只复制清单声明的文件（外加 manifest 供审计），不复制目录内其他内容。
fn copy_declared(from: &Path, to: &Path, files: &[UpdateFile]) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| e.to_string())?;
    let mut copies: Vec<(PathBuf, PathBuf)> = files
        .iter()
        .map(|file| (from.join(&file.path), to.join(&file.path)))
        .collect();
    copies.push((from.join(MANIFEST_NAME), to.join(MANIFEST_NAME)));
    for (source, target) in copies {
        if !source.is_file() {
            return error(format!("版本文件缺失：{}", source.display()));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut input = File::open(&source).map_err(|e| e.to_string())?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| format!("无法写入 {}：{e}", target.display()))?;
        std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        output.sync_all().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 把暂存版本安装到 `app/releases/<build_id>/`（已存在则校验后复用）。
pub fn install_staged_at(root: &Path, build_id: &str) -> Result<PathBuf, String> {
    let meta = verify_staged_at(root, build_id)?;
    let releases = root.join(paths::APP_DIR).join("releases");
    fs::create_dir_all(&releases).map_err(|e| e.to_string())?;
    let target = releases.join(build_id);
    if target.is_dir() {
        verify_release_at(root, build_id)?;
        return Ok(target);
    }
    let temp = releases.join(format!(".tmp-{build_id}-{}", random_hex(4)));
    let staged = root.join(paths::UPDATES_DIR).join("staged").join(build_id);
    let allowed = checked_meta_files(&meta)?;
    let result = copy_declared(&staged, &temp, &meta.files)
        .and_then(|()| verify_files(&temp, &meta.files, "安装文件"))
        .and_then(|()| verify_no_extras(&temp, &allowed, "安装目录"));
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&temp);
        return Err(e);
    }
    match fs::rename(&temp, &target) {
        Ok(()) => {}
        Err(_e) if target.is_dir() => {
            let _ = fs::remove_dir_all(&temp);
            verify_release_at(root, build_id)?;
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&temp);
            return error(format!("无法安装版本 {build_id}：{e}"));
        }
    }
    let _ = meta;
    Ok(target)
}

fn write_pointer_at(root: &Path, pointer: &CurrentPointer) -> Result<(), String> {
    let dir = root.join(paths::APP_DIR);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(paths::CURRENT_FILE_NAME);
    let temp = dir.join(format!(".current-{}.tmp", random_hex(4)));
    let text = serde_json::to_vec_pretty(pointer).map_err(|e| e.to_string())?;
    {
        let mut file = File::create(&temp).map_err(|e| e.to_string())?;
        file.write_all(&text).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&temp, &path).map_err(|e| format!("无法切换版本指针：{e}"))
}

fn restore_pointer_at(root: &Path, previous: Option<&[u8]>) -> Result<(), String> {
    let path = root.join(paths::APP_DIR).join(paths::CURRENT_FILE_NAME);
    match previous {
        Some(bytes) => {
            let temp = path.with_extension("json.restore");
            {
                let mut file = File::create(&temp).map_err(|e| e.to_string())?;
                file.write_all(bytes).map_err(|e| e.to_string())?;
                file.sync_all().map_err(|e| e.to_string())?;
            }
            fs::rename(&temp, &path).map_err(|e| e.to_string())
        }
        None => {
            if path.exists() {
                fs::remove_file(&path).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}

fn helper_source(root: &Path, target: &str) -> Option<PathBuf> {
    let mut candidates = vec![
        root.join(paths::UPDATES_DIR)
            .join("staged")
            .join(target)
            .join("vc-updater.exe"),
    ];
    if let Some(dir) = paths::active_release_dir_at(root) {
        candidates.push(dir.join("vc-updater.exe"));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join("vc-updater.exe"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn spawn_detached(helper: &Path, job: &Path, log: &Path) -> Result<u32, String> {
    let mut command = std::process::Command::new(helper);
    command
        .arg("--job")
        .arg(job)
        .arg("--log")
        .arg(log)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP：父进程正常退出后 helper 继续运行。
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    command
        .spawn()
        .map(|child| child.id())
        .map_err(|e| format!("无法启动更新 helper：{e}"))
}

/// 记录扁平布局的运行时基线；已存在则保留首次记录，不覆盖。
fn write_legacy_baseline(
    root: &Path,
    exe: Option<&str>,
    native_dir: Option<&str>,
) -> Result<PathBuf, String> {
    let path = root.join(paths::UPDATES_DIR).join(LEGACY_BASELINE_FILE);
    if path.is_file() {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let excluded = [
        paths::APP_DIR,
        paths::DATA_DIR,
        paths::UPDATES_DIR,
        "run",
        paths::UPDATE_TOKEN_FILE_NAME,
        paths::PEER_UPDATE_TOKEN_FILE_NAME,
    ];
    let mut entries = Vec::new();
    let reader = fs::read_dir(root).map_err(|e| format!("读取 {} 失败：{e}", root.display()))?;
    for item in reader.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        if excluded.iter().any(|skip| skip.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let Ok(kind) = item.file_type() else {
            continue;
        };
        if kind.is_dir() {
            entries.push(BaselineEntry {
                name,
                kind: "dir".into(),
                bytes: None,
                sha256: None,
            });
        } else if kind.is_file() {
            entries.push(BaselineEntry {
                name,
                kind: "file".into(),
                bytes: item.metadata().ok().map(|meta| meta.len()),
                sha256: sha256_file(&item.path()).ok(),
            });
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let baseline = LegacyBaseline {
        schema: 1,
        root: root.to_string_lossy().to_string(),
        exe: exe.map(str::to_string),
        native_dir: native_dir.map(str::to_string),
        created_utc: now_utc(),
        entries,
    };
    let text = serde_json::to_vec_pretty(&baseline).map_err(|e| e.to_string())?;
    {
        let mut file = File::create(&path).map_err(|e| e.to_string())?;
        file.write_all(&text).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    Ok(path)
}

/// `vc-updater launch` 使用的当前可执行文件：有指针取 release，否则取扁平根。
pub fn launch_exe_at(root: &Path, role: Role) -> Result<PathBuf, String> {
    let exe = match paths::layout_at(root) {
        // 指针存在即 Managed；指针损坏/版本目录缺失必须明确失败，不能悄悄退回扁平布局。
        paths::Layout::Managed => {
            let build = paths::current_build_id_at(root).ok_or(
                "app/current.json 存在但缺少有效 build_id（指针损坏）；拒绝启动，请修复指针。",
            )?;
            let exe = root
                .join(paths::APP_DIR)
                .join("releases")
                .join(&build)
                .join(role.exe());
            if !exe.is_file() {
                return error(format!(
                    "当前版本 {build} 缺少可执行文件：{}；拒绝退回扁平布局。",
                    exe.display()
                ));
            }
            exe
        }
        paths::Layout::Legacy => root.join(role.exe()),
    };
    if !exe.is_file() {
        return error(format!("缺少可执行文件：{}", exe.display()));
    }
    Ok(exe)
}

pub fn prepare_activation_at(
    root: &Path,
    request: ActivationRequest,
) -> Result<Activation, String> {
    if !valid_build_id(&request.target) {
        return error("目标 build_id 无效。");
    }
    let meta = verify_staged_at(root, &request.target)?;
    let helper = helper_source(root, &request.target)
        .ok_or("发布包与当前版本都缺少 vc-updater.exe，无法激活。")?;
    let worker = root.join(paths::UPDATES_DIR).join("worker");
    fs::create_dir_all(&worker).map_err(|e| e.to_string())?;
    let nonce = random_hex(8);
    let helper_copy = worker.join(format!("vc-updater-{nonce}.exe"));
    fs::copy(&helper, &helper_copy).map_err(|e| format!("无法复制更新 helper：{e}"))?;
    let ready_path = request
        .require_ready
        .then(|| worker.join(format!("ready-{nonce}.json")));
    if let Some(path) = &ready_path
        && path.exists()
    {
        let _ = fs::remove_file(path);
    }
    let previous = read_pointer_at(root).map(|p| p.build_id);
    let previous_exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    // 回滚要按当前进程真实的 native 目录启动（开发时可能是 --native-dir 覆盖），
    // 不能固定写 root，否则从 managed 回滚到扁平/开发构建会找不到原生 DLL。
    let previous_native_dir = paths::native_exe(native_probe(request.role))
        .parent()
        .map(|path| path.to_string_lossy().to_string());
    if previous.is_none() {
        write_legacy_baseline(
            root,
            previous_exe.as_deref(),
            previous_native_dir.as_deref(),
        )
        .map_err(|e| format!("无法记录扁平回滚基线：{e}"))?;
    }
    let job = ActivationJob {
        schema: 1,
        root: fs::canonicalize(root)
            .unwrap_or_else(|_| root.to_path_buf())
            .to_string_lossy()
            .to_string(),
        role: request.role,
        target: request.target,
        previous,
        previous_exe,
        previous_native_dir,
        parent_pid: request.parent_pid,
        port: request.port,
        bind: request.bind,
        wait_ms: request.wait_ms.max(5_000),
        ready_ms: request.ready_ms.max(5_000),
        ready_path: ready_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        created_utc: now_utc(),
    };
    let job_path = worker.join(format!("job-{nonce}.json"));
    let text = serde_json::to_vec_pretty(&job).map_err(|e| e.to_string())?;
    {
        let mut file = File::create(&job_path).map_err(|e| e.to_string())?;
        file.write_all(&text).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    let log_path = worker.join(format!("updater-{nonce}.log"));
    let helper_pid = spawn_detached(&helper_copy, &job_path, &log_path)?;
    append_history_at(
        root,
        &HistoryEntry {
            build_id: meta.build_id,
            version: meta.version,
            state: "prepared".into(),
            at_utc: now_utc(),
            error: None,
        },
    );
    Ok(Activation {
        helper_pid,
        job_path,
        ready_path,
        log_path,
    })
}

pub fn prepare_activation(request: ActivationRequest) -> Result<Activation, String> {
    prepare_activation_at(&paths::root(), request)
}

pub fn read_job(path: &Path) -> Result<ActivationJob, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("无法读取 job：{e}"))?;
    let job: ActivationJob =
        serde_json::from_str(&text).map_err(|e| format!("job 文件无效：{e}"))?;
    if job.schema != 1 {
        return error("不支持的 job schema。");
    }
    if job.root.is_empty() || !valid_build_id(&job.target) {
        return error("job 缺少有效 root/target。");
    }
    Ok(job)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    if pid == 0 {
        return false;
    }
    // 安全：只通过本进程打开的查询句柄读取退出码；句柄在成功路径上恰好关闭
    // 一次，失败时提前返回。
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE
    }
}

#[cfg(not(windows))]
fn process_alive(_pid: u32) -> bool {
    false
}

fn wait_parent(pid: u32, wait_ms: u64) -> Result<(), ActivationError> {
    let deadline = SystemTime::now() + Duration::from_millis(wait_ms);
    while process_alive(pid) {
        if SystemTime::now() >= deadline {
            return Err(ActivationError::new(3, "等待旧进程退出超时；未切换版本。"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn same_root(a: &str, b: &Path) -> bool {
    let actual = fs::canonicalize(a).unwrap_or_else(|_| PathBuf::from(a));
    let expected = fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    actual
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(expected.to_string_lossy().trim_end_matches(['\\', '/']))
}

fn health_once(port: u16, bind: &str) -> Option<Value> {
    let host = match bind {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    };
    let url = format!("http://{host}:{port}/health");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(1)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent.get(&url).call().ok()?;
    if !response.status().is_success() {
        return None;
    }
    serde_json::from_str(&response.body_mut().read_to_string().ok()?).ok()
}

/// 就绪必须来自我们刚启动的子进程，且版本与当前指针预期一致；
/// `expected_build = None` 表示扁平布局（health 的 build 应为 null）。
fn capture_ready(
    job: &ActivationJob,
    root: &Path,
    expected_build: Option<&str>,
    child_pid: Option<u32>,
) -> bool {
    let Some(health) = health_once(job.port, &job.bind) else {
        return false;
    };
    if health["ok"] != true
        || health["implementation"] != "virtualcapture-rust"
        || health["role"] != "capture"
    {
        return false;
    }
    if let Some(pid) = child_pid
        && health["pid"].as_u64() != Some(pid as u64)
    {
        return false;
    }
    let build_matches = match expected_build {
        Some(build) => health["build"].as_str() == Some(build),
        None => health["build"].is_null(),
    };
    build_matches
        && health["root"]
            .as_str()
            .is_some_and(|value| same_root(value, root))
}

fn host_ready(job: &ActivationJob, child_pid: u32, root: &Path) -> bool {
    let Some(path) = &job.ready_path else {
        return true; // 回滚时不要求旧版 host 支持 ready 回执。
    };
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    value["schema"] == 1
        && value["pid"].as_u64() == Some(child_pid as u64)
        && value["build"].as_str() == Some(job.target.as_str())
        && value["root"]
            .as_str()
            .is_some_and(|value| same_root(value, root))
}

fn wait_ready(
    job: &ActivationJob,
    root: &Path,
    child: &mut std::process::Child,
) -> Result<(), String> {
    let deadline = SystemTime::now() + Duration::from_millis(job.ready_ms);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())?
            && !status.success()
        {
            return Err(format!("新版本进程提前退出（{status}）。"));
        }
        let ready = match job.role {
            Role::Capture => capture_ready(job, root, Some(&job.target), Some(child.id())),
            Role::Host => {
                child.try_wait().ok().flatten().is_none() && host_ready(job, child.id(), root)
            }
        };
        if ready {
            return Ok(());
        }
        if SystemTime::now() >= deadline {
            return Err("新版本未在时限内就绪。".into());
        }
        std::thread::sleep(READY_POLL);
    }
}

fn role_args(job: &ActivationJob, native_dir: &Path, ready: Option<&Path>) -> Vec<String> {
    let mut args = vec![
        "--root".into(),
        job.root.clone(),
        "--native-dir".into(),
        native_dir.to_string_lossy().to_string(),
    ];
    match job.role {
        Role::Capture => {
            args.push("--bind".into());
            args.push(job.bind.clone());
            args.push("--port".into());
            args.push(job.port.to_string());
        }
        Role::Host => {
            if let Some(path) = ready {
                args.push("--update-ready".into());
                args.push(path.to_string_lossy().to_string());
            }
        }
    }
    args
}

fn spawn_role(
    job: &ActivationJob,
    exe: &Path,
    native_dir: Option<&Path>,
    ready: Option<&Path>,
) -> Result<std::process::Child, String> {
    if !exe.is_file() {
        return error(format!("缺少可执行文件：{}", exe.display()));
    }
    let fallback = exe.parent().unwrap_or(Path::new("."));
    let native_dir = native_dir.unwrap_or(fallback);
    let mut command = std::process::Command::new(exe);
    command
        .args(role_args(job, native_dir, ready))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    command
        .spawn()
        .map_err(|e| format!("无法启动 {}：{e}", exe.display()))
}

fn release_exe(root: &Path, build_id: &str, role: Role) -> PathBuf {
    root.join(paths::APP_DIR)
        .join("releases")
        .join(build_id)
        .join(role.exe())
}

fn restart_previous(job: &ActivationJob, root: &Path) -> Result<(), String> {
    let (exe, ready) = match &job.previous {
        Some(build) => (release_exe(root, build, job.role), None),
        None => {
            let exe = job
                .previous_exe
                .as_ref()
                .map(PathBuf::from)
                .ok_or("没有可回滚的旧版本。")?;
            (exe, None)
        }
    };
    // 回滚不传 --update-ready：旧版 host 不认识该参数，会直接退出。
    let native_dir = job.previous_native_dir.as_deref().map(Path::new);
    let mut child = spawn_role(job, &exe, native_dir, ready)?;
    let deadline = SystemTime::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())?
            && !status.success()
        {
            return Err(format!("回滚版本启动失败（{status}）。"));
        }
        let alive = child.try_wait().ok().flatten().is_none();
        let healthy = match job.role {
            Role::Capture => capture_ready(job, root, job.previous.as_deref(), Some(child.id())),
            Role::Host => alive,
        };
        if alive && healthy {
            return Ok(());
        }
        if SystemTime::now() >= deadline {
            return Err("回滚版本未在时限内就绪。".into());
        }
        std::thread::sleep(READY_POLL);
    }
}

/// 进入 Managed 布局时把旧扁平令牌复制到 `data/`，保持主机配对不失效。
fn migrate_credentials(root: &Path) -> Result<(), String> {
    if paths::layout_at(root) != paths::Layout::Managed {
        return Ok(());
    }
    let data = root.join(paths::DATA_DIR);
    for (legacy_name, name) in [
        (paths::UPDATE_TOKEN_FILE_NAME, paths::UPDATE_TOKEN_FILE_NAME),
        (
            paths::PEER_UPDATE_TOKEN_FILE_NAME,
            paths::PEER_UPDATE_TOKEN_FILE_NAME,
        ),
    ] {
        let legacy = root.join(legacy_name);
        let managed = data.join(name);
        if legacy.is_file() && !managed.exists() {
            fs::create_dir_all(&data).map_err(|e| e.to_string())?;
            fs::copy(&legacy, &managed).map_err(|e| format!("无法迁移 {legacy_name}：{e}"))?;
        }
    }
    Ok(())
}

/// 激活失败后的统一回滚：恢复原指针并重启旧版本；任一失败以 code 6 退出并记入历史。
fn rollback_after_failure(
    job: &ActivationJob,
    root: &Path,
    previous_bytes: Option<&[u8]>,
    version: &str,
    base_code: i32,
    message: String,
) -> ActivationError {
    let restore = restore_pointer_at(root, previous_bytes);
    let restart = restart_previous(job, root);
    let mut detail = message;
    if let Err(e) = &restore {
        detail = format!("{detail}；恢复版本指针失败：{e}");
    }
    if let Err(e) = &restart {
        detail = format!("{detail}；旧版本重启失败：{e}");
    }
    let code = if restore.is_err() || restart.is_err() {
        6
    } else {
        base_code
    };
    // code 6 = 旧版本也未能恢复，和“已回滚但旧版重启”区分开，供 UI/host 展示。
    let state = if code == 6 {
        "rollback_failed"
    } else {
        "rolled_back"
    };
    append_history_at(
        root,
        &HistoryEntry {
            build_id: job.target.clone(),
            version: version.to_string(),
            state: state.into(),
            at_utc: now_utc(),
            error: Some(detail.clone()),
        },
    );
    ActivationError::new(code, detail)
}

/// helper 主流程：等旧进程退出 → 复核 → 原子切换 → 启动并确认就绪；失败恢复旧版本。
pub fn execute_activation(job: &ActivationJob) -> Result<(), ActivationError> {
    let root = PathBuf::from(&job.root);
    wait_parent(job.parent_pid, job.wait_ms)?;
    // 旧进程已退出：之后任何失败都必须恢复指针并重启旧版本，否则后台会一直停着。
    let pointer_path = root.join(paths::APP_DIR).join(paths::CURRENT_FILE_NAME);
    let previous_bytes = fs::read(&pointer_path).ok();
    let meta = match verify_staged_at(&root, &job.target) {
        Ok(meta) => meta,
        Err(e) => {
            return Err(rollback_after_failure(
                job,
                &root,
                previous_bytes.as_deref(),
                "",
                4,
                format!("激活前复核失败：{e}"),
            ));
        }
    };
    let release = match install_staged_at(&root, &job.target) {
        Ok(release) => release,
        Err(e) => {
            return Err(rollback_after_failure(
                job,
                &root,
                previous_bytes.as_deref(),
                &meta.version,
                4,
                format!("安装版本失败：{e}"),
            ));
        }
    };
    let previous_generation = read_pointer_at(&root)
        .map(|pointer| pointer.generation)
        .unwrap_or(0);
    append_history_at(
        &root,
        &HistoryEntry {
            build_id: job.target.clone(),
            version: meta.version.clone(),
            state: "activating".into(),
            at_utc: now_utc(),
            error: None,
        },
    );
    let pointer = CurrentPointer {
        schema: 1,
        build_id: job.target.clone(),
        version: meta.version.clone(),
        generation: previous_generation + 1,
        activated_utc: now_utc(),
    };
    if let Err(e) = write_pointer_at(&root, &pointer) {
        return Err(rollback_after_failure(
            job,
            &root,
            previous_bytes.as_deref(),
            &meta.version,
            4,
            format!("切换版本指针失败：{e}"),
        ));
    }
    // 指针已切到 Managed 后、启动新进程前，把旧扁平配置/令牌搬进 `data/`；
    // 否则新进程会读到默认配置并生成新令牌，破坏主机配对。
    let migration = paths::migrate_legacy_config(&root)
        .map_err(|e| e.to_string())
        .and_then(|_| migrate_credentials(&root));
    if let Err(e) = migration {
        return Err(rollback_after_failure(
            job,
            &root,
            previous_bytes.as_deref(),
            &meta.version,
            4,
            format!("迁移本地配置/令牌失败：{e}"),
        ));
    }
    let exe = release.join(job.role.exe());
    let ready = job.ready_path.as_ref().map(PathBuf::from);
    let mut child = match spawn_role(job, &exe, None, ready.as_deref()) {
        Ok(child) => child,
        Err(e) => {
            return Err(rollback_after_failure(
                job,
                &root,
                previous_bytes.as_deref(),
                &meta.version,
                5,
                e,
            ));
        }
    };
    if let Err(e) = wait_ready(job, &root, &mut child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(rollback_after_failure(
            job,
            &root,
            previous_bytes.as_deref(),
            &meta.version,
            5,
            e,
        ));
    }
    append_history_at(
        &root,
        &HistoryEntry {
            build_id: job.target.clone(),
            version: meta.version,
            state: "active".into(),
            at_utc: now_utc(),
            error: None,
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ids_are_bounded_and_safe() {
        assert!(valid_build_id("20261114-124010-20e160"));
        assert!(valid_build_id("a.b_c"));
        assert!(!valid_build_id(""));
        assert!(!valid_build_id("../x"));
        assert!(!valid_build_id("a/b"));
        assert!(!valid_build_id("."));
        assert!(!valid_build_id(".."));
        assert!(!valid_build_id("CON"));
        assert!(!valid_build_id("lpt1.backup"));
        assert!(!valid_build_id("a."));
        assert!(!valid_build_id(&"a".repeat(65)));
    }

    #[test]
    fn relative_paths_reject_traversal_and_windows_ambiguity() {
        assert_eq!(validate_relative("a/b.txt").unwrap(), "a/b.txt");
        assert!(validate_relative("../a").is_err());
        assert!(validate_relative("/a").is_err());
        assert!(validate_relative("C:/a").is_err());
        assert!(validate_relative("a\\b").is_err());
        assert!(validate_relative("a//b").is_err());
        assert!(validate_relative("a/").is_err());
        assert!(validate_relative("con.txt").is_err());
        assert!(validate_relative("a/name.").is_err());
    }

    #[test]
    fn manifest_requires_known_product_and_file_hashes() {
        let mut manifest = PackageManifest {
            product: PRODUCT.into(),
            schema: UPDATE_SCHEMA,
            version: "0.1.0".into(),
            build_id: "b1".into(),
            files: vec![UpdateFile {
                path: "a.txt".into(),
                bytes: 1,
                sha256: "0".repeat(64),
            }],
        };
        assert!(validate_manifest(&manifest).is_ok());
        manifest.product = "other".into();
        assert!(validate_manifest(&manifest).is_err());
        manifest.product = PRODUCT.into();
        manifest.files[0].sha256 = "xyz".into();
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn token_comparison_rejects_prefixes() {
        let token = "a".repeat(64);
        assert!(token_matches(&token, &token));
        assert!(!token_matches(&token, &token[..63]));
        assert!(!token_matches(&token, ""));
    }

    #[test]
    fn iso_time_is_well_formed() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn rollback_native_probe_tracks_versioned_runtime_dlls() {
        let file_name = |path: PathBuf| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        };
        assert_eq!(
            file_name(paths::data_plane_dll()).as_deref(),
            Some(native_probe(Role::Capture))
        );
        assert_eq!(
            file_name(paths::vcam_dll()).as_deref(),
            Some(native_probe(Role::Host))
        );
        // 旧 C++ 名字已不在发布包内；回滚探测不能再用它们。
        assert_ne!(native_probe(Role::Capture), "vc-sender.exe");
        assert_ne!(native_probe(Role::Host), "vc-vcam.dll");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vc-update-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn tokens_are_strictly_64_hex() {
        assert!(valid_token(&"a".repeat(64)));
        assert!(!valid_token(&"a".repeat(63)));
        assert!(!valid_token(&"g".repeat(64)));
        assert!(!valid_token(""));
    }

    #[test]
    fn token_file_read_stops_at_limit() {
        let dir = temp_dir("token");
        let path = dir.join("update-token.json");
        fs::write(
            &path,
            format!(
                r#"{{"schema":1,"token":"{}","created_utc":"x"}}"#,
                "a".repeat(5_000)
            ),
        )
        .unwrap();
        assert!(read_token_file(&path).is_none());
        let expected = "a".repeat(64);
        fs::write(
            &path,
            format!(r#"{{"schema":1,"token":"{expected}","created_utc":"x"}}"#),
        )
        .unwrap();
        assert_eq!(read_token_file(&path), Some(expected));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn staged_dirs_reject_extra_files() {
        let dir = temp_dir("extras");
        fs::write(dir.join(MANIFEST_NAME), b"{}").unwrap();
        fs::write(dir.join("a.dll"), b"x").unwrap();
        let mut allowed = std::collections::HashSet::new();
        allowed.insert(MANIFEST_NAME.to_string());
        allowed.insert("a.dll".to_string());
        assert!(verify_no_extras(&dir, &allowed, "test").is_ok());
        fs::write(dir.join("evil.dll"), b"x").unwrap();
        assert!(verify_no_extras(&dir, &allowed, "test").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn staged_meta_paths_are_revalidated() {
        let meta = StagedMeta {
            schema: UPDATE_SCHEMA,
            build_id: "b1".into(),
            version: "0.1.0".into(),
            zip_sha256: "0".repeat(64),
            bytes: 1,
            files: vec![UpdateFile {
                path: "../escape".into(),
                bytes: 1,
                sha256: "0".repeat(64),
            }],
            staged_utc: String::new(),
        };
        assert!(checked_meta_files(&meta).is_err());
    }

    #[test]
    fn launch_rejects_corrupt_pointer_instead_of_falling_back() {
        let dir = temp_dir("launch");
        fs::create_dir_all(dir.join(paths::APP_DIR)).unwrap();
        fs::write(
            dir.join(paths::APP_DIR).join(paths::CURRENT_FILE_NAME),
            "not json",
        )
        .unwrap();
        assert!(launch_exe_at(&dir, Role::Capture).is_err());
        fs::write(dir.join("vc-capture.exe"), b"MZ").unwrap();
        assert!(launch_exe_at(&dir, Role::Capture).is_err());
        fs::remove_file(dir.join(paths::APP_DIR).join(paths::CURRENT_FILE_NAME)).unwrap();
        assert!(launch_exe_at(&dir, Role::Capture).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
