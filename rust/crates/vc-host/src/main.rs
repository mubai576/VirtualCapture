//! 单一产品入口：记住桌面角色、显式采集后台和隔离虚拟卡能力探测。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app_ui;
mod console;
mod host_ui;
mod probe;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use update::Role;
use vc_core::startup::{self, DesktopRole};
use vc_core::{paths, update, vcam};

#[derive(Parser)]
#[command(
    name = "VirtualCapture",
    version,
    about = "VirtualCapture：启动上次保存的桌面角色，首次运行先选择角色"
)]
struct Cli {
    /// 固定运行根目录（配置、日志与更新数据）。
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// 显式 DLL 目录，默认随实际运行版本。
    #[arg(long, global = true)]
    native_dir: Option<PathBuf>,
    /// 跳过角色选择，直接打开上次保存的角色（仅更新/内部入口）。
    #[arg(long, hide = true, conflicts_with_all = ["select_role", "start_role"])]
    resume_role: bool,
    /// 启动指定桌面角色（仅更新入口）。
    #[arg(long, value_parser = ["receive", "capture"], hide = true, conflicts_with_all = ["select_role", "resume_role"])]
    start_role: Option<String>,
    /// 忽略已保存的启动角色，打开选择界面。
    #[arg(long, conflicts_with_all = ["resume_role", "start_role"])]
    select_role: bool,
    /// 图形采集端的监听地址（默认 0.0.0.0）。
    #[arg(long, value_name = "IP")]
    capture_bind: Option<String>,
    /// 图形采集端的监听端口（默认 5143）。
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    capture_port: Option<u16>,
    /// 更新助手等待的 GUI 首帧就绪回执。
    #[arg(long, hide = true, conflicts_with = "select_role")]
    update_ready: Option<PathBuf>,
    #[command(subcommand)]
    mode: Option<Mode>,
}

#[derive(Subcommand)]
enum Mode {
    /// 启动被采集机后台，或运行其诊断命令。
    Capture(vc_capture::Options),
    #[command(name = vcam::PROBE_MODE, hide = true)]
    VcamProbe {
        #[arg(long)]
        dll: PathBuf,
    },
}

/// 识别 `ROOT/app/releases/<build>/`，避免用版本目录里的 version.json 当数据根。
fn release_root(exe_dir: &Path) -> Option<&Path> {
    let releases = exe_dir.parent()?;
    let app = releases.parent()?;
    if releases.file_name()?.eq_ignore_ascii_case("releases")
        && app.file_name()?.eq_ignore_ascii_case(paths::APP_DIR)
    {
        app.parent()
    } else {
        None
    }
}

fn default_root(exe_dir: &Path) -> PathBuf {
    if let Some(root) = release_root(exe_dir) {
        return root.to_path_buf();
    }
    let pointer = exe_dir.join(paths::APP_DIR).join(paths::CURRENT_FILE_NAME);
    // 固定入口所在目录不依赖 version.json。坏指针留给体检报错，不向上找别的 ROOT。
    if !matches!(std::fs::symlink_metadata(pointer), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    {
        return exe_dir.to_path_buf();
    }
    paths::root()
}

fn record_desktop_launch(exe: &Path, root: &Path) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::time::{SystemTime, UNIX_EPOCH};
    let logs = paths::run_dir();
    std::fs::create_dir_all(&logs)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let tag = format!("host-{stamp}-{}", std::process::id());
    let open = |path: &Path| OpenOptions::new().create_new(true).write(true).open(path);
    let stdout = logs.join(format!("{tag}.stdout.log"));
    let stderr = logs.join(format!("{tag}.stderr.log"));
    console::redirect_if_missing(false, open(&stdout)?)?;
    console::redirect_if_missing(true, open(&stderr)?)?;
    serde_json::to_writer(
        open(&logs.join(format!("{tag}.launch.json")))?,
        &serde_json::json!({"pid":std::process::id(), "root":root, "exe":exe,
                            "stdout":stdout, "stderr":stderr}),
    )?;
    Ok(())
}

