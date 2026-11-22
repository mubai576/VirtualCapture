//! OTA helper：由运行中的 VirtualCapture 复制到 `updates/worker/` 后脱离父进程运行。
//!
//! 只信任 job 文件；流程与安全校验在 `vc_core::update::execute_activation` 中实现，
//! 本入口保持极薄，避免升级逻辑分散。
//!
//! `vc-updater launch` 可在终端前台使用：解析当前 release（或扁平根）后
//! 前台启动并等待子进程退出；Ctrl-C/Ctrl-Break 只交给子进程处理，避免启动器先退出
//! 导致脚本在旧后台尚未停机时立刻重启而抢占端口。
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use vc_core::update::{self, Role};

fn log(path: &Path, message: &str) {
    use std::io::Write;
    let stamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{stamp} {message}");
        let _ = file.sync_data();
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: vc-updater --job <job.json> [--log <updater.log>]");
    eprintln!(
        "       vc-updater launch --root <ROOT> --role capture|host [--bind <IP>] [--port <PORT>] [--native-dir <DIR>]"
    );
    ExitCode::from(2)
}

struct Launch {
    root: PathBuf,
    role: Role,
    bind: Option<String>,
    port: Option<u16>,
    native_dir: Option<PathBuf>,
}

#[cfg(windows)]
unsafe extern "system" fn ignore_ctrl(_event: u32) -> i32 {
    // CTRL_C_EVENT=0、CTRL_BREAK_EVENT=1 只交给子进程；其他事件（关闭窗口）照常处理。
    if _event <= 1 { 1 } else { 0 }
}

fn launch(spec: Launch) -> ExitCode {
    vc_core::paths::set_root_override(spec.root.clone());
    let target = match update::launch_role_at(&spec.root, spec.role) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("vc-updater: {error}");
            return ExitCode::from(2);
        }
    };
    let mut command = target.command(&spec.root, spec.native_dir.as_deref());
    if spec.role == Role::Host && target.capabilities.role_launch_args {
        command.arg("--start-role").arg("receive");
    }
    if spec.role == Role::Capture {
        if let Some(bind) = &spec.bind {
            command.arg("--bind").arg(bind);
        }
        if let Some(port) = spec.port {
            command.arg("--port").arg(port.to_string());
        }
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        SetConsoleCtrlHandler(Some(ignore_ctrl), 1);
    }
    match command.spawn() {
        Ok(mut child) => loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(error) => {
                    eprintln!("vc-updater: 等待子进程失败：{error}");
                    return ExitCode::from(5);
                }
            }
        },
        Err(error) => {
            eprintln!("vc-updater: 无法启动 {}：{error}", target.exe.display());
            ExitCode::from(5)
        }
    }
}

fn parse_launch(mut args: impl Iterator<Item = String>) -> Result<Launch, ExitCode> {
    let mut root = None;
    let mut role = None;
    let mut bind = None;
    let mut port = None;
    let mut native_dir = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = args.next().map(PathBuf::from),
            "--role" => {
                role = match args.next().as_deref() {
                    Some("capture") => Some(Role::Capture),
                    Some("host") => Some(Role::Host),
                    _ => return Err(usage()),
                }
            }
            "--bind" => bind = args.next(),
            "--port" => {
                port = args.next().and_then(|value| value.parse::<u16>().ok());
                if port.is_none() {
                    return Err(usage());
                }
            }
            "--native-dir" => native_dir = args.next().map(PathBuf::from),
            other => {
                eprintln!("unknown argument: {other}");
                return Err(usage());
            }
        }
    }
    match (root, role) {
        (Some(root), Some(role)) => Ok(Launch {
            root,
            role,
            bind,
            port,
            native_dir,
        }),
        _ => Err(usage()),
    }
}

fn run() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut job_path: Option<PathBuf> = None;
    let mut log_path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "launch" => return parse_launch(args).map_or_else(|code| code, launch),
            "--job" => match args.next().map(PathBuf::from) {
                Some(path) => job_path = Some(path),
                None => return usage(),
            },
            "--log" => log_path = args.next().map(PathBuf::from),
            "--version" => {
                println!("vc-updater {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return usage();
            }
        }
    }
    let Some(job_path) = job_path else {
        return usage();
    };
    let mut job = match update::read_job(&job_path) {
        Ok(job) => job,
        Err(error) => {
            eprintln!("vc-updater: {error}");
            if let Some(path) = &log_path {
                log(path, &format!("job invalid: {error}"));
            }
            return ExitCode::from(2);
        }
    };
    vc_core::paths::set_root_override(PathBuf::from(&job.root));
    if job.target_capabilities.is_none() {
        job.target_capabilities =
            vc_core::update::staged_capabilities_at(Path::new(&job.root), &job.target).ok();
    }
    let log_path = log_path.unwrap_or_else(|| job_path.with_extension("log"));
    log(
        &log_path,
        &format!(
            "start role={:?} target={} previous={:?} parent={}",
            job.role, job.target, job.previous, job.parent_pid
        ),
    );
    match update::execute_activation(&job) {
        Ok(()) => {
            log(&log_path, &format!("active {}", job.target));
            ExitCode::SUCCESS
        }
        Err(error) => {
            log(
                &log_path,
                &format!("failed code={} {}", error.code, error.message),
            );
            eprintln!("vc-updater: {}", error.message);
            ExitCode::from(error.code.clamp(0, 255) as u8)
        }
    }
}

fn main() -> ExitCode {
    run()
}
