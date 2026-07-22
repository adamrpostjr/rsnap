fn main() {
    // Only meaningful when actually targeting Windows (keeps `cargo check` sane
    // on other platforms during development, even though this app is Windows-only).
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_manifest_file("rsnap.exe.manifest");
    res.set_icon("icon.ico");
    if let Err(e) = res.compile() {
        eprintln!("Failed to embed Windows manifest: {e}");
        std::process::exit(1);
    }
}
