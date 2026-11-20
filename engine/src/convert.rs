// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Saving a finished file in another format: a video as MP4 or MKV, its sound
//! alone as MP3 or M4A, its picture without sound, an animated GIF as a still
//! picture or a video. Done with the converter (see `tools`); the original is
//! replaced only when the new file exists and is not empty.

use crate::tools;
use crate::Error;
use std::path::{Path, PathBuf};

/// The format to save as. `None` anywhere means "keep the original".
pub fn extension(target: &str) -> Option<&'static str> {
    Some(match target {
        "mp4" => "mp4",
        "mkv" => "mkv",
        "mp3" => "mp3",
        "m4a" => "m4a",
        "png" => "png",
        "jpg" => "jpg",
        _ => return None,
    })
}

/// The converter's arguments: a fast lossless attempt first, a re-encode if
/// the first cannot hold the streams.
pub fn attempts(target: &str, input: &Path) -> Vec<Vec<String>> {
    let is_gif = input
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"));
    let v = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<String>>();
    match target {
        "mp4" if is_gif => vec![v(&[
            "-movflags",
            "+faststart",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "scale=trunc(iw/2)*2:trunc(ih/2)*2",
            "-c:v",
            "libx264",
        ])],
        "mp4" => vec![
            v(&["-map", "0", "-c", "copy", "-movflags", "+faststart"]),
            v(&[
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "20",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-movflags",
                "+faststart",
            ]),
        ],
        "mkv" => vec![
            v(&["-map", "0", "-c", "copy"]),
            v(&[
                "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-c:a", "aac",
            ]),
        ],
        "mp3" => vec![v(&["-vn", "-c:a", "libmp3lame", "-q:a", "2"])],
        "m4a" => vec![
            v(&["-vn", "-c:a", "copy"]),
            v(&["-vn", "-c:a", "aac", "-b:a", "192k"]),
        ],
        "video_only" => vec![v(&["-an", "-c:v", "copy"])],
        "png" => vec![v(&["-frames:v", "1"])],
        "jpg" => vec![v(&["-frames:v", "1", "-q:v", "2"])],
        _ => Vec::new(),
    }
}

/// Convert `input` to `target`, beside it; the original goes once the result
/// is there. Returns the new path.
pub async fn convert(ffmpeg: &Path, input: &Path, target: &str) -> Result<PathBuf, Error> {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let folder = input.parent().map(Path::to_path_buf).unwrap_or_default();
    let ext = extension(target)
        .map(str::to_owned)
        .or_else(|| input.extension().map(|e| e.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let base = if target == "video_only" {
        format!("{stem} (video only).{ext}")
    } else {
        format!("{stem}.{ext}")
    };
    let output = crate::name::unique(&folder, &base);
    let mut last = String::from("no way to convert to this format");
    for args in attempts(target, input) {
        let out = tools::command(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(input)
            .args(&args)
            .arg(&output)
            .output()
            .await
            .map_err(|e| Error::Io(e.to_string()))?;
        if out.status.success() && std::fs::metadata(&output).is_ok_and(|m| m.len() > 0) {
            if output != input {
                let _ = std::fs::remove_file(input);
            }
            return Ok(output);
        }
        let _ = std::fs::remove_file(&output);
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .last()
            .unwrap_or("conversion failed")
            .clone_into(&mut last);
    }
    Err(Error::Unsupported(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lossless_try_comes_before_a_re_encode() {
        let a = attempts("mp4", Path::new("talk.webm"));
        assert_eq!(a.len(), 2);
        assert!(a[0].contains(&"copy".to_owned()));
        assert!(a[1].contains(&"libx264".to_owned()));
        assert!(attempts("mp4", Path::new("cat.GIF"))[0].contains(&"yuv420p".to_owned()));
        assert!(attempts("png", Path::new("cat.gif"))[0].contains(&"-frames:v".to_owned()));
        assert!(attempts("nonsense", Path::new("a.mp4")).is_empty());
        assert_eq!(extension("video_only"), None);
        assert_eq!(extension("m4a"), Some("m4a"));
    }
}
