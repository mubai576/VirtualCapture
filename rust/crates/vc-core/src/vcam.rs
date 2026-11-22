//! 虚拟采集卡（Rust DShow COM `vc_vcam.dll`）注册与状态。
//!
//! 复用与 C++ 兼容的 CLSID/注册表语义，不重写 DShow filter 接口。注册写
//! HKCU\\Software\\Classes\\CLSID\\{...}，产品固定 `FrameMap=<shm>`、`Motion=0`。
//! 能力探测走主程序的隔离子进程，不注册 COM、不接管现用设备。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 与 `vcam/vcam.cpp` 一致的 CLSID。
pub const CLSID: &str = "{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}";

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct VcamStatus {
    /// 本目录下找到的 DLL 绝对路径。
    pub dll: String,
    /// 注册表里当前的 InprocServer32 路径。
    pub registered: String,
    /// 注册项是否指向本目录 DLL。
    pub match_local: bool,
    /// 注册表读取失败（权限/IO），不能把“读不到”当成“未注册”。
    #[serde(default)]
    pub registration_unknown: bool,
}

impl VcamStatus {
    pub fn managed(&self) -> bool {
        self.match_local && !self.registered.is_empty()
    }
}

pub fn dll_path() -> PathBuf {
    crate::paths::vcam_dll()
}

pub fn status() -> VcamStatus {
    let dll = dll_path();
    let (registered, registration_unknown) = match read_registered_dll() {
        RegistryRead::Missing => (String::new(), false),
        RegistryRead::Value(value) => (value, false),
        RegistryRead::Unknown => (String::new(), true),
    };
    let match_local = same_path(&registered, &dll.display().to_string());
    VcamStatus {
        dll: dll.display().to_string(),
        registered,
        match_local,
        registration_unknown,
    }
}

fn normalize(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    #[cfg(windows)]
    let path = normalize_windows(path, canonicalize_for_owner)?;
    Some(owner_path_key(&path))
}

fn owner_path_key(path: &str) -> String {
    let path = path.replace('/', "\\").to_ascii_lowercase();
    let path = if let Some(unc) = path.strip_prefix(r"\\?\unc\") {
        format!(r"\\{unc}")
    } else if path.as_bytes().get(5..7) == Some(b":\\") {
        // 只有 DOS 盘符会去掉 verbatim 前缀。卷 GUID 必须保留其命名空间，
        // 否则键值可能与相对的 "Volume{...}" 路径混叠。
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
    } else {
        path
    };
    path.trim_end_matches('\\').to_owned()
}

#[cfg(windows)]
fn canonicalize_for_owner(path: &std::path::Path) -> std::io::Result<PathBuf> {
    canonicalize_for_owner_using(
        path,
        |candidate| std::fs::canonicalize(candidate),
        |candidate| std::fs::symlink_metadata(candidate).map(|_| ()),
    )
}

#[cfg(windows)]
fn canonicalize_for_owner_using(
    path: &std::path::Path,
    canonicalize: impl FnOnce(&std::path::Path) -> std::io::Result<PathBuf>,
    symlink_metadata: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    match canonicalize(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // 悬挂的 junction/symlink 不算“已删除的词法组件”。
            // 若现存条目的目标无法解析，绝不越过它继续向上攀爬。
            match symlink_metadata(path) {
                Ok(_) => Err(std::io::Error::other("existing owner path is unresolved")),
                Err(metadata_error) if metadata_error.kind() != std::io::ErrorKind::NotFound => {
                    Err(metadata_error)
                }
                Err(_) => Err(error),
            }
        }
        result => result,
    }
}

#[cfg(windows)]
fn normalize_windows(
    path: &str,
    canonicalize: impl Fn(&std::path::Path) -> std::io::Result<PathBuf>,
) -> Option<String> {
    use std::path::{Component, Prefix};
    if path.is_empty() {
        return None;
    }
    let input = std::path::Path::new(path);
    if let Some(Component::Prefix(prefix)) = input.components().next() {
        let verbatim = match prefix.kind() {
            Prefix::Disk(_) | Prefix::UNC(_, _) => false,
            Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(_, _) => true,
            Prefix::Verbatim(name) if volume_guid_namespace(name) && input.is_absolute() => true,
            _ => return None,
        };
        // verbatim 路径的遍历规则不同。绝不把字面分隔符或点组件当作
        // 普通 Win32 遍历重新解释。
        if verbatim
            && (path.contains('/') || path.split('\\').any(|part| matches!(part, "." | "..")))
        {
            return None;
        }
    }
    let absolute = std::path::absolute(input).ok()?;
    let mut lexical = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(lexical.components().next_back(), Some(Component::Normal(_))) {
                    lexical.pop();
                }
            }
            _ => lexical.push(component.as_os_str()),
        }
    }
    // 已被移除的 OTA release DLL 本身无法规范化。改为解析最深的现存祖先，
    // 使 junction 别名仍能映射到真实根。只有 NotFound 才允许补上缺失后缀；
    // 其他错误都视为未能证明。
    let resolved = canonicalize_with_existing_ancestor(&lexical, &canonicalize)?;
    Some(resolved.to_string_lossy().into_owned())
}

