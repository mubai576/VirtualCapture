//! 稳定的图形入口，打包为 VirtualCapture.exe。不依赖 shell 或 PowerShell。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use vc_core::paths;
use vc_core::update::{self, Role};

#[derive(Parser)]
#[command(name = "VirtualCapture", version, about = "VirtualCapture 主机应用")]
struct Cli {
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long)]
    native_dir: Option<PathBuf>,
}

/// 也允许从旧 OTA 客户端安装的 release 启动本入口。
/// 该 release 内的 version.json 不得把它变成独立的数据根。
fn default_root(package: &Path) -> PathBuf {
    if let Some(releases) = package.parent()
        && releases.file_name().is_some_and(|name| name == "releases")
        && let Some(app) = releases.parent()
        && app.file_name().is_some_and(|name| name == "app")
        && let Some(root) = app.parent()
        && root.join("app/current.json").exists()
    {
        return root.to_path_buf();
    }
    package.to_path_buf()
}

fn launch(cli: Cli) -> Result<(), String> {
    let own_exe = std::env::current_exe().map_err(|e| format!("无法确定应用目录：{e}"))?;
    let package = own_exe.parent().ok_or("无法确定应用目录。")?;
    let root = cli
        .root
        .or_else(|| std::env::var_os("VC_ROOT").map(PathBuf::from))
        .unwrap_or_else(|| default_root(package));
    std::fs::create_dir_all(&root).map_err(|e| format!("无法创建运行目录：{e}"))?;
    let root = std::fs::canonicalize(root).map_err(|e| format!("无法解析运行目录：{e}"))?;
    paths::validate_current_at(&root)?;
    let managed = paths::layout_at(&root) == paths::Layout::Managed;
    let exe = if managed {
        update::launch_exe_at(&root, Role::Host)?
    } else {
        package.join("vc-host.exe")
    };
    if !exe.is_file() {
        return Err(format!(
            "未找到主机应用：{}。请完整解压运行包。",
            exe.display()
        ));
    }
    let native = cli
        .native_dir
        .or_else(|| std::env::var_os("VC_NATIVE_DIR").map(PathBuf::from))
        .unwrap_or_else(|| exe.parent().unwrap_or(package).to_path_buf());
    let data = if managed {
        root.join("data")
    } else {
        root.clone()
    };
    let logs = data.join("run");
    std::fs::create_dir_all(&logs).map_err(|e| format!("无法创建日志目录：{e}"))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let tag = format!("host-{stamp}-{}", std::process::id());
    let stdout_path = logs.join(format!("{tag}.stdout.log"));
    let stderr_path = logs.join(format!("{tag}.stderr.log"));
    let open_log = |path: &Path| {
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("无法创建启动日志 {}：{e}", path.display()))
    };
    let stdout = open_log(&stdout_path)?;
    let stderr = open_log(&stderr_path)?;
    let mut command = Command::new(&exe);
    command
        .arg("--root")
        .arg(&root)
        .arg("--native-dir")
        .arg(&native)
        .current_dir(exe.parent().unwrap_or(package))
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    let mut child = command
        .spawn()
        .map_err(|e| format!("无法启动主机应用：{e}\n{}", exe.display()))?;
    let receipt = logs.join(format!("{tag}.launch.json"));
    let details = serde_json::json!({
        "pid": child.id(), "root": root, "exe": exe,
        "stdout": stdout_path, "stderr": stderr_path,
    });
    // 仅用于诊断的回执：绝不包含配对/更新令牌。
    if let Ok(file) = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(receipt)
    {
        let _ = serde_json::to_writer(file, &details);
    }
    // 尽快释放稳定入口，以便 OTA 直接重启真正的 GUI。
    std::thread::sleep(Duration::from_millis(700));
    if let Some(status) = child.try_wait().map_err(|e| e.to_string())?
        && !status.success()
    {
        return Err(format!(
            "主机应用启动失败（{status}）。\n请查看：{}",
            stderr_path.display()
        ));
    }
    Ok(())
}

fn report_error(message: &str) {
    eprintln!("{message}");
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let title: Vec<u16> = "VirtualCapture 启动失败\0".encode_utf16().collect();
        let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        // 安全：text/title 都是以 NUL 结尾的 UTF-16 缓冲区，且在调用期间保持
        // 存活；MessageBoxW 不会保留这些指针。
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

fn main() -> ExitCode {
    match launch(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_error(&error);
            ExitCode::from(2)
        }
    }
}
