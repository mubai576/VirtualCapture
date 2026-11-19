#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    vc_stream::windows::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("vc-stream live Npcap I/O is supported only on Windows");
    std::process::exit(2);
}