fn volume_guid_namespace(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let bytes = name.as_bytes();
    bytes.len() == 44
        && bytes[..7].eq_ignore_ascii_case(b"Volume{")
        && bytes[43] == b'}'
        && bytes[7..43].iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[cfg(windows)]
fn canonicalize_with_existing_ancestor(
    path: &std::path::Path,
    canonicalize: &impl Fn(&std::path::Path) -> std::io::Result<PathBuf>,
) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match canonicalize(ancestor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Some(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        let name = ancestor.file_name()?;
        if name.to_str()?.contains([':', '/', '\\']) {
            return None;
        }
        missing.push(name.to_os_string());
        ancestor = ancestor.parent()?;
    }
}

fn same_path(left: &str, right: &str) -> bool {
    same_path_using(left, right, &normalize)
}

fn same_path_using(
    left: &str,
    right: &str,
    normalize_path: &impl Fn(&str) -> Option<String>,
) -> bool {
    match (normalize_path(left), normalize_path(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn path_inside_root(path: &str, root: &str) -> bool {
    match (normalize(path), normalize(root)) {
        (Some(path), Some(root)) => path_inside_normalized_root(&path, &root),
        _ => false,
    }
}

fn path_inside_normalized_root(path: &str, root: &str) -> bool {
    !root.is_empty()
        && path.len() > root.len()
        && path.starts_with(root)
        && path.as_bytes()[root.len()] == b'\\'
}

// ---------------------------------------------------------------- 所有权记录
//
// `vcam_owned` 只存在于主机进程内存里；崩溃或 OTA 换版本后，注册表仍可能指向
// 上一次注册的 DLL。HKCU CLSID 的全局 owner 必须与本运行根的一份镜像完全一致；
// 共享 native-dir 下，DLL 路径本身不能区分运行根。镜像固定在 data/run，不随
// Legacy/Managed 布局切换。pending 先落盘、注册表后提交，任一步失败都保留与
// 全局记录匹配的旧镜像或 pending，绝不靠放宽不一致记录的判定来恢复。

const OWNER_FILE: &str = "vcam-owned.json";
const OWNER_PENDING_FILE: &str = "vcam-owned.pending.json";
const OWNER_REGISTRY_VALUE: &str = "VirtualCaptureOwner";
const OWNER_SCHEMA: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct OwnerRecord {
    schema: u32,
    dll: String,
    root: String,
    pid: u32,
    /// 进程创建时间（FILETIME，100ns），用于区分 PID 复用。
    start_time: u64,
}

fn owner_paths_at(root: &std::path::Path) -> [PathBuf; 3] {
    let stable = root.join("data/run");
    [
        stable.join(OWNER_FILE),
        stable.join(OWNER_PENDING_FILE),
        // 只读兼容 pre-journal 的 Legacy 根。不要移动 run/。
        root.join("run").join(OWNER_FILE),
    ]
}

fn read_owner_file(path: &std::path::Path) -> Option<OwnerRecord> {
    let text = std::fs::read_to_string(path).ok()?;
    let record: OwnerRecord = serde_json::from_str(&text).ok()?;
    valid_owner(&record).then_some(record)
}

fn read_local_owner_at(root: &std::path::Path, global: &OwnerRecord) -> Option<OwnerRecord> {
    owner_paths_at(root)
        .iter()
        .filter_map(|path| read_owner_file(path))
        .find(|record| same_owner(record, global))
}

fn read_local_owner(global: &OwnerRecord) -> Option<OwnerRecord> {
    read_local_owner_at(&crate::paths::root(), global)
}

fn read_registry_owner() -> Result<Option<OwnerRecord>, ()> {
    match read_registry_string(
        &format!(r"Software\Classes\CLSID\{CLSID}"),
        OWNER_REGISTRY_VALUE,
    ) {
        RegistryRead::Missing => Ok(None),
        RegistryRead::Unknown => Err(()),
        RegistryRead::Value(text) => {
            let record: OwnerRecord = serde_json::from_str(&text).map_err(|_| ())?;
            if valid_owner(&record) {
                Ok(Some(record))
            } else {
                Err(())
            }
        }
    }
}

fn valid_owner(record: &OwnerRecord) -> bool {
    valid_owner_using(record, &normalize)
}

fn valid_owner_using(
    record: &OwnerRecord,
    normalize_path: &impl Fn(&str) -> Option<String>,
) -> bool {
    record.schema == OWNER_SCHEMA
        && canonical_owner_path(&record.dll, normalize_path)
        && canonical_owner_path(&record.root, normalize_path)
        && record.pid != 0
        && record.start_time != 0
}

fn canonical_owner_path(path: &str, normalize_path: &impl Fn(&str) -> Option<String>) -> bool {
    canonical_owner_path_with_volume_lookup(path, normalize_path, &volume_guid_for_dos_root)
}

fn canonical_owner_path_with_volume_lookup(
    path: &str,
    normalize_path: &impl Fn(&str) -> Option<String>,
    volume_guid_for_root: &impl Fn(&str) -> Option<String>,
) -> bool {
    if path.is_empty() {
        return false;
    }
    let Some(normalized) = normalize_path(path) else {
        return false;
    };
    let normalized = owner_path_key(&normalized);
    if normalized == owner_path_key(path) {
        return true;
    }
    let Some(_) = volume_guid_path_parts(path) else {
        return false;
    };
    let bytes = normalized.as_bytes();
    if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return false;
    }
    let suffix_start = match bytes.get(2) {
        Some(b'\\') => 3,
        None => 2,
        _ => return false,
    };
    let dos_root = format!("{}:\\", &normalized[..1]);
    let Some(volume_root) = volume_guid_for_root(&dos_root) else {
        return false;
    };
    let Some(_) = volume_guid_path_parts(&volume_root) else {
        return false;
    };
    let suffix = &normalized[suffix_start..];
    let volume_path = if suffix.is_empty() {
        volume_root
    } else {
        format!("{}\\{suffix}", volume_root.trim_end_matches('\\'))
    };
    owner_path_key(&volume_path) == owner_path_key(path)
}

fn volume_guid_path_parts(path: &str) -> Option<()> {
    if !path.get(..4)?.eq_ignore_ascii_case(r"\\?\") {
        return None;
    }
    let name = path.get(4..48)?;
    if !volume_guid_namespace(std::ffi::OsStr::new(name)) {
        return None;
    }
    match path.as_bytes().get(48) {
        Some(b'\\') | None => Some(()),
        _ => None,
    }
}

fn volume_guid_for_dos_root(root: &str) -> Option<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;

        let root: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        let mut volume = [0u16; 128];
        // 安全：root 以 NUL 结尾，volume 可写且容量按 u16 个数传入。
        let ok = unsafe {
            GetVolumeNameForVolumeMountPointW(
                root.as_ptr(),
                volume.as_mut_ptr(),
                volume.len() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        let length = volume.iter().position(|unit| *unit == 0)?;
        String::from_utf16(&volume[..length]).ok()
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        None
    }
}

fn same_owner(left: &OwnerRecord, right: &OwnerRecord) -> bool {
    same_owner_using(left, right, &same_path)
}

fn same_owner_using(
    left: &OwnerRecord,
    right: &OwnerRecord,
    same_path: &impl Fn(&str, &str) -> bool,
) -> bool {
    left.schema == right.schema
        && same_path(&left.dll, &right.dll)
        && same_path(&left.root, &right.root)
        && left.pid == right.pid
        && left.start_time == right.start_time
}

fn write_owner(dll: &std::path::Path) -> Result<(), String> {
    // 创建时间拿不到就不能写记录：0 会让存活判断永远失败，等于把活着的属主当残留。
    let start_time = match process_liveness(std::process::id()) {
        ProcessState::Alive(time) if time != 0 => time,
        state => {
            return Err(format!(
                "无法确认本进程创建时间（{state:?}），拒绝写入虚拟卡所有权记录。"
            ));
        }
    };
    let root = std::fs::canonicalize(crate::paths::root())
        .map_err(|error| format!("无法解析虚拟卡运行根：{error}"))?;
    let dll = std::fs::canonicalize(dll).map_err(|error| format!("无法解析虚拟卡 DLL：{error}"))?;
    let record = OwnerRecord {
        schema: OWNER_SCHEMA,
        dll: dll.display().to_string(),
        root: root.display().to_string(),
        pid: std::process::id(),
        start_time,
    };
    let previous = read_registry_owner().map_err(|()| recovery_error(Recovery::Unknown))?;
    if let Some(previous) = &previous
        && (!same_path(&record.root, &previous.root) || read_local_owner(previous).is_none())
    {
        return Err(recovery_error(Recovery::Unknown));
    }
    write_owner_transaction(&root, &record, previous.as_ref(), |value| {
        write_string_value(OWNER_REGISTRY_VALUE, value)
    })
}

fn save_owner_file(path: &std::path::Path, record: &OwnerRecord) -> Result<(), String> {
    use std::io::Write;
    let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = std::fs::File::create(&temporary)
        .map_err(|error| format!("无法写入虚拟卡所有权记录：{error}"))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("无法刷盘虚拟卡所有权记录：{error}"))?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(|error| format!("无法保存虚拟卡所有权记录：{error}"))
}

/// 调用方持有 CLSID 互斥锁。回调是唯一的注册表操作；测试在提交边界注入
/// 失败，不触碰真实 COM/HKCU。
fn write_owner_transaction(
    root: &std::path::Path,
    record: &OwnerRecord,
    previous: Option<&OwnerRecord>,
    write_registry: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    let [current, pending, _legacy] = owner_paths_at(root);
    let dir = current.parent().expect("owner path has a parent");
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("无法创建运行目录 {}：{error}", dir.display()))?;
    // 先前中断的提交可能只留下 pending/Legacy 镜像。在覆盖 pending 之前，
    // 先提升它的精确全局证明，供下一次尝试使用。
    if let Some(previous) = previous {
        save_owner_file(&current, previous)?;
    }
    save_owner_file(&pending, record)?;
    let value = serde_json::to_string(record).map_err(|error| error.to_string())?;
    // 出错时保留两个镜像：current 证明旧的注册表值；若进程恰在注册表提交
    // 之后死亡，pending 证明新值。
    write_registry(&value)?;
    std::fs::rename(&pending, &current)
        .map_err(|error| format!("虚拟卡归属已提交，待提交镜像保留供重试：{error}"))
}

fn clear_owner() {
    for path in owner_paths_at(&crate::paths::root()) {
        let _ = std::fs::remove_file(path);
    }
}

fn clear_registry_owner() -> Result<(), String> {
    delete_registry_value(OWNER_REGISTRY_VALUE)
}

/// PID 查询结果：必须区分“确认不存在”和“无法查询”，后者不能当作已退出。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessState {
    Alive(u64),
    Dead,
    Unknown,
}

/// 所有权记录相对本进程的存活状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerLiveness {
    /// 记录就是本进程写的（同 PID 且同创建时间）。
    SelfProcess,
    /// 记录属主是另一个仍存活、且创建时间匹配的进程。
    LiveElsewhere,
    /// 确认原属主已退出，或 PID 已被其他进程复用。
    Dead,
    /// 无法确认属主是否存活（权限/查询失败/记录不可信）。
    Unknown,
}

#[cfg(windows)]
fn process_liveness(pid: u32) -> ProcessState {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, GetLastError};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const ERROR_INVALID_PARAMETER: u32 = 87;
    if pid == 0 {
        return ProcessState::Unknown;
    }
    // 安全：FILETIME 输出均已零初始化，句柄有效且只关闭一次；失败返回 Unknown。
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // 不存在的 PID 才是 Dead；拒绝访问等一律 Unknown，绝不猜成已退出。
            return match GetLastError() {
                ERROR_INVALID_PARAMETER => ProcessState::Dead,
                _ => ProcessState::Unknown,
            };
        }
        let mut creation = std::mem::zeroed::<FILETIME>();
        let mut exit = std::mem::zeroed::<FILETIME>();
        let mut kernel = std::mem::zeroed::<FILETIME>();
        let mut user = std::mem::zeroed::<FILETIME>();
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(handle);
        if ok == 0 {
            return ProcessState::Unknown;
        }
        let time = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        if time == 0 {
            ProcessState::Unknown
        } else {
            ProcessState::Alive(time)
        }
    }
}

#[cfg(not(windows))]
fn process_liveness(_pid: u32) -> ProcessState {
    ProcessState::Unknown
}

fn classify_owner(record: &OwnerRecord, current_pid: u32, state: ProcessState) -> OwnerLiveness {
    if record.start_time == 0 {
        return OwnerLiveness::Unknown;
    }
    match state {
        ProcessState::Alive(time) if time == record.start_time => {
            if record.pid == current_pid {
                OwnerLiveness::SelfProcess
            } else {
                OwnerLiveness::LiveElsewhere
            }
        }
        // PID 已被另一个进程复用：记录的属主已经不在了。
        ProcessState::Alive(_) | ProcessState::Dead => OwnerLiveness::Dead,
        ProcessState::Unknown => OwnerLiveness::Unknown,
    }
}

