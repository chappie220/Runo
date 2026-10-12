fn main() {
    // The target triple, so `update` picks the matching release binary.
    println!(
        "cargo:rustc-env=OPENCLAW_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    // The release number, which may have a fourth part Cargo does not allow.
    println!("cargo:rerun-if-changed=Cargo.toml");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let manifest = std::fs::read_to_string("Cargo.toml").unwrap_or_default();
    let release = release(&manifest).unwrap_or_else(|| version.clone());
    assert!(
        release == version || release.starts_with(&format!("{version}.")),
        "package.metadata.openclaw.release {release} must be {version} or {version}.<n>, optionally with a -<revision>"
    );
    println!("cargo:rustc-env=OPENCLAW_VERSION={release}");
}

/// `release = "..."` in `[package.metadata.openclaw]`.
fn release(manifest: &str) -> Option<String> {
    let mut inside = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == "[package.metadata.openclaw]";
            continue;
        }
        if !inside {
            continue;
        }
        if let Some(value) = line.strip_prefix("release") {
            let value = value.trim_start().strip_prefix('=')?.trim();
            return Some(value.trim_matches('"').to_owned());
        }
    }
    None
}
