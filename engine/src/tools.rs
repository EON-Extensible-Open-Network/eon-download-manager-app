// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Helper programs fetched the first time they are needed, never bundled:
//! a media extractor (`yt-dlp`, Unlicense) for pages that carry video or audio,
//! and a converter (`FFmpeg`) for changing formats and joining video with sound.
//!
//! Each is downloaded from its project's own release page and checked against
//! the SHA-256 list that release publishes; a file that does not match is
//! deleted and never run. They live in `<data>/tools/`; the extractor is
//! refreshed weekly, because the sites it reads change.

use crate::{http, Error};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const YTDLP_BASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
const FFMPEG_BASE: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest";

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// The extractor's file name on the release page for this system.
fn ytdlp_asset() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else if cfg!(target_os = "macos") {
        "yt-dlp_macos"
    } else {
        "yt-dlp_linux"
    }
}

/// Look for a program on `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe(name)))
        .find(|p| p.is_file())
}

pub fn ytdlp_path(data_dir: &Path) -> Option<PathBuf> {
    let own = data_dir.join("tools").join(exe("yt-dlp"));
    own.is_file().then_some(own)
}

/// The converter: our own copy, or one already on the system.
pub fn ffmpeg_path(data_dir: &Path) -> Option<PathBuf> {
    let own = data_dir.join("tools").join(exe("ffmpeg"));
    if own.is_file() {
        return Some(own);
    }
    on_path("ffmpeg")
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `name` → its SHA-256 from a `sha256sum`-style list.
fn expected(list: &str, name: &str) -> Option<String> {
    list.lines().find_map(|line| {
        let (sum, file) = line.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| sum.trim().to_ascii_lowercase())
    })
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String, Error> {
    let source = http::Source {
        url: url.to_owned(),
        ..http::Source::default()
    };
    let response = http::get(client, &source, url)
        .send()
        .await
        .map_err(http::network)?;
    if !response.status().is_success() {
        return Err(http::status_error(response.status()));
    }
    response.text().await.map_err(http::network)
}

/// Stream `url` into `path` (never held whole in memory), hashing as it goes;
/// the file is kept only if its SHA-256 is the one `list` publishes for `name`.
async fn fetch_checked(
    client: &reqwest::Client,
    url: &str,
    list: &str,
    name: &str,
    path: &Path,
) -> Result<(), Error> {
    use tokio::io::AsyncWriteExt;
    let want = expected(list, name)
        .ok_or_else(|| Error::Invalid(format!("{name} is not in its release's checksum list")))?;
    let source = http::Source {
        url: url.to_owned(),
        ..http::Source::default()
    };
    let mut response = http::get(client, &source, url)
        .send()
        .await
        .map_err(http::network)?;
    if !response.status().is_success() {
        return Err(http::status_error(response.status()));
    }
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| Error::Io(e.to_string()))?;
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| Error::Io(e.to_string()))?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = response.chunk().await.map_err(http::network)? {
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| Error::Io(e.to_string()))?;
    }
    file.flush().await.map_err(|e| Error::Io(e.to_string()))?;
    drop(file);
    if hex(&hasher.finalize()) == want {
        Ok(())
    } else {
        let _ = tokio::fs::remove_file(path).await;
        Err(Error::Invalid(format!(
            "{name} does not match its published SHA-256; not used"
        )))
    }
}

#[cfg(test)]
fn checked(bytes: &[u8], list: &str, name: &str) -> Result<(), Error> {
    let want = expected(list, name)
        .ok_or_else(|| Error::Invalid(format!("{name} is not in its release's checksum list")))?;
    if hex(&Sha256::digest(bytes)) == want {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{name} does not match its published SHA-256; not used"
        )))
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn make_executable(_: &Path) {}