/// 当前注册相对本运行根的归属。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// 尚无注册。
    Fresh,
    /// 注册指向本目录 DLL：崩溃/重启留下的本机注册，可复用。
    Local,
    /// 注册指向本运行根上一版本的 DLL，且原属主已不在：OTA/崩溃残留，可回收。
    StaleOwned,
    /// 另一个存活主机持有该全局 CLSID 注册；不得抢占。
    Busy,
    /// 无法证明属于本运行根；不得复用或注销。
    Foreign,
    /// 所有权记录缺失/不可读，或属主存活状态无法确认；保守拒绝。
    Unknown,
}

struct RecoveryPaths<'a> {
    registered: &'a str,
    local: &'a str,
    root: &'a str,
    inside_root: bool,
}

fn decide_recovery_with_global_owner(
    registered: &str,
    local: &str,
    root: &str,
    local_owner: Option<&OwnerRecord>,
    registry_owner: Option<&OwnerRecord>,
    owner_liveness: OwnerLiveness,
    inside_root: bool,
) -> Recovery {
    decide_recovery_with_global_owner_using(
        RecoveryPaths {
            registered,
            local,
            root,
            inside_root,
        },
        local_owner,
        registry_owner,
        owner_liveness,
        &normalize,
    )
}

fn decide_recovery_with_global_owner_using(
    paths: RecoveryPaths<'_>,
    local_owner: Option<&OwnerRecord>,
    registry_owner: Option<&OwnerRecord>,
    owner_liveness: OwnerLiveness,
    normalize_path: &impl Fn(&str) -> Option<String>,
) -> Recovery {
    let same_path = |left: &str, right: &str| same_path_using(left, right, normalize_path);
    let RecoveryPaths {
        registered,
        local,
        root,
        inside_root,
    } = paths;
    let Some(registry_owner) = registry_owner else {
        return if registered.is_empty() {
            Recovery::Fresh
        } else if same_path(registered, local) {
            Recovery::Unknown
        } else {
            Recovery::Foreign
        };
    };
    if !valid_owner_using(registry_owner, normalize_path)
        || (!registered.is_empty() && !same_path(registered, &registry_owner.dll))
    {
        return Recovery::Unknown;
    }

    // 注册表记录对每个用户的 CLSID 是全局的。先查它，再看任何 run-root
    // 本地记录，这样其他根无法占用共享 DLL。
    if !same_path(root, &registry_owner.root) {
        return match owner_liveness {
            OwnerLiveness::LiveElsewhere => Recovery::Busy,
            OwnerLiveness::SelfProcess | OwnerLiveness::Dead => Recovery::Foreign,
            OwnerLiveness::Unknown => Recovery::Unknown,
        };
    }

    let Some(local_owner) = local_owner else {
        return Recovery::Unknown;
    };
    if !same_owner_using(registry_owner, local_owner, &same_path) {
        return Recovery::Unknown;
    }
    if owner_liveness == OwnerLiveness::LiveElsewhere {
        return Recovery::Busy;
    }
    if owner_liveness == OwnerLiveness::Unknown {
        return Recovery::Unknown;
    }
    if registered.is_empty() {
        return Recovery::Fresh;
    }
    if same_path(registered, local) {
        return Recovery::Local;
    }
    if !inside_root {
        return Recovery::Foreign;
    }
    // 上一版本残留；本进程自己写下的旧 release 记录同样可以回收。
    Recovery::StaleOwned
}

fn current_recovery(status: &VcamStatus) -> Recovery {
    if status.registration_unknown {
        return Recovery::Unknown;
    }
    let root = crate::paths::root();
    let registry_owner = match read_registry_owner() {
        Ok(record) => record,
        Err(()) => return Recovery::Unknown,
    };
    let local_owner = registry_owner.as_ref().and_then(read_local_owner);
    let owner_liveness = registry_owner
        .as_ref()
        .map(|record| classify_owner(record, std::process::id(), process_liveness(record.pid)))
        .unwrap_or(OwnerLiveness::Unknown);
    let inside_root = path_inside_root(&status.registered, &root.to_string_lossy());
    decide_recovery_with_global_owner(
        &status.registered,
        &status.dll,
        &root.to_string_lossy(),
        local_owner.as_ref(),
        registry_owner.as_ref(),
        owner_liveness,
        inside_root,
    )
}

/// 供主机控制器区分：Unknown 时必须保留 `vcam_owned` 并拒绝注销。
pub(crate) fn recovery(status: &VcamStatus) -> Recovery {
    current_recovery(status)
}

pub(crate) fn recovery_error(recovery: Recovery) -> String {
    match recovery {
        Recovery::Busy => "虚拟卡正由另一台存活的主机使用；请先关闭那台主机。".into(),
        Recovery::Foreign => {
            "虚拟采集卡已注册到其他目录；先关闭并停用已有主机，避免覆盖稳定部署。".into()
        }
        Recovery::Unknown => {
            "无法证明虚拟卡注册属于本运行根（所有权记录缺失、不可读或属主状态未知）；\
             请确认没有其他主机在使用后手工清理注册。"
                .into()
        }
        _ => "虚拟采集卡注册状态不可复用。".into(),
    }
}

/// 启动前只读检查：只在“无注册 / 可证明属于本根的本目录注册 /
/// 可证明属于本根的已停止残留”三种情况下放行；其他目录、存活主机或
/// 无法证明的注册一律拒绝，不静默接管。这里只是预检；register 会在
/// 跨进程注册锁内重新判定一次。
pub fn check_available(status: &VcamStatus) -> Result<(), String> {
    match current_recovery(status) {
        Recovery::Fresh | Recovery::Local | Recovery::StaleOwned => Ok(()),
        recovery => Err(recovery_error(recovery)),
    }
}

pub const PROBE_MODE: &str = "__vcam-probe";

