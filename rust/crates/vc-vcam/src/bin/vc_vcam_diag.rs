//! VirtualCapture SHM v3 环的只读诊断工具。
//!
//! 本程序不接触 COM、注册表或接收端：它打开滤镜会使用的同名映射（布局见
//! `docs/spec/shm-v3.md`），校验冻结头，等待一次稳定的 seqlock 发布，并
//! 报告帧 id / 哈希。适用于在注册 DLL 之前证明机器上的共享读取路径可用。
//!
//! ```text
//! vc-vcam-diag.exe --capabilities
//! vc-vcam-diag.exe --shm Local\VirtualCaptureVcamFrame --wait-ms 2000
//! ```

use std::env;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use vc_vcam::capabilities::CAPABILITIES_JSON;
use vc_vcam::shm;
use vc_vcam::win;

fn usage() -> ExitCode {
    eprintln!(
        "usage: vc-vcam-diag [--capabilities] [--shm <mapping-name>] [--wait-ms <ms>]\n\
         without --shm the filter resolution order is used (VCAM_FRAME_MAP, registry FrameMap, default)"
    );
    ExitCode::from(64)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn main() -> ExitCode {
    let mut show_capabilities = false;
    let mut shm_name: Option<String> = None;
    let mut wait_ms: u64 = 2_000;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--capabilities" => show_capabilities = true,
            "--shm" => match args.next() {
                Some(name) => shm_name = Some(name),
                None => return usage(),
            },
            "--wait-ms" => match args.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(ms) => wait_ms = ms,
                None => return usage(),
            },
            "--help" | "-h" => {
                let _ = usage();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return usage();
            }
        }
    }

    if show_capabilities {
        println!("capabilities={CAPABILITIES_JSON}");
    }

    let name = shm_name.unwrap_or_else(win::frame_map_name);
    println!("shm_name={name}");

    let Some(mapping) = win::ReadMapping::open(&name) else {
        println!("opened=false");
        println!("hint=start the receiver (or pass --shm) and retry");
        return ExitCode::from(2);
    };

    let view = mapping.view();
    let frame_bytes = mapping.frame_bytes() as usize;
    println!("opened=true");
    println!("mapping_bytes={}", mapping.size());
    println!("frame_bytes={frame_bytes}");
    match mapping.geometry() {
        Some(geometry) => {
            println!(
                "geometry={}x{}@{}",
                geometry.width(),
                geometry.height(),
                geometry.fps()
            );
            println!("expected_frame_bytes={}", geometry.frame_bytes());
        }
        None => println!("geometry=invalid"),
    }

    let mut buffer = vec![0u8; frame_bytes];
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let mut last_error = "no stable publication";
    loop {
        let peek = unsafe { shm::peek(view) };
        if let Some((slot, seq)) = peek {
            if let Some(copy) = unsafe { shm::copy_slot(view, slot, seq, buffer.as_mut_ptr()) } {
                let hash = fnv1a64(&buffer[..frame_bytes]);
                println!("stable=true");
                println!("slot={slot}");
                println!("seq={seq}");
                println!("frame_id={}", copy.frame_id);
                println!("qpc_pub_ns={}", copy.qpc_pub_ns);
                println!("now_ns={}", win::qpc_ns());
                println!("fnv1a64={hash:016x}");
                println!("first_bytes={:02x?}", &buffer[..8.min(buffer.len())]);
                return ExitCode::SUCCESS;
            }
            last_error = "seqlock copy torn";
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    println!("stable=false");
    println!("reason={last_error}");
    ExitCode::from(3)
}