/// Fetch (or update) the extractor, checked against its release's SHA-256 list.
pub async fn install_ytdlp(client: &reqwest::Client, data_dir: &Path) -> Result<PathBuf, Error> {
    let asset = ytdlp_asset();
    let sums = fetch_text(client, &format!("{YTDLP_BASE}/SHA2-256SUMS")).await?;
    let path = data_dir.join("tools").join(exe("yt-dlp"));
    let tmp = path.with_extension("download");
    fetch_checked(client, &format!("{YTDLP_BASE}/{asset}"), &sums, asset, &tmp).await?;
    make_executable(&tmp);
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| Error::Io(e.to_string()))?;
    Ok(path)
}

/// The extractor, fetched if missing and refreshed when a week old (sites
/// change; the extractor follows them). An old copy is kept if the refresh fails.
pub async fn ytdlp(client: &reqwest::Client, data_dir: &Path) -> Result<PathBuf, Error> {
    if let Some(p) = ytdlp_path(data_dir) {
        let fresh = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() < 7 * 24 * 3600);
        if fresh {
            return Ok(p);
        }
        return Ok(install_ytdlp(client, data_dir).await.unwrap_or(p));
    }
    install_ytdlp(client, data_dir).await
}

/// The converter. On Windows a static build is fetched, checked against its
/// release's list and unpacked with the system's own `tar`; elsewhere the
/// system's (`apt`, `brew`) is used.
pub async fn ffmpeg(client: &reqwest::Client, data_dir: &Path) -> Result<PathBuf, Error> {
    if let Some(p) = ffmpeg_path(data_dir) {
        return Ok(p);
    }
    if !cfg!(windows) {
        return Err(Error::Unsupported(
            "this needs ffmpeg; install it with your system's package manager".into(),
        ));
    }
    let asset = "ffmpeg-master-latest-win64-gpl.zip";
    let sums = fetch_text(client, &format!("{FFMPEG_BASE}/checksums.sha256")).await?;
    let tools = data_dir.join("tools");
    let work = tools.join("ffmpeg-unpack");
    let _ = tokio::fs::remove_dir_all(&work).await;
    let archive = work.join(asset);
    fetch_checked(
        client,
        &format!("{FFMPEG_BASE}/{asset}"),
        &sums,
        asset,
        &archive,
    )
    .await?;
    let status = command(Path::new("tar"))
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&work)
        .args(["*/bin/ffmpeg.exe", "*/bin/ffprobe.exe"])
        .status()
        .await
        .map_err(|e| Error::Io(e.to_string()))?;
    if !status.success() {
        let _ = tokio::fs::remove_dir_all(&work).await;
        return Err(Error::Invalid(
            "the converter's archive could not be unpacked".into(),
        ));
    }
    let bin = std::fs::read_dir(&work)
        .map_err(|e| Error::Io(e.to_string()))?
        .filter_map(Result::ok)
        .map(|e| e.path().join("bin"))
        .find(|b| b.join("ffmpeg.exe").is_file())
        .ok_or_else(|| Error::Invalid("ffmpeg.exe is not in the archive".into()))?;
    for name in ["ffmpeg.exe", "ffprobe.exe"] {
        tokio::fs::rename(bin.join(name), tools.join(name))
            .await
            .map_err(|e| Error::Io(e.to_string()))?;
    }
    let _ = tokio::fs::remove_dir_all(&work).await;
    ffmpeg_path(data_dir).ok_or_else(|| Error::Io("ffmpeg was not written".into()))
}

/// A command for a helper: no console window on Windows.
pub fn command(program: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_lists_are_read_and_enforced() {
        let list = "0123abcd  yt-dlp.exe\nffff  *yt-dlp_linux\n";
        assert_eq!(expected(list, "yt-dlp.exe").as_deref(), Some("0123abcd"));
        assert_eq!(expected(list, "yt-dlp_linux").as_deref(), Some("ffff"));
        assert_eq!(expected(list, "other"), None);
        let data = b"hello";
        let good = format!("{}  thing\n", hex(&Sha256::digest(data)));
        assert!(checked(data, &good, "thing").is_ok());
        assert!(checked(b"tampered", &good, "thing").is_err());
        assert!(checked(data, &good, "missing").is_err());
    }
}