/// 隔离子进程只读探测实际 DLL，不注册 COM 或接管现用设备。
pub fn require_capabilities() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位主程序：{e}"))?;
    let output = crate::process::run_bounded(
        std::process::Command::new(exe)
            .arg(PROBE_MODE)
            .arg("--dll")
            .arg(dll_path()),
        std::time::Duration::from_secs(5),
    )?;
    if !output.status.success() {
        return Err(format!(
            "虚拟卡能力探测失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let caps: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("虚拟卡能力响应无效：{e}"))?;
    if caps["stream_revision"] != 2
        || caps["variable_resolution"] != true
        || caps["shm_version"] != 3
    {
        return Err("虚拟卡缺少 stream_revision=2 / 可变尺寸 / SHM v3 能力。".into());
    }
    Ok(())
}

// ---------------------------------------------------------------- 跨运行根互斥
//
// CLSID 写在本用户的 HKCU，是整机唯一的注册资源；运行根目录里的锁文件挡不住
// 另一个安装目录（另一个运行根）的主机。register/unregister 的“检查 → 写记录
// → regsvr → 复核”必须在同一把跨进程锁内完成，否则两个主机可能同时通过检查
// 并互相覆盖注册。锁拿不到时宁可失败，也不做没有串行化的注册变更。

/// 锁名与运行根无关：不同 release/运行根的主机必须抢同一把锁。
fn registration_mutex_name() -> String {
    format!(r"Global\VirtualCapture.VcamClsid.{CLSID}")
}

/// 等待上限覆盖最坏情况的一次注册变更（regsvr 超时 10s，最多两段）。
const REGISTRATION_LOCK_TIMEOUT_MS: u32 = 30_000;

#[cfg(windows)]
struct RegistrationLock {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl RegistrationLock {
    fn acquire(name: &str, timeout_ms: u32) -> Result<Self, String> {
        use windows_sys::Win32::Foundation::{
            CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        };
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let name = wide(name);
        // 安全：name 是以 NUL 结尾的 UTF-16 缓冲区；句柄在成功时移交本对象，
        // 失败路径则立即关闭。
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(format!(
                "无法创建虚拟卡注册互斥体（{}）；为避免与其他主机并发覆盖注册，拒绝本次操作。",
                std::io::Error::last_os_error()
            ));
        }
        match unsafe { WaitForSingleObject(handle, timeout_ms) } {
            // 上一属主崩溃时互斥体被遗弃；随后的 status/恢复检查会重新判定归属。
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { handle }),
            WAIT_TIMEOUT => {
                unsafe { CloseHandle(handle) };
                Err("另一台主机的虚拟卡注册变更仍未结束；为避免并发覆盖注册，拒绝本次操作，请稍后重试。".into())
            }
            _ => {
                let error = std::io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                Err(format!(
                    "等待虚拟卡注册互斥体失败（{error}）；为避免并发覆盖注册，拒绝本次操作。"
                ))
            }
        }
    }
}

#[cfg(windows)]
impl Drop for RegistrationLock {
    fn drop(&mut self) {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::ReleaseMutex};
        // 安全：句柄由 acquire 创建且只由本对象持有；释放后立即关闭，不再使用。
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(not(windows))]
struct RegistrationLock;

#[cfg(not(windows))]
impl RegistrationLock {
    fn acquire(_name: &str, _timeout_ms: u32) -> Result<Self, String> {
        Err("虚拟采集卡仅支持 Windows。".into())
    }
}

/// 拿不到锁就失败：注册变更不能在没有串行化的情况下继续。
fn lock_registration() -> Result<RegistrationLock, String> {
    RegistrationLock::acquire(&registration_mutex_name(), REGISTRATION_LOCK_TIMEOUT_MS)
}

/// 注册本目录 DLL，并写入产品固定 FrameMap/Motion。
///
/// 崩溃重启后可复用指向本目录 DLL 的既有注册；OTA 后如注册仍指向本运行根
/// 上一版本且原属主已退出，则先回收该残留再注册。其他目录或存活主机的注册
/// 一律拒绝，绝不接管/注销。整个“检查 → 写记录 → regsvr → 复核”序列在
/// 全机互斥体内完成，避免其他运行根的主机同时通过检查。
pub fn register(shm: &str) -> Result<VcamStatus, String> {
    let dll = dll_path();
    if !dll.exists() {
        return Err(format!("找不到 vc_vcam.dll：{}", dll.display()));
    }
    let _guard = lock_registration()?;
    let current = status();
    match current_recovery(&current) {
        Recovery::Fresh | Recovery::Local => {}
        Recovery::StaleOwned => unregister_stale_owned()?,
        recovery => return Err(recovery_error(recovery)),
    }
    // 先写所有权记录再注册：这样即便 regsvr/FrameMap 失败，后续清理也能
    // 证明这条同目录注册是本进程刚写的，而不是另一台主机的。
    write_owner(&dll)?;
    regsvr_register(&dll)?;
    write_string_value("FrameMap", shm)?;
    write_dword_value("Motion", 0)?;
    let status = status();
    if status.registration_unknown {
        return Err(recovery_error(Recovery::Unknown));
    }
    if !status.match_local {
        return Err(format!(
            "注册完成但 InprocServer32 未指向本目录 DLL（当前：{}）。",
            status.registered
        ));
    }
    if current_recovery(&status) != Recovery::Local {
        return Err(recovery_error(current_recovery(&status)));
    }
    Ok(status)
}

/// 只回收本目录匹配的注册，不碰其他目录。
///
/// 本运行根上一版本、且所有权记录证明属主已退出的残留也允许回收（OTA 清理）；
/// 存活主机或其他目录的注册仍然拒绝。整个“检查 → regsvr /u → 复核 → 清记录”
/// 序列在全机互斥体内完成，避免与其他运行根的主机并发覆盖。
pub fn unregister() -> Result<VcamStatus, String> {
    let _guard = lock_registration()?;
    let current = status();
    if current.registration_unknown {
        return Err(recovery_error(Recovery::Unknown));
    }
    if current.registered.is_empty() {
        if let Some(owner) = read_registry_owner()
            .map_err(|()| recovery_error(Recovery::Unknown))?
            .filter(|owner| same_path(&owner.root, &crate::paths::root().to_string_lossy()))
        {
            let local_owner =
                read_local_owner(&owner).ok_or_else(|| recovery_error(Recovery::Unknown))?;
            let liveness = classify_owner(&owner, std::process::id(), process_liveness(owner.pid));
            if !same_owner(&owner, &local_owner)
                || !matches!(liveness, OwnerLiveness::SelfProcess | OwnerLiveness::Dead)
            {
                return Err(recovery_error(match liveness {
                    OwnerLiveness::LiveElsewhere => Recovery::Busy,
                    _ => Recovery::Unknown,
                }));
            }
            clear_registry_owner()?;
        }
        clear_owner();
        return Ok(current);
    }
    match current_recovery(&current) {
        Recovery::Local | Recovery::StaleOwned => {}
        recovery => return Err(recovery_error(recovery)),
    }
    regsvr_unregister()?;
    let after = status();
    if after.registration_unknown {
        return Err(recovery_error(Recovery::Unknown));
    }
    if !after.registered.is_empty() {
        return Err("注销后虚拟采集卡注册仍存在，需重试停止。".into());
    }
    // DllUnregisterServer 删除了整个 CLSID 键，包括其全局 owner 值。
    clear_owner();
    Ok(after)
}

fn regsvr_path() -> String {
    std::env::var("SystemRoot")
        .map(|root| format!("{root}\\System32\\regsvr32.exe"))
        .unwrap_or_else(|_| r"C:\Windows\System32\regsvr32.exe".into())
}

fn regsvr_register(dll: &std::path::Path) -> Result<(), String> {
    let output = crate::process::run_bounded(
        std::process::Command::new(regsvr_path()).arg("/s").arg(dll),
        std::time::Duration::from_secs(10),
    )?;
    if !output.status.success() {
        return Err(format!(
            "regsvr32 注册失败（退出码 {:?}）：{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn regsvr_unregister() -> Result<(), String> {
    let output = crate::process::run_bounded(
        std::process::Command::new(regsvr_path())
            .args(["/s", "/u"])
            .arg(dll_path()),
        std::time::Duration::from_secs(10),
    )?;
    if !output.status.success() {
        return Err(format!(
            "regsvr32 注销失败（退出码 {:?}）。",
            output.status.code()
        ));
    }
    Ok(())
}

/// 回收已证明属于本运行根、且原属主已退出的上一版本注册。
///
/// 只由 `register` 在持有注册锁时调用；不要单独加锁，否则会改变调用者的序列。
fn unregister_stale_owned() -> Result<(), String> {
    let recovery = current_recovery(&status());
    if recovery != Recovery::StaleOwned {
        return Err(recovery_error(recovery));
    }
    // DllUnregisterServer 删除共享 CLSID 键，与注册时使用的 release 无关；
    // 用当前 DLL 执行即可，不加载/执行上一版本二进制。
    regsvr_unregister()?;
    let after = status();
    if after.registration_unknown {
        return Err(recovery_error(Recovery::Unknown));
    }
    if !after.registered.is_empty() {
        return Err("旧版本虚拟卡注册未能清除，已停止启动；请手动检查注册表。".into());
    }
    // CLSID owner 值随服务器注册一起被删除。
    clear_owner();
    Ok(())
}

/// 注册表读取结果：读失败不能当成“未注册”，否则会静默覆盖别人的注册。
enum RegistryRead {
    Missing,
    Value(String),
    Unknown,
}

#[cfg(windows)]
fn read_registry_string(subkey: &str, value_name: &str) -> RegistryRead {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    let subkey = wide(subkey);
    let mut key = std::ptr::null_mut();
    let opened =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut key) };
    if opened != 0 {
        return if opened == ERROR_FILE_NOT_FOUND {
            RegistryRead::Missing
        } else {
            RegistryRead::Unknown
        };
    }
    let name = wide(value_name);
    let mut kind = 0u32;
    let mut size = 0u32;
    let query = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if query != 0 {
        unsafe { RegCloseKey(key) };
        return if query == ERROR_FILE_NOT_FOUND {
            RegistryRead::Missing
        } else {
            RegistryRead::Unknown
        };
    }
    if kind != REG_SZ || size == 0 {
        unsafe { RegCloseKey(key) };
        return RegistryRead::Unknown;
    }
    let mut buffer = vec![0u16; (size / 2) as usize + 1];
    let mut size = (buffer.len() * 2) as u32;
    // 安全：传入的缓冲区可写、按 u16 对齐，容量以字节数告知；RegQueryValueExW
    // 最多写入该容量。
    let query = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            buffer.as_mut_ptr() as *mut u8,
            &mut size,
        )
    };
    unsafe { RegCloseKey(key) };
    if query != 0 {
        return RegistryRead::Unknown;
    }
    let value = from_wide(&buffer);
    if value.is_empty() {
        RegistryRead::Unknown
    } else {
        RegistryRead::Value(value)
    }
}

#[cfg(windows)]
fn read_registered_dll() -> RegistryRead {
    read_registry_string(
        &format!(r"Software\Classes\CLSID\{CLSID}\InprocServer32"),
        "",
    )
}

#[cfg(not(windows))]
fn read_registry_string(_subkey: &str, _value_name: &str) -> RegistryRead {
    RegistryRead::Missing
}

#[cfg(not(windows))]
fn read_registered_dll() -> RegistryRead {
    RegistryRead::Missing
}

#[cfg(windows)]
fn write_string_value(name: &str, value: &str) -> Result<(), String> {
    let wide = wide(value);
    // 安全：wide 在调用期间保持存活；按 len * 2 字节读取 u16 缓冲区，u8 视图
    // 没有对齐要求。
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    write_value(name, bytes, 1)
}

