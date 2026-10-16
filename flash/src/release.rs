//! Downloading the image from Runo's releases on GitHub.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::text::t;

pub const REPO: &str = "chappie220/Runo";
const API: &str = "https://api.github.com";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// An image in a release.
#[derive(Clone, Debug)]
pub struct Found {
    pub tag: String,
    pub name: String,
    url: String,
    sums: Option<String>,
    pub size: u64,
}

fn client(timeout: Option<Duration>) -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("runo-flash/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(timeout)
        .build()?)
}

/// The newest release with an image for `board` (rpi5 or rpi4).
pub fn find(board: &str, api: Option<&str>) -> Result<Found> {
    let url = format!("{}/repos/{REPO}/releases?per_page=20", api.unwrap_or(API));
    let releases: Vec<Release> = client(Some(Duration::from_secs(30)))?
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .and_then(|r| r.error_for_status())
        .context(t("cannot reach GitHub", "连不上 GitHub"))?
        .json()?;
    pick(releases, board)
}

fn pick(releases: Vec<Release>, board: &str) -> Result<Found> {
    let name = format!("runo-{board}.img.xz");
    // Releases come newest first; stable ones are preferred.
    let mut releases: Vec<_> = releases.into_iter().filter(|r| !r.draft).collect();
    releases.sort_by_key(|r| r.prerelease);
    for release in releases {
        let Some(image) = release.assets.iter().find(|a| a.name == name) else {
            continue;
        };
        // The Image workflow names them by board.
        let sums_name = format!("SHA256SUMS-{board}");
        let sums = release
            .assets
            .iter()
            .find(|a| a.name == sums_name)
            .map(|a| a.browser_download_url.clone());
        return Ok(Found {
            tag: release.tag_name.clone(),
            name: name.clone(),
            url: image.browser_download_url.clone(),
            sums,
            size: image.size,
        });
    }
    bail!(
        "{} {name}{}",
        t("no release has", "还没有哪个版本提供"),
        t(
            " yet: download it from the Image workflow on GitHub Actions and choose the file",
            "：请从 GitHub Actions 的 Image 工作流下载，再选择本地文件",
        )
    )
}

fn sum_for<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (sum, file) = line.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then_some(sum)
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_file(path: &PathBuf) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(&hash.finalize()));
        }
        hash.update(&buf[..n]);
    }
}

/// Downloads the image to a cache folder, unless it is there already, and
/// checks it against the release's SHA256SUMS.
pub fn download(found: &Found, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("runo-flash").join(&found.tag);
    fs::create_dir_all(&dir)?;
    let path = dir.join(&found.name);
    let client = client(None)?;
    let want = match &found.sums {
        Some(url) => {
            let sums = client
                .get(url)
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.text())?;
            Some(
                sum_for(&sums, &found.name)
                    .map(str::to_string)
                    .context("SHA256SUMS")?,
            )
        }
        None => None,
    };
    if let Some(want) = &want
        && path.exists()
        && sha256_file(&path)? == *want
    {
        progress(found.size, found.size);
        return Ok(path);
    }
    let part = dir.join(format!("{}.part", found.name));
    let mut response = client
        .get(&found.url)
        .send()
        .and_then(|r| r.error_for_status())
        .context(t("cannot download the image", "下载镜像失败"))?;
    let total = response.content_length().unwrap_or(found.size);
    let mut out = File::create(&part)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        let n = response
            .read(&mut buf)
            .context(t("the download broke off", "下载中断"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        hash.update(&buf[..n]);
        done += n as u64;
        progress(done, total);
    }
    out.sync_all()?;
    drop(out);
    if let Some(want) = want
        && hex(&hash.finalize()) != want
    {
        let _ = fs::remove_file(&part);
        bail!(t(
            "the downloaded image does not match its checksum",
            "下载的镜像和校验值不符"
        ));
    }
    fs::rename(&part, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool, names: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            assets: names
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    browser_download_url: format!("https://x/{tag}/{n}"),
                    size: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn the_newest_stable_release_with_the_image_is_chosen() {
        let found = pick(
            vec![
                release("b0.7.0", true, &["runo-rpi4.img.xz"]),
                release(
                    "v0.6.0",
                    false,
                    &[
                        "runo-rpi4.img.xz",
                        "runo-rpi5.img.xz",
                        "SHA256SUMS",
                        "SHA256SUMS-rpi5",
                    ],
                ),
                release("v0.5.0", false, &["runo-x86_64-unknown-linux-musl"]),
            ],
            "rpi5",
        )
        .unwrap();
        assert_eq!(found.tag, "v0.6.0");
        assert_eq!(found.url, "https://x/v0.6.0/runo-rpi5.img.xz");
        assert_eq!(
            found.sums.as_deref(),
            Some("https://x/v0.6.0/SHA256SUMS-rpi5")
        );
        assert!(pick(vec![release("v0.5.0", false, &["runo"])], "rpi4").is_err());
    }

    #[test]
    fn checksums_are_found_by_name() {
        let sums = "aa  runo-rpi4.img.xz\nbb *runo-rpi5.img.xz\n";
        assert_eq!(sum_for(sums, "runo-rpi5.img.xz"), Some("bb"));
        assert_eq!(sum_for(sums, "runo-rpi4.img.xz"), Some("aa"));
        assert_eq!(sum_for(sums, "runo-rpi3.img.xz"), None);
    }
}
