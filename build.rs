fn main() {
    // The target triple, so `update` picks the matching release binary.
    println!(
        "cargo:rustc-env=RUNO_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    // The release number, which may have a fourth part Cargo does not allow.
    println!("cargo:rerun-if-changed=Cargo.toml");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let manifest = std::fs::read_to_string("Cargo.toml").unwrap_or_default();
    let release = release(&manifest).unwrap_or_else(|| version.clone());
    // An alpha or beta is the release number after an `a` or `b`.
    let number = release.strip_prefix(['a', 'b']).unwrap_or(&release);
    let number = number.split_once('-').map_or(number, |(n, _)| n);
    assert!(
        number == version || number.starts_with(&format!("{version}.")),
        "package.metadata.runo.release {release} must be {version} or {version}.<n>, optionally with a leading a or b and a -<revision>"
    );
    println!("cargo:rustc-env=RUNO_VERSION={release}");
}

/// `release = "..."` in `[package.metadata.runo]`.
fn release(manifest: &str) -> Option<String> {
    let mut inside = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == "[package.metadata.runo]";
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
