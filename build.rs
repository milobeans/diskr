fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os != "macos" || !matches!(arch.as_str(), "aarch64" | "x86_64") {
        panic!("unsupported target: diskr requires macOS on Apple Silicon (aarch64) or Intel (x86_64); Linux and Windows are not supported");
    }
}