#[cfg(windows)]
fn write_dword_value(name: &str, value: u32) -> Result<(), String> {
    write_value(name, &value.to_le_bytes(), 4)
}

#[cfg(windows)]
fn write_value(name: &str, bytes: &[u8], kind: u32) -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegCreateKeyExW, RegSetValueExW,
    };
    let subkey = wide(&format!(r"Software\Classes\CLSID\{CLSID}"));
    let mut key = std::ptr::null_mut();
    let created = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if created != 0 {
        return Err(format!("无法打开虚拟卡注册表键（错误 {created}）。"));
    }
    let name = wide(name);
    let result = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            kind,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    if result != 0 {
        return Err(format!("写入虚拟卡注册表值失败（错误 {result}）。"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn write_string_value(_name: &str, _value: &str) -> Result<(), String> {
    Err("虚拟采集卡仅支持 Windows。".into())
}

#[cfg(not(windows))]
fn write_dword_value(_name: &str, _value: u32) -> Result<(), String> {
    Err("虚拟采集卡仅支持 Windows。".into())
}

#[cfg(windows)]
fn delete_registry_value(name: &str) -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    };
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    let subkey = wide(&format!(r"Software\Classes\CLSID\{CLSID}"));
    let mut key = std::ptr::null_mut();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut key,
        )
    };
    if opened == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if opened != 0 {
        return Err(format!("无法打开虚拟卡所有权注册表键（错误 {opened}）。"));
    }
    let name = wide(name);
    let removed = unsafe { RegDeleteValueW(key, name.as_ptr()) };
    unsafe { RegCloseKey(key) };
    if removed == 0 || removed == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("无法清除虚拟卡所有权注册表值（错误 {removed}）。"))
    }
}