fn run(cli: Cli) -> anyhow::Result<i32> {
    let capture = match cli.mode {
        Some(Mode::VcamProbe { dll }) => return probe::run(&dll).map(|()| 0),
        Some(Mode::Capture(options)) => Some(options),
        None => None,
    };
    let desktop_capture = capture
        .as_ref()
        .is_some_and(|options| options.is_desktop() && !options.is_diagnostic());
    let selected_capture = cli.start_role.as_deref() == Some("capture");
    let desktop = capture.is_none() || desktop_capture || selected_capture;
    let has_console = !desktop && console::attach();
    anyhow::ensure!(
        capture.is_none()
            || (cli.update_ready.is_none()
                && !cli.select_role
                && cli.start_role.is_none()
                && cli.capture_bind.is_none()
                && cli.capture_port.is_none()),
        "这些图形启动选项不能与 capture 子命令一起使用"
    );
    let exe = std::fs::canonicalize(std::env::current_exe()?)?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法定位程序目录"))?;
    let root = cli
        .root
        .or_else(|| std::env::var_os("VC_ROOT").map(PathBuf::from))
        .unwrap_or_else(|| default_root(exe_dir));
    std::fs::create_dir_all(&root)?;
    let root = std::fs::canonicalize(root)?;
    paths::validate_current_at(&root).map_err(anyhow::Error::msg)?;
    let saved_role = if capture.is_none() && cli.start_role.is_none() && cli.update_ready.is_none()
    {
        match startup::load_at(&root) {
            Ok(role) => role,
            Err(_) if cli.select_role => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    let stored_capture = capture.is_none()
        && !cli.select_role
        && cli.start_role.is_none()
        && cli.update_ready.is_none()
        && saved_role == Some(DesktopRole::Capture);
    let role = if capture.is_some() || selected_capture || stored_capture {
        Role::Capture
    } else {
        Role::Host
    };
    let native = cli.native_dir.or_else(|| {
        std::env::var_os("VC_NATIVE_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
    });
    if let Some(own_root) = release_root(exe_dir) {
        anyhow::ensure!(own_root == root, "版本目录中的程序必须使用所属固定 ROOT");
        anyhow::ensure!(
            paths::layout_at(&root) == paths::Layout::Managed,
            "固定 ROOT 没有激活的版本；请从 ROOT 启动"
        );
    }
    if paths::layout_at(&root) == paths::Layout::Managed
        || root.join(update::MANIFEST_NAME).try_exists()?
    {
        let target = update::launch_role_at(&root, role).map_err(anyhow::Error::msg)?;
        if std::fs::canonicalize(&target.exe)? != exe {
            // 只有 ROOT 等外部入口可以引导。过期 release 不再创建其他角色进程，
            // OTA helper 等待的 PID 必须就是运行实际角色的进程。
            anyhow::ensure!(
                release_root(exe_dir).is_none() && cli.update_ready.is_none(),
                "程序不属于当前版本；请从固定 ROOT 启动"
            );
            let mut command = target.command(&root, native.as_deref());
            if let Some(capture) = capture {
                capture.append_args(&mut command);
                if !desktop_capture {
                    return Ok(console::run_child(command, has_console)?);
                }
            } else if selected_capture || stored_capture {
                command.arg("--desktop");
                if let Some(bind) = &cli.capture_bind {
                    command.arg("--bind").arg(bind);
                }
                if let Some(port) = cli.capture_port {
                    command.arg("--port").arg(port.to_string());
                }
            } else if cli.select_role {
                command.arg("--select-role");
                if let Some(bind) = &cli.capture_bind {
                    command.arg("--capture-bind").arg(bind);
                }
                if let Some(port) = cli.capture_port {
                    command.arg("--capture-port").arg(port.to_string());
                }
            } else {
                if let Some(role) = &cli.start_role {
                    command.arg("--start-role").arg(role);
                }
                if cli.resume_role {
                    command.arg("--resume-role");
                }
                if let Some(bind) = &cli.capture_bind {
                    command.arg("--capture-bind").arg(bind);
                }
                if let Some(port) = cli.capture_port {
                    command.arg("--capture-port").arg(port.to_string());
                }
            }
            command.spawn()?;
            return Ok(0);
        }
    }
    paths::set_root_override(root.clone());
    paths::set_native_dir_override(native.unwrap_or_else(|| exe_dir.to_path_buf()));
    if !desktop {
        vc_capture::run(capture.expect("后台入口需要采集参数"), has_console)?;
        return Ok(0);
    }
    let start = if desktop_capture || selected_capture {
        app_ui::Start::Role(DesktopRole::Capture)
    } else if let Some(role) = cli.start_role.as_deref() {
        app_ui::Start::Role(match role {
            "capture" => DesktopRole::Capture,
            "receive" => DesktopRole::Receive,
            _ => unreachable!("clap 已限制内部启动角色"),
        })
    } else if cli.update_ready.is_some() {
        app_ui::Start::Role(DesktopRole::Receive)
    } else {
        let choose = cli.select_role;
        match startup::load_at(&root) {
            Ok(Some(role)) if !choose || cli.resume_role => app_ui::Start::Role(role),
            Ok(previous) => app_ui::Start::Choose {
                previous,
                error: None,
            },
            Err(error) => app_ui::Start::Choose {
                previous: None,
                error: Some(format!(
                    "无法读取启动角色（{error}）。选择后将重新保存启动角色。"
                )),
            },
        }
    };
    let capture = capture.unwrap_or_else(|| {
        vc_capture::Options::listening(
            cli.capture_bind.unwrap_or_else(|| "0.0.0.0".into()),
            cli.capture_port.unwrap_or(5143),
        )
    });
    record_desktop_launch(&exe, &root)?;
    app_ui::run(start, capture, cli.update_ready);
    Ok(0)
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if std::env::args_os().nth(1).as_deref() != Some(vcam::PROBE_MODE.as_ref()) {
                console::attach();
            }
            let _ = error.print();
            return ExitCode::from(error.exit_code() as u8);
        }
    };
    let graphical = cli.mode.is_none() && cli.update_ready.is_none();
    match run(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            let message = format!("启动失败：{error:#}");
            eprintln!("{message}");
            if graphical && !console::has_stderr() {
                show_error(&message);
            }
            ExitCode::from(2)
        }
    }
}

fn show_error(message: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MessageBoxW};
        let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        // 安全：指针指向有效的 NUL 结尾 UTF-16；MessageBoxW 不会保留它。
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                windows_sys::w!("VirtualCapture"),
                MB_ICONERROR,
            )
        };
    }
    #[cfg(not(windows))]
    let _ = message;
}
