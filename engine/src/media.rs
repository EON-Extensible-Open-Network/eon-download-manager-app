// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! A page that carries video or audio, rather than a file: the extractor
//! (see `tools`) finds what the page plays, the person chooses what to keep,
//! and the download runs here — paused, resumed and shown like any other.

use crate::plan::Segment;
use crate::tools;
use crate::transfer::{Live, Outcome};
use crate::Error;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

/// What a page offers, as the add sheet shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaInfo {
    pub title: String,
    pub duration: Option<f64>,
    pub uploader: Option<String>,
    /// Video heights on offer, best first (`[2160, 1080, 720]`).
    pub heights: Vec<u32>,
    pub has_video: bool,
    pub has_audio: bool,
    pub page: String,
}

/// What to keep.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Pick {
    Best,
    /// The best at or below this height.
    Height(u32),
    AudioOnly,
    VideoOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaSpec {
    pub pick: Pick,
    /// `mp4`, `mkv` for video; `mp3`, `m4a` for audio; `None` keeps what the page has.
    #[serde(default)]
    pub container: Option<String>,
}

/// Netscape cookie file for the extractor, from the `Cookie` header a browser
/// would send to `url` — the only cookies EON ever holds (see the spec).
pub fn cookie_file(path: &Path, url: &str, header: &str) -> std::io::Result<()> {
    let parsed = url::Url::parse(url).map_err(std::io::Error::other)?;
    let host = parsed.host_str().unwrap_or_default();
    let secure = if parsed.scheme() == "https" {
        "TRUE"
    } else {
        "FALSE"
    };
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for pair in header.split(';') {
        if let Some((name, value)) = pair.trim().split_once('=') {
            let (name, value) = (name.trim(), value.trim());
            out += &[
                ".",
                host,
                "\tTRUE\t/\t",
                secure,
                "\t0\t",
                name,
                "\t",
                value,
                "\n",
            ]
            .concat();
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, out)
}

fn last_error(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find(|l| l.contains("ERROR"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("the page offered nothing to download")
        .trim()
        .trim_start_matches("ERROR:")
        .trim()
        .to_owned()
}

/// Ask the extractor what a page plays.
pub async fn inspect(ytdlp: &Path, url: &str, cookies: Option<&Path>) -> Result<MediaInfo, Error> {
    let mut cmd = tools::command(ytdlp);
    cmd.args(["-J", "--no-playlist", "--no-warnings", "--no-progress"]);
    if let Some(c) = cookies {
        cmd.arg("--cookies").arg(c);
    }
    cmd.arg("--").arg(url);
    let out = cmd.output().await.map_err(|e| Error::Io(e.to_string()))?;
    if !out.status.success() {
        return Err(Error::Unsupported(last_error(&String::from_utf8_lossy(
            &out.stderr,
        ))));
    }
    let mut v: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| Error::Invalid(e.to_string()))?;
    if v["_type"] == "playlist" {
        v = v["entries"]
            .as_array()
            .and_then(|e| e.first())
            .cloned()
            .unwrap_or(Value::Null);
    }
    if v["is_live"] == true {
        return Err(Error::Unsupported(
            "live streams cannot be downloaded yet".into(),
        ));
    }
    let formats = v["formats"].as_array().cloned().unwrap_or_default();
    let has = |key: &str| {
        formats
            .iter()
            .any(|f| f[key].as_str().is_some_and(|c| c != "none"))
            || v[key].as_str().is_some_and(|c| c != "none")
    };
    let mut heights: Vec<u32> = formats
        .iter()
        .filter(|f| f["vcodec"].as_str().is_some_and(|c| c != "none"))
        .filter_map(|f| f["height"].as_u64().and_then(|h| u32::try_from(h).ok()))
        .collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();
    Ok(MediaInfo {
        title: v["title"].as_str().unwrap_or("video").to_owned(),
        duration: v["duration"].as_f64(),
        uploader: v["uploader"]
            .as_str()
            .or(v["channel"].as_str())
            .map(str::to_owned),
        heights,
        has_video: has("vcodec"),
        has_audio: has("acodec"),
        page: v["webpage_url"].as_str().unwrap_or(url).to_owned(),
    })
}

pub struct MediaJob {
    pub ytdlp: PathBuf,
    pub ffmpeg: Option<PathBuf>,
    pub url: String,
    pub cookies: Option<PathBuf>,
    pub folder: PathBuf,
    pub spec: MediaSpec,
    pub connections: u32,
    /// The file's name without its extension, as the person chose it.
    pub stem: String,
}

/// The extractor's arguments for a job.
pub fn arguments(job: &MediaJob) -> Vec<String> {
    let mut a: Vec<String> = [
        "--no-playlist", "--newline", "--no-warnings", "--no-mtime", "--continue", "--progress",
        "--progress-template", "download:EONP %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s",
        "--print", "after_move:EONF %(filepath)s", "--no-simulate",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    // The name chosen in the add sheet; the extractor adds the extension.
    a.push("-o".into());
    a.push(format!("{}.%(ext)s", job.stem.replace('%', "%%")));
    a.push("--concurrent-fragments".into());
    a.push(job.connections.clamp(1, 16).to_string());
    a.push("-P".into());
    a.push(job.folder.to_string_lossy().into_owned());
    match &job.spec.pick {
        Pick::Best => a.extend(["-f".into(), "bv*+ba/b".into()]),
        Pick::Height(h) => a.extend(["-f".into(), format!("bv*[height<={h}]+ba/b[height<={h}]")]),
        Pick::VideoOnly => a.extend(["-f".into(), "bv*/b".into()]),
        Pick::AudioOnly => {
            a.extend(["-f".into(), "ba/b".into(), "-x".into()]);
            if let Some(c) = job
                .spec
                .container
                .as_deref()
                .filter(|c| ["mp3", "m4a", "opus", "flac", "wav"].contains(c))
            {
                a.extend(["--audio-format".into(), c.to_owned()]);
            }
        }
    }
    if !matches!(job.spec.pick, Pick::AudioOnly) {
        if let Some(c) = job
            .spec
            .container
            .as_deref()
            .filter(|c| ["mp4", "mkv", "webm"].contains(c))
        {
            a.extend([
                "--merge-output-format".into(),
                c.to_owned(),
                "--remux-video".into(),
                c.to_owned(),
            ]);
        }
    }
    if matches!(job.spec.pick, Pick::VideoOnly) {
        a.extend(["--postprocessor-args".into(), "ffmpeg:-an".into()]);
    }
    if let Some(f) = &job.ffmpeg {
        a.push("--ffmpeg-location".into());
        a.push(f.to_string_lossy().into_owned());
    }
    if let Some(c) = &job.cookies {
        a.push("--cookies".into());
        a.push(c.to_string_lossy().into_owned());
    }
    a.push("--".into());
    a.push(job.url.clone());
    a
}

#[cfg(windows)]
async fn kill_tree(child: &mut tokio::process::Child) {
    // The extractor starts a child of its own; end the whole tree.
    if let Some(pid) = child.id() {
        let _ = tools::command(Path::new("taskkill"))
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .status()
            .await;
    }
    let _ = child.kill().await;
}

#[cfg(not(windows))]
async fn kill_tree(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
}

/// Download what the page plays. A stop ends the extractor; its partial file
/// stays, and the next start continues it.
pub async fn run(
    job: MediaJob,
    live: Arc<Live>,
    mut stop: watch::Receiver<bool>,
) -> Result<Outcome, Error> {
    tokio::fs::create_dir_all(&job.folder)
        .await
        .map_err(|e| Error::Io(e.to_string()))?;
    let mut cmd = tools::command(&job.ytdlp);
    cmd.args(arguments(&job))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| Error::Io(e.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Io("no output".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Io("no errors stream".into()))?;
    let err_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut s).await;
        s
    });
    let mut reader = BufReader::new(stdout).lines();
    let mut finished: Option<PathBuf> = None;
    // Video and sound may come as two downloads: count what is done before.
    let (mut prior_files, mut file_done, mut file_total) = (0u64, 0u64, 0u64);
    live.connections
        .store(job.connections.clamp(1, 16), Ordering::Relaxed);
    loop {
        let got = tokio::select! {
            next = reader.next_line() => next.map_err(|e| Error::Io(e.to_string()))?,
            _ = stop.changed() => {
                kill_tree(&mut child).await;
                live.connections.store(0, Ordering::Relaxed);
                return Ok(Outcome::Stopped);
            }
        };
        let Some(text) = got else { break };
        if let Some(rest) = text.strip_prefix("EONP ") {
            let mut parts = rest.split_whitespace().map(|p| p.parse::<f64>().ok());
            let done = parts.next().flatten().unwrap_or(0.0) as u64;
            let total = parts
                .next()
                .flatten()
                .or(parts.next().flatten())
                .unwrap_or(0.0) as u64;
            if done + 1024 * 1024 < file_done && file_total > 0 {
                prior_files += file_total;
            }
            let grown = done.saturating_sub(file_done);
            if done >= file_done {
                live.received.fetch_add(grown, Ordering::Relaxed);
            } else {
                live.received.fetch_add(done, Ordering::Relaxed);
            }
            file_done = done;
            file_total = total.max(done);
            let end = prior_files + file_total.max(1);
            *live.segments() = vec![Segment {
                id: 0,
                start: 0,
                pos: prior_files + done,
                end,
                owner: Some(1),
            }];
        } else if let Some(path) = text.strip_prefix("EONF ") {
            finished = Some(PathBuf::from(path.trim()));
        }
    }
    let status = child.wait().await.map_err(|e| Error::Io(e.to_string()))?;
    live.connections.store(0, Ordering::Relaxed);
    let errors = err_task.await.unwrap_or_default();
    if !status.success() {
        return Err(Error::Network(last_error(&errors)));
    }
    let path = finished
        .ok_or_else(|| Error::Io("the extractor did not say where the file went".into()))?;
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    *live.segments() = vec![Segment {
        id: 0,
        start: 0,
        pos: size,
        end: size.max(1),
        owner: None,
    }];
    Ok(Outcome::Done(path))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn job(pick: Pick, container: Option<&str>) -> MediaJob {
        MediaJob {
            ytdlp: PathBuf::from("yt-dlp"),
            ffmpeg: Some(PathBuf::from("/tools/ffmpeg")),
            url: "https://example.org/watch?v=1".into(),
            cookies: None,
            folder: PathBuf::from("/downloads/Video"),
            spec: MediaSpec {
                pick,
                container: container.map(str::to_owned),
            },
            connections: 8,
            stem: "Talk 100%".into(),
        }
    }

    #[test]
    fn choices_become_the_right_arguments() {
        let a = arguments(&job(Pick::Height(720), Some("mp4")));
        assert!(a
            .windows(2)
            .any(|w| w[0] == "-f" && w[1] == "bv*[height<=720]+ba/b[height<=720]"));
        assert!(a
            .windows(2)
            .any(|w| w[0] == "--merge-output-format" && w[1] == "mp4"));
        assert_eq!(a.last().unwrap(), "https://example.org/watch?v=1");
        assert_eq!(
            a[a.len() - 2],
            "--",
            "the address can never be read as an option"
        );
        assert!(a
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "Talk 100%%.%(ext)s"));
        let audio = arguments(&job(Pick::AudioOnly, Some("mp3")));
        assert!(
            audio.contains(&"-x".to_owned())
                && audio
                    .windows(2)
                    .any(|w| w[0] == "--audio-format" && w[1] == "mp3")
        );
        assert!(!audio.contains(&"--merge-output-format".to_owned()));
        let silent = arguments(&job(Pick::VideoOnly, None));
        assert!(silent
            .windows(2)
            .any(|w| w[0] == "--postprocessor-args" && w[1] == "ffmpeg:-an"));
    }

    #[test]
    fn cookies_become_a_netscape_file_for_that_site_only() {
        let dir = std::env::temp_dir().join(format!("eon-dm-cookies-{}", std::process::id()));
        let path = dir.join("c.txt");
        cookie_file(&path, "https://video.example.org/watch", "a=1; b=two").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(".video.example.org\tTRUE\t/\tTRUE\t0\ta\t1"));
        assert!(text.contains("\tb\ttwo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_are_the_extractors_own_words() {
        assert_eq!(
            last_error("[info] x\nERROR: [site] 123: Private video\n"),
            "[site] 123: Private video"
        );
    }
}