#[cfg(not(windows))]
fn delete_registry_value(_name: &str) -> Result<(), String> {
    Err("虚拟采集卡仅支持 Windows。".into())
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn from_wide(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_at(dll: &str, root: &str, pid: u32) -> OwnerRecord {
        OwnerRecord {
            schema: OWNER_SCHEMA,
            dll: dll.into(),
            root: root.into(),
            pid,
            start_time: 5678,
        }
    }

    fn decide_recovery(
        registered: &str,
        local: &str,
        owner: Option<&OwnerRecord>,
        owner_liveness: OwnerLiveness,
        inside_root: bool,
    ) -> Recovery {
        let root = owner.map(|record| record.root.as_str()).unwrap_or("");
        decide_recovery_with_global_owner(
            registered,
            local,
            root,
            owner,
            owner,
            owner_liveness,
            inside_root,
        )
    }

    struct OwnerTestRoot(PathBuf);

    impl OwnerTestRoot {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "vc-owner-test-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(std::fs::canonicalize(root).unwrap())
        }

        fn path(&self, relative: &str) -> String {
            self.0.join(relative).to_string_lossy().into_owned()
        }

        fn owner(&self, relative: &str, pid: u32) -> OwnerRecord {
            owner_at(&self.path(relative), &self.0.to_string_lossy(), pid)
        }

        fn record(&self, pid: u32) -> OwnerRecord {
            self.owner("app/releases/b1/vc_vcam.dll", pid)
        }
    }

    impl Drop for OwnerTestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn owner_proof_survives_first_activation_and_layout_rollback() {
        let root = OwnerTestRoot::new();
        let previous = root.record(1234);
        let next = root.record(2222);
        let [current, _pending, legacy] = owner_paths_at(&root.0);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        save_owner_file(&legacy, &previous).unwrap();
        assert_eq!(
            crate::paths::layout_at(&root.0),
            crate::paths::Layout::Legacy
        );
        assert_eq!(
            read_local_owner_at(&root.0, &previous),
            Some(previous.clone())
        );

        let pointer = root.0.join("app/current.json");
        std::fs::create_dir_all(pointer.parent().unwrap()).unwrap();
        std::fs::write(&pointer, r#"{"schema":1,"build_id":"b2"}"#).unwrap();
        assert_eq!(
            crate::paths::layout_at(&root.0),
            crate::paths::Layout::Managed
        );
        assert_eq!(owner_paths_at(&root.0)[0], current);
        assert_eq!(
            read_local_owner_at(&root.0, &previous),
            Some(previous.clone())
        );
        write_owner_transaction(&root.0, &next, Some(&previous), |_| Ok(())).unwrap();

        std::fs::remove_file(pointer).unwrap();
        assert_eq!(
            crate::paths::layout_at(&root.0),
            crate::paths::Layout::Legacy
        );
        assert_eq!(read_local_owner_at(&root.0, &next), Some(next));
        assert_eq!(
            read_owner_file(&legacy),
            Some(previous),
            "do not move old run evidence"
        );
    }

    #[test]
    fn failed_registry_commit_keeps_the_previous_local_proof() {
        let root = OwnerTestRoot::new();
        let previous = root.record(1234);
        let next = root.record(2222);
        write_owner_transaction(&root.0, &previous, None, |_| Ok(())).unwrap();
        let error = write_owner_transaction(&root.0, &next, Some(&previous), |_| {
            Err("injected registry write failure".into())
        })
        .unwrap_err();
        assert!(error.contains("injected"));
        let [current, pending, _legacy] = owner_paths_at(&root.0);
        assert_eq!(read_owner_file(&current), Some(previous.clone()));
        assert_eq!(read_owner_file(&pending), Some(next));
        assert_eq!(read_local_owner_at(&root.0, &previous), Some(previous));
    }

    #[test]
    fn interrupted_commit_and_its_retry_keep_a_matching_proof() {
        let root = OwnerTestRoot::new();
        let previous = root.record(1234);
        let next = root.record(2222);
        let retry = root.record(3333);
        write_owner_transaction(&root.0, &previous, None, |_| Ok(())).unwrap();
        let mut global = previous.clone();
        // 模拟全局写成功后、最终 rename 之前进程立即死亡。
        // 这些测试不执行任何真实注册表操作。
        write_owner_transaction(&root.0, &next, Some(&previous), |value| {
            global = serde_json::from_str(value).unwrap();
            Err("injected interruption after global commit".into())
        })
        .unwrap_err();
        assert_eq!(read_local_owner_at(&root.0, &global), Some(next.clone()));
        let [current, pending, _legacy] = owner_paths_at(&root.0);
        assert_eq!(read_owner_file(&current), Some(previous));
        assert_eq!(read_owner_file(&pending), Some(next.clone()));

        // 重试必须首先提升权威的 pending 记录。若在其注册表写入前再次中断，
        // 也不能销毁这份证明。
        write_owner_transaction(&root.0, &retry, Some(&next), |_| {
            assert_eq!(read_owner_file(&current), Some(next.clone()));
            Err("injected retry failure before global commit".into())
        })
        .unwrap_err();
        assert_eq!(read_local_owner_at(&root.0, &global), Some(next));
        assert_eq!(read_owner_file(&pending), Some(retry));
    }

    #[test]
    fn failed_final_rename_leaves_the_committed_pending_proof_readable() {
        let root = OwnerTestRoot::new();
        let record = root.record(2222);
        let [current, pending, _legacy] = owner_paths_at(&root.0);
        let mut global = None;
        write_owner_transaction(&root.0, &record, None, |value| {
            global = Some(serde_json::from_str::<OwnerRecord>(value).unwrap());
            // 目标是目录，使最终 rename 失败。
            std::fs::create_dir(&current).unwrap();
            Ok(())
        })
        .unwrap_err();
        assert_eq!(read_owner_file(&pending), global);
        assert_eq!(
            read_local_owner_at(&root.0, global.as_ref().unwrap()),
            global
        );
    }

    #[test]
    fn missing_corrupt_or_foreign_local_files_never_prove_ownership() {
        let root = OwnerTestRoot::new();
        let global = root.record(2222);
        let [current, _pending, _legacy] = owner_paths_at(&root.0);
        assert_eq!(read_local_owner_at(&root.0, &global), None);
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, "broken JSON").unwrap();
        assert_eq!(read_local_owner_at(&root.0, &global), None);
        let foreign_root = OwnerTestRoot::new();
        let foreign = OwnerRecord {
            root: foreign_root.0.to_string_lossy().into_owned(),
            ..global.clone()
        };
        save_owner_file(&current, &foreign).unwrap();
        assert_eq!(read_local_owner_at(&root.0, &global), None);
        let mismatched_pid = OwnerRecord {
            pid: global.pid + 1,
            ..global.clone()
        };
        save_owner_file(&current, &mismatched_pid).unwrap();
        assert_eq!(read_local_owner_at(&root.0, &global), None);
    }

    #[cfg(windows)]
    #[test]
    fn deleted_alias_dll_recovers_against_its_canonical_owner_record() {
        use std::path::Path;

        let root = OwnerTestRoot::new();
        let real_root = root.0.clone();
        std::fs::create_dir_all(root.0.join("app/releases/b2")).unwrap();
        // 私有 fixture 内的唯一缺失别名。绝不用固定主机路径（例如本机
        // 可能存在的 C:\Alias）。
        let alias_prefix = root.0.join("missing-alias");
        let alias_root = alias_prefix.to_string_lossy().into_owned();
        let registered = alias_prefix
            .join("app/releases/b1/vc_vcam.dll")
            .to_string_lossy()
            .into_owned();
        let local = root
            .0
            .join("app/releases/b2/vc_vcam.dll")
            .to_string_lossy()
            .into_owned();
        let expected_dll = real_root
            .join("app/releases/b1/vc_vcam.dll")
            .to_string_lossy()
            .into_owned();
        let expected_root = real_root.to_string_lossy().into_owned();
        let record = owner_at(&expected_dll, &expected_root, 1234);
        // 同一个 owner 用别名拼写序列化：只能通过注入的比较判定相等，
        // 绝不可能通过全局规范化器。
        let alias_record = owner_at(&registered, &alias_root, 1234);
        let canonicalize_alias = |candidate: &Path| -> std::io::Result<PathBuf> {
            if let Ok(relative) = candidate.strip_prefix(&alias_prefix) {
                std::fs::canonicalize(real_root.join(relative))
            } else {
                std::fs::canonicalize(candidate)
            }
        };
        let normalize_alias = |path: &str| {
            normalize_windows(path, canonicalize_alias).map(|resolved| owner_path_key(&resolved))
        };
        assert!(!same_owner(&record, &alias_record));
        assert!(same_owner_using(
            &record,
            &alias_record,
            &|left: &str, right: &str| same_path_using(left, right, &normalize_alias)
        ));
        assert_eq!(
            decide_recovery_with_global_owner(
                &registered,
                &local,
                &expected_root,
                Some(&record),
                Some(&record),
                OwnerLiveness::Dead,
                true,
            ),
            Recovery::Unknown,
            "without resolving the alias, the global owner mismatch fails closed"
        );
        assert_eq!(
            decide_recovery_with_global_owner_using(
                RecoveryPaths {
                    registered: &registered,
                    local: &local,
                    root: &alias_root,
                    inside_root: true,
                },
                Some(&alias_record),
                Some(&record),
                OwnerLiveness::Dead,
                &normalize_alias,
            ),
            Recovery::StaleOwned,
            "resolving the alias and using the same comparison restores recovery"
        );
        let normalized_registered = normalize_alias(&registered).unwrap();
        let normalized_root = normalize_alias(&alias_root).unwrap();
        assert!(valid_owner_using(&record, &normalize_alias));
        assert!(path_inside_normalized_root(
            &normalized_registered,
            &normalized_root
        ));
        assert!(!path_inside_normalized_root(
            &format!("{}2\\vc_vcam.dll", normalized_root),
            &normalized_root
        ));
    }

    #[cfg(windows)]
    #[test]
    fn unresolved_alias_path_errors_do_not_fall_back_to_lexical_ownership() {
        use std::path::Path;

        let path = r"Q:\MissingAlias\app\old\vc_vcam.dll";
        let resolved = normalize_windows(path, |candidate| {
            if candidate == Path::new(path) || candidate == Path::new(r"Q:\MissingAlias\app\old") {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            } else {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            }
        });
        assert_eq!(resolved, None);
    }

    #[cfg(windows)]
    #[test]
    fn equivalent_root_spellings_recover_the_same_registration() {
        let root = OwnerTestRoot::new();
        let dll = root.path("app/releases/b1/vc_vcam.dll");
        let canonical_root = root.0.to_string_lossy().into_owned();
        let plain_root = canonical_root
            .strip_prefix(r"\\?\UNC\")
            .map(|unc| format!(r"\\{unc}"))
            .or_else(|| {
                canonical_root
                    .strip_prefix(r"\\?\")
                    .filter(|plain| plain.as_bytes().get(1..3) == Some(b":\\"))
                    .map(str::to_owned)
            });
        let record = owner_at(&dll, &canonical_root, 1234);
        let equivalent = match plain_root.as_deref() {
            Some(plain) if plain.starts_with(r"\\") => [
                canonical_root.clone(),
                format!(r"{plain}\child\.."),
                format!(r"{plain}\."),
            ],
            Some(plain) => [
                format!(r"\\?\{plain}"),
                format!(r"{plain}\child\.."),
                format!(r"{plain}\."),
            ],
            None => [
                canonical_root.clone(),
                canonical_root.clone(),
                canonical_root.clone(),
            ],
        };
        for alias_root in &equivalent {
            assert!(same_path(alias_root, &record.root), "{alias_root}");
            assert_eq!(
                decide_recovery_with_global_owner(
                    &dll,
                    &dll,
                    alias_root,
                    Some(&record),
                    Some(&record),
                    OwnerLiveness::Dead,
                    true,
                ),
                Recovery::Local,
                "{alias_root}",
            );
        }
        if let Some(plain_root) = plain_root {
            // verbatim 遍历是字面语义，不是普通 Win32 遍历。
            let verbatim_traversal = format!(r"{}\child\..", equivalent[0]);
            assert_eq!(normalize(&verbatim_traversal), None, "{verbatim_traversal}");
            let outside = format!(r"{plain_root}2\.");
            assert!(!same_path(&outside, &record.root));
            assert!(!path_inside_root(&dll, &outside));
        }
    }

    #[test]
    fn owner_records_must_use_canonical_path_spellings() {
        let record = owner_at(r"C:\Alias\app\vc_vcam.dll", r"C:\Alias", 1234);
        let canonicalize_alias = |path: &str| {
            let canonical = path
                .strip_prefix(r"C:\Alias")
                .map(|suffix| format!(r"D:\Root{suffix}"))
                .unwrap_or_else(|| path.to_owned());
            Some(canonical.replace('/', "\\").to_ascii_lowercase())
        };
        assert!(!valid_owner_using(&record, &canonicalize_alias));

        let canonical_record = owner_at(r"D:\Root\app\vc_vcam.dll", r"D:\Root", 1234);
        assert!(valid_owner_using(&canonical_record, &canonicalize_alias));
    }

    #[test]
    fn volume_guid_owner_accepts_only_same_volume_and_same_path() {
        let volume = r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\";
        let owner_path = format!(r"{volume}app\vc_vcam.dll");
        let normalize_drive = |_: &str| Some(owner_path_key(r"\\?\C:\app\vc_vcam.dll"));
        let same_volume = |root: &str| root.eq_ignore_ascii_case(r"C:\").then(|| volume.to_owned());
        assert!(canonical_owner_path_with_volume_lookup(
            &owner_path,
            &normalize_drive,
            &same_volume,
        ));

        let wrong_volume =
            |_: &str| Some(r"\\?\Volume{aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa}\".to_owned());
        assert!(!canonical_owner_path_with_volume_lookup(
            &owner_path,
            &normalize_drive,
            &wrong_volume,
        ));

        let wrong_path = |_: &str| Some(owner_path_key(r"\\?\C:\other\vc_vcam.dll"));
        assert!(!canonical_owner_path_with_volume_lookup(
            &owner_path,
            &wrong_path,
            &same_volume,
        ));
    }

    #[cfg(windows)]
    #[test]
    fn real_volume_guid_owner_round_trips_after_drive_canonicalization() {
        let executable = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let drive_path = owner_path_key(&executable.display().to_string());
        let drive_root = format!("{}:\\", &drive_path[..1]);
        let volume_root = volume_guid_for_dos_root(&drive_root).expect("current volume GUID");
        let relative = drive_path
            .strip_prefix(&drive_root)
            .expect("executable on drive");
        let root = format!("{volume_root}VirtualCapture-owner-path-test");
        let dll = format!("{root}\\app\\releases\\b1\\vc_vcam.dll");
        let local = format!("{root}\\app\\releases\\b2\\vc_vcam.dll");
        let registered = owner_at(&dll, &root, 1234);
        let normalize_drive_tree = |path: &str| {
            let suffix = path.strip_prefix(&root).unwrap_or("");
            Some(owner_path_key(&format!(
                r"\\?\{drive_root}VirtualCapture-owner-path-test{suffix}"
            )))
        };
        assert!(valid_owner(&owner_at(
            &format!("{volume_root}{relative}"),
            &volume_root,
            1234,
        )));
        assert_eq!(
            decide_recovery_with_global_owner_using(
                RecoveryPaths {
                    registered: &dll,
                    local: &local,
                    root: &root,
                    inside_root: true,
                },
                Some(&registered),
                Some(&registered),
                OwnerLiveness::Dead,
                &normalize_drive_tree,
            ),
            Recovery::StaleOwned
        );
    }

    #[cfg(windows)]
    #[test]
    fn owner_normalization_rejects_unrecognized_verbatim_namespaces() {
        use std::cell::Cell;

        for path in [
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\app\vc_vcam.dll",
            r"\\?\PIPE\vc-owner",
            r"\\?\Device\Harddisk0\DR0",
            r"\\.\pipe\vc-owner",
            r"\\?\Volume{not-a-guid}\app\vc_vcam.dll",
            r"\\?\Volume{12345678-1234-1234-1234}\app\vc_vcam.dll",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc",
            r"\\?\Volume{zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz}\app\vc_vcam.dll",
            r"\\?\Volume{1234-56781234-1234-1234-123456789abc}\app\vc_vcam.dll",
        ] {
            let calls = Cell::new(0);
            let resolved = normalize_windows(path, |_| {
                calls.set(calls.get() + 1);
                Ok(PathBuf::from(r"\\?\C:\unexpected"))
            });
            assert_eq!(resolved, None, "{path}");
            assert_eq!(calls.get(), 0, "{path}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn owner_normalization_rejects_verbatim_traversal_spellings() {
        for path in [
            r"\\?\C:\app/vc_vcam.dll",
            r"\\?\C:\app\..\vc_vcam.dll",
            r"\\?\C:\app\.\vc_vcam.dll",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\..\vc_vcam.dll",
        ] {
            let resolved = normalize_windows(path, |_| {
                panic!("canonicalize must not run for verbatim traversal: {path}")
            });
            assert_eq!(resolved, None, "{path}");
        }
    }

    #[test]
    fn volume_namespace_stays_part_of_the_owner_key() {
        let volume_key =
            owner_path_key(r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\app\vc_vcam.dll");
        assert!(volume_key.starts_with(r"\\?\volume{"), "{volume_key}");
        let relative_key =
            owner_path_key(r"Volume{12345678-1234-1234-1234-123456789abc}\app\vc_vcam.dll");
        assert!(!relative_key.starts_with(r"\\?\"), "{relative_key}");
        assert_ne!(volume_key, relative_key);
        // DOS 盘符仍会去掉 verbatim 前缀，使不同拼写比较相等。
        assert_eq!(
            owner_path_key(r"\\?\C:\Root\vc_vcam.dll"),
            owner_path_key(r"C:\Root\vc_vcam.dll")
        );
    }

    #[cfg(windows)]
    #[test]
    fn canonicalize_for_owner_rejects_dangling_existing_entries() {
        use std::path::Path;

        let path = Path::new(r"Q:\VirtualCapture\owner\vc_vcam.dll");
        let error = canonicalize_for_owner_using(
            path,
            |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_ne!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "a dangling existing entry must stay unresolved"
        );
    }

    #[cfg(windows)]
    #[test]
    fn dangling_or_unreadable_entries_do_not_climb_to_an_existing_ancestor() {
        use std::cell::Cell;
        use std::path::Path;

        let path = r"Q:\VirtualCapture\owner\vc_vcam.dll";
        for (tag, metadata_kind) in [
            ("dangling", None),
            ("access-denied", Some(std::io::ErrorKind::PermissionDenied)),
        ] {
            let canonicalize_calls = Cell::new(0);
            let metadata_calls = Cell::new(0);
            let canonicalize = |candidate: &Path| {
                canonicalize_calls.set(canonicalize_calls.get() + 1);
                assert_eq!(candidate, Path::new(path), "{tag}");
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            };
            let metadata = |candidate: &Path| {
                metadata_calls.set(metadata_calls.get() + 1);
                assert_eq!(candidate, Path::new(path), "{tag}");
                match metadata_kind {
                    None => Ok(()),
                    Some(kind) => Err(std::io::Error::from(kind)),
                }
            };
            let normalized = normalize_windows(path, |candidate| {
                canonicalize_for_owner_using(candidate, canonicalize, metadata)
            });
            assert_eq!(normalized, None, "{tag} must not climb");
            assert_eq!(canonicalize_calls.get(), 1, "{tag}");
            assert_eq!(metadata_calls.get(), 1, "{tag}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn truly_missing_entries_may_climb_to_their_existing_ancestor() {
        use std::cell::Cell;
        use std::path::Path;

        let path = r"Q:\VirtualCapture\owner\releases\b1\vc_vcam.dll";
        let existing = Path::new(r"Q:\VirtualCapture\owner");
        let canonicalize_calls = Cell::new(0);
        let canonicalize = |candidate: &Path| {
            canonicalize_calls.set(canonicalize_calls.get() + 1);
            if candidate == existing {
                Ok(existing.to_path_buf())
            } else {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            }
        };
        let metadata = |_: &Path| Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        let normalized = normalize_windows(path, |candidate| {
            canonicalize_for_owner_using(candidate, canonicalize, metadata)
        });
        assert_eq!(
            normalized.as_deref(),
            Some(r"Q:\VirtualCapture\owner\releases\b1\vc_vcam.dll")
        );
        assert_eq!(canonicalize_calls.get(), 4);
        let absent = canonicalize_for_owner_using(
            Path::new(path),
            |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        )
        .unwrap_err();
        assert_eq!(absent.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(windows)]
    #[test]
    fn canonicalize_permission_errors_never_probe_metadata_or_fall_back() {
        use std::cell::Cell;
        use std::path::Path;

        let path = Path::new(r"Q:\VirtualCapture\owner\vc_vcam.dll");
        let metadata_calls = Cell::new(0);
        let error = canonicalize_for_owner_using(
            path,
            |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            |_| {
                metadata_calls.set(metadata_calls.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(metadata_calls.get(), 0);
        assert_eq!(
            normalize_windows(r"Q:\VirtualCapture\owner\vc_vcam.dll", |_| {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            }),
            None
        );
    }

    #[test]
    fn unresolved_paths_never_prove_equality_or_ownership() {
        let unresolved = |_: &str| None;
        assert!(!same_path_using(
            r"C:\Root\vc_vcam.dll",
            r"C:\Root\vc_vcam.dll",
            &unresolved
        ));
        let record = owner_at(r"C:\Root\vc_vcam.dll", r"C:\Root", 1234);
        assert!(!valid_owner_using(&record, &unresolved));
        assert!(!path_inside_root("", ""));
    }

    #[test]
    fn owner_reservation_is_not_fresh_for_another_host() {
        let shared_root = OwnerTestRoot::new();
        let runtime_a = OwnerTestRoot::new();
        let runtime_b = OwnerTestRoot::new();
        let shared = shared_root.path("build/vc_vcam.dll");
        let root_a = runtime_a.0.to_string_lossy().into_owned();
        let root_b = runtime_b.0.to_string_lossy().into_owned();
        let record = owner_at(&shared, &root_a, 2222);
        assert_eq!(
            decide_recovery_with_global_owner(
                "",
                &shared,
                &root_b,
                None,
                Some(&record),
                OwnerLiveness::LiveElsewhere,
                false,
            ),
            Recovery::Busy,
        );
        assert_eq!(
            decide_recovery_with_global_owner(
                "",
                &shared,
                &root_a,
                Some(&record),
                Some(&record),
                OwnerLiveness::SelfProcess,
                false,
            ),
            Recovery::Fresh,
        );
    }

    #[test]
    fn same_path_registration_requires_a_matching_owner_record() {
        let root = OwnerTestRoot::new();
        let local = root.path("app/releases/b2/vc_vcam.dll");
        assert_eq!(
            decide_recovery("", &local, None, OwnerLiveness::Unknown, true),
            Recovery::Fresh
        );
        // 同路径但没有记录/记录不符：无法证明属于本根，保守拒绝。
        assert_eq!(
            decide_recovery(&local, &local, None, OwnerLiveness::Unknown, true),
            Recovery::Unknown
        );
        let previous = root.owner("app/releases/b1/vc_vcam.dll", 1234);
        assert_eq!(
            decide_recovery(&local, &local, Some(&previous), OwnerLiveness::Dead, true),
            Recovery::Unknown
        );
        let record = root.owner("app/releases/b2/vc_vcam.dll", 1234);
        assert_eq!(
            decide_recovery(
                &local,
                &local,
                Some(&record),
                OwnerLiveness::SelfProcess,
                true
            ),
            Recovery::Local
        );
        assert_eq!(
            decide_recovery(&local, &local, Some(&record), OwnerLiveness::Dead, true),
            Recovery::Local,
            "crashed owner with a matching record may be reused"
        );
        // 来自本根的存活宿主仍拥有同一注册。
        assert_eq!(
            decide_recovery(
                &local,
                &local,
                Some(&record),
                OwnerLiveness::LiveElsewhere,
                true
            ),
            Recovery::Busy
        );
        assert_eq!(
            decide_recovery(&local, &local, Some(&record), OwnerLiveness::Unknown, true),
            Recovery::Unknown,
            "unknown liveness must never be treated as dead"
        );
    }

    #[test]
    fn ota_leftover_is_recoverable_only_when_recorded_and_owner_dead() {
        let root = OwnerTestRoot::new();
        let local = root.path("app/releases/b2/vc_vcam.dll");
        let previous = root.path("app/releases/b1/vc_vcam.dll");
        let record = root.owner("app/releases/b1/vc_vcam.dll", 1234);
        assert_eq!(
            decide_recovery(&previous, &local, Some(&record), OwnerLiveness::Dead, true),
            Recovery::StaleOwned
        );
        // 本进程自己写下的上一版本记录（例如运行期切换 current release）也可回收。
        assert_eq!(
            decide_recovery(
                &previous,
                &local,
                Some(&record),
                OwnerLiveness::SelfProcess,
                true
            ),
            Recovery::StaleOwned
        );
        // 旧 release 仍在运行：不要抢它的卡。
        assert_eq!(
            decide_recovery(
                &previous,
                &local,
                Some(&record),
                OwnerLiveness::LiveElsewhere,
                true
            ),
            Recovery::Busy
        );
        // 存活状态未知：失败关闭，而不是假定 owner 已死。
        assert_eq!(
            decide_recovery(
                &previous,
                &local,
                Some(&record),
                OwnerLiveness::Unknown,
                true
            ),
            Recovery::Unknown
        );
        // 没有所有权记录或记录不匹配：绝不触碰。
        assert_eq!(
            decide_recovery(&previous, &local, None, OwnerLiveness::Unknown, true),
            Recovery::Foreign
        );
        let foreign_root = OwnerTestRoot::new();
        let foreign_record = owner_at(&previous, &foreign_root.0.to_string_lossy(), 1234);
        assert_eq!(
            decide_recovery_with_global_owner(
                &previous,
                &local,
                &root.0.to_string_lossy(),
                Some(&foreign_record),
                Some(&foreign_record),
                OwnerLiveness::Dead,
                false,
            ),
            Recovery::Foreign
        );
        // 记录路径在本运行根之外，属于另一个安装。
        assert_eq!(
            decide_recovery(&previous, &local, Some(&record), OwnerLiveness::Dead, false),
            Recovery::Foreign
        );
    }

    #[test]
    fn shared_dll_cannot_be_claimed_from_another_run_root() {
        let shared_root = OwnerTestRoot::new();
        let runtime_a = OwnerTestRoot::new();
        let runtime_b = OwnerTestRoot::new();
        let shared = shared_root.path("build/Release/vc_vcam.dll");
        let root_a = runtime_a.0.to_string_lossy().into_owned();
        let root_b = runtime_b.0.to_string_lossy().into_owned();
        let active_a = owner_at(&shared, &root_a, 2222);
        let stale_b = owner_at(&shared, &root_b, 1234);

        // 运行根 B 旧的每根记录不能覆盖全局存活 owner A。
        assert_eq!(
            decide_recovery_with_global_owner(
                &shared,
                &shared,
                &root_b,
                Some(&stale_b),
                Some(&active_a),
                OwnerLiveness::LiveElsewhere,
                false,
            ),
            Recovery::Busy
        );
        // A 退出后，B 仍不能注销/接管 A 的 owner：根不匹配优先。
        assert_eq!(
            decide_recovery_with_global_owner(
                &shared,
                &shared,
                &root_b,
                Some(&stale_b),
                Some(&active_a),
                OwnerLiveness::Dead,
                false,
            ),
            Recovery::Foreign
        );
        // 损坏/缺失的本地镜像不能用来恢复全局记录。
        assert_eq!(
            decide_recovery_with_global_owner(
                &shared,
                &shared,
                &root_a,
                None,
                Some(&active_a),
                OwnerLiveness::Dead,
                false,
            ),
            Recovery::Unknown
        );
    }

    #[test]
    fn owner_liveness_distinguishes_dead_reuse_and_unknown() {
        let local = r"Q:\OwnerLiveness\vc_vcam.dll";
        let record = owner_at(local, r"Q:\OwnerLiveness", 1234);
        assert_eq!(
            classify_owner(&record, record.pid, ProcessState::Alive(record.start_time)),
            OwnerLiveness::SelfProcess
        );
        assert_eq!(
            classify_owner(
                &record,
                record.pid + 1,
                ProcessState::Alive(record.start_time)
            ),
            OwnerLiveness::LiveElsewhere
        );
        assert_eq!(
            classify_owner(
                &record,
                record.pid,
                ProcessState::Alive(record.start_time + 1)
            ),
            OwnerLiveness::Dead,
            "PID reuse with a different creation time means the recorded owner is gone"
        );
        assert_eq!(
            classify_owner(&record, record.pid, ProcessState::Dead),
            OwnerLiveness::Dead
        );
        assert_eq!(
            classify_owner(&record, record.pid, ProcessState::Unknown),
            OwnerLiveness::Unknown
        );
        let unverifiable = OwnerRecord {
            start_time: 0,
            ..owner_at(local, r"Q:\OwnerLiveness", 1234)
        };
        assert_eq!(
            classify_owner(
                &unverifiable,
                unverifiable.pid,
                ProcessState::Alive(record.start_time)
            ),
            OwnerLiveness::Unknown,
            "a zero creation time can never prove liveness"
        );
    }

    #[test]
    fn unrecorded_foreign_registration_is_never_adopted() {
        let root = OwnerTestRoot::new();
        let local = root.path("vc_vcam.dll");
        let other = root.path("other/vc_vcam.dll");
        assert_eq!(
            decide_recovery(&other, &local, None, OwnerLiveness::Unknown, true),
            Recovery::Foreign
        );
    }

    #[test]
    fn root_containment_uses_component_boundaries() {
        let root = OwnerTestRoot::new();
        let canonical_root = root.0.to_string_lossy().into_owned();
        let plain_root = canonical_root
            .strip_prefix(r"\\?\UNC\")
            .map(|unc| format!(r"\\{unc}"))
            .or_else(|| {
                canonical_root
                    .strip_prefix(r"\\?\")
                    .filter(|plain| plain.as_bytes().get(1..3) == Some(b":\\"))
                    .map(str::to_owned)
            });
        let dll = format!(r"{canonical_root}\app\releases\b1\vc_vcam.dll");
        assert!(path_inside_root(&dll, &canonical_root));
        assert!(path_inside_root(&dll, &format!("{canonical_root}\\")));
        if let Some(plain_root) = &plain_root {
            assert!(!path_inside_root(
                &format!("{plain_root}2\\vc_vcam.dll"),
                &canonical_root
            ));
        }
        assert!(!path_inside_root(&canonical_root, &canonical_root));
        assert!(!path_inside_root("", &canonical_root));
        // 归一化时，空输入绝不能变成工作目录。
        let cwd = std::env::current_dir().unwrap();
        assert!(!path_inside_root(
            &cwd.join("vc_vcam.dll").to_string_lossy(),
            ""
        ));
        if let Some(parent) = cwd.parent() {
            assert!(!path_inside_root("", &parent.to_string_lossy()));
        }
        if let Some(plain_root) = &plain_root {
            assert!(same_path(&canonical_root, &plain_root.replace('\\', "/")));
        }
    }

    #[test]
    fn registration_mutex_name_is_global_and_clsid_scoped() {
        // 互斥体不能落在运行根目录里，否则不同 release/安装目录挡不住彼此。
        let name = registration_mutex_name();
        assert!(name.starts_with(r"Global\"), "{name}");
        assert!(name.contains(CLSID), "{name}");
    }

    #[cfg(windows)]
    fn test_mutex_name(tag: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            r"Global\VirtualCapture.Test.{tag}.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[cfg(windows)]
    #[test]
    fn registration_lock_excludes_another_thread_until_released() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let name = test_mutex_name("excludes");
        let (ready, wait_ready) = mpsc::channel();
        let release = Arc::new(AtomicBool::new(false));
        let holder_release = release.clone();
        let holder_name = name.clone();
        let holder = std::thread::spawn(move || {
            let _guard = RegistrationLock::acquire(&holder_name, 5_000).expect("holder acquires");
            ready.send(()).expect("holder signals");
            while !holder_release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        wait_ready.recv().expect("holder ready");
        let started = Instant::now();
        let error = match RegistrationLock::acquire(&name, 100) {
            Ok(_) => panic!("second acquisition must fail while the mutex is held"),
            Err(error) => error,
        };
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "acquisition returned before the timeout"
        );
        assert!(error.contains("拒绝本次操作"), "{error}");
        release.store(true, Ordering::SeqCst);
        holder.join().expect("holder joins");
        // 释放后同一把锁必须可以再取得。
        let _guard = RegistrationLock::acquire(&name, 5_000).expect("lock is released");
    }

    #[cfg(windows)]
    #[test]
    fn registration_lock_recovers_an_abandoned_mutex() {
        // 属主线程直接退出（未 Release）时互斥体会被遗弃；下一个属主必须能继续，
        // 否则一次崩溃会让所有主机的虚拟卡注册永久失败。
        let name = test_mutex_name("abandoned");
        let holder_name = name.clone();
        std::thread::spawn(move || {
            let guard = RegistrationLock::acquire(&holder_name, 5_000).expect("thread acquires");
            // 句柄随线程结束泄漏，互斥体仍归已退出的线程所有。
            std::mem::forget(guard);
        })
        .join()
        .expect("holder joins");
        let _guard = RegistrationLock::acquire(&name, 5_000).expect("abandoned mutex is acquired");
    }
}
