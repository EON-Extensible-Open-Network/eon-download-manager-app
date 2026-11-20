// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! File names: where they come from, and what they may contain.
//!
//! A name arrives from a server (`Content-Disposition`), a URL path, a browser
//! extension or a person. All of those are untrusted: a name never carries a
//! directory, never is a reserved Windows device, never starts with a dot, and
//! never replaces a file that is already there.

use std::path::{Path, PathBuf};

/// The name in a `Content-Disposition` header (RFC 6266). `filename*` (RFC
/// 5987, percent-encoded UTF-8) wins over `filename`.
pub fn from_disposition(header: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for part in split_params(header) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "filename*" {
            // charset'language'percent-encoded
            let mut pieces = value.splitn(3, '\'');
            let charset = pieces.next().unwrap_or_default();
            let _language = pieces.next();
            if let Some(encoded) = pieces.next() {
                let bytes: Vec<u8> = percent_encoding::percent_decode_str(encoded).collect();
                let text = if charset.eq_ignore_ascii_case("utf-8") {
                    String::from_utf8(bytes).ok()
                } else {
                    // ISO-8859-1: every byte is the code point.
                    Some(bytes.iter().map(|&b| char::from(b)).collect())
                };
                extended = text.filter(|t| !t.is_empty());
            }
        } else if key == "filename" {
            let unquoted = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .map_or_else(|| value.to_owned(), |v| v.replace("\\\"", "\""));
            plain = Some(unquoted).filter(|t| !t.is_empty());
        }
    }
    extended
        .or(plain)
        .map(|n| clean(&n))
        .filter(|n| !n.is_empty())
}

/// `;`-separated parameters, respecting quotes.
fn split_params(header: &str) -> Vec<String> {
    let (mut out, mut current, mut quoted) = (Vec::new(), String::new(), false);
    for c in header.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ';' if !quoted => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    out.push(current);
    out
}

/// The last path segment of a URL, percent-decoded.
pub fn from_url(url: &url::Url) -> Option<String> {
    let last = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    let decoded = percent_encoding::percent_decode_str(last).decode_utf8_lossy();
    Some(clean(&decoded)).filter(|n| !n.is_empty())
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A name that is safe as one path component on Windows, macOS and Linux.
pub fn clean(name: &str) -> String {
    let out: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // No leading dots (hidden files, `..`), no trailing dots or spaces (Windows
    // drops them silently).
    let mut out = out
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' '])
        .to_owned();
    let stem = out
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        out.insert(0, '_');
    }
    // 255 bytes is the common limit; keep the extension when shortening.
    if out.len() > 240 {
        let ext = Path::new(&out)
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .filter(|e| e.len() <= 16)
            .unwrap_or_default();
        let mut stem: String = out.chars().take(200).collect();
        while stem.len() > 200 {
            stem.pop();
        }
        out = if ext.is_empty() {
            stem
        } else {
            format!("{stem}.{ext}")
        };
    }
    out
}

/// An extension for a name that has none, from the MIME type.
pub fn extension_for(mime: &str) -> Option<&'static str> {
    let mime = mime.split(';').next().unwrap_or_default().trim();
    Some(match mime {
        "application/zip" => "zip",
        "application/pdf" => "pdf",
        "application/x-msdownload" | "application/vnd.microsoft.portable-executable" => "exe",
        "application/x-iso9660-image" => "iso",
        "application/x-7z-compressed" => "7z",
        "application/vnd.rar" | "application/x-rar-compressed" => "rar",
        "application/gzip" => "gz",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/mp2t" => "ts",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/flac" => "flac",
        "audio/ogg" => "ogg",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "text/plain" => "txt",
        _ => return None,
    })
}

/// `folder/name`, or `folder/name (1).ext`, `(2)` … — the first that exists
/// neither as a file nor as a download still in progress.
pub fn unique(folder: &Path, name: &str) -> PathBuf {
    let taken = |p: &Path| {
        p.exists() || {
            let mut part = p.as_os_str().to_owned();
            part.push(".part");
            Path::new(&part).exists()
        }
    };
    let first = folder.join(name);
    if !taken(&first) {
        return first;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map_or_else(|| name.to_owned(), |s| s.to_string_lossy().into_owned());
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 1..10_000 {
        let candidate = match &ext {
            Some(ext) => folder.join(format!("{stem} ({n}).{ext}")),
            None => folder.join(format!("{stem} ({n})")),
        };
        if !taken(&candidate) {
            return candidate;
        }
    }
    folder.join(format!("{stem} ({}).download", std::process::id()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn disposition_names_including_rfc5987() {
        assert_eq!(
            from_disposition(r#"attachment; filename="report 2026.pdf""#).as_deref(),
            Some("report 2026.pdf")
        );
        assert_eq!(
            from_disposition("attachment; filename=plain.zip").as_deref(),
            Some("plain.zip")
        );
        assert_eq!(
            from_disposition(
                r#"attachment; filename="fallback.txt"; filename*=UTF-8''%C3%BC%C3%A7%C3%BCnc%C3%BC%20rapor.txt"#
            )
            .as_deref(),
            Some("üçüncü rapor.txt")
        );
        assert_eq!(
            from_disposition(r#"attachment; filename="a;b.zip""#).as_deref(),
            Some("a;b.zip")
        );
        assert_eq!(from_disposition("inline").as_deref(), None);
    }

    #[test]
    fn names_never_escape_their_folder() {
        assert_eq!(clean("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(clean("..\\..\\boot.ini"), "_.._boot.ini");
        assert_eq!(clean(".hidden"), "hidden");
        assert_eq!(clean("CON.txt"), "_CON.txt");
        assert_eq!(clean("lpt1"), "_lpt1");
        assert_eq!(clean("trailing. . "), "trailing");
        assert_eq!(clean("a\u{0}b\nc"), "a_b_c");
        let long = format!("{}.tar.gz", "x".repeat(400));
        let short = clean(&long);
        assert!(
            short.len() <= 240 && Path::new(&short).extension().is_some_and(|e| e == "gz"),
            "{short}"
        );
    }

    #[test]
    fn url_names_are_decoded() {
        let u = url::Url::parse("https://example.org/files/My%20File%C3%A7.zip?x=1").unwrap();
        assert_eq!(from_url(&u).as_deref(), Some("My Fileç.zip"));
        let root = url::Url::parse("https://example.org/").unwrap();
        assert_eq!(from_url(&root), None);
    }

    #[test]
    fn existing_files_are_never_replaced() {
        let dir = std::env::temp_dir().join(format!("eon-dm-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.zip"), b"x").unwrap();
        std::fs::write(dir.join("a (1).zip.part"), b"x").unwrap();
        assert_eq!(unique(&dir, "a.zip"), dir.join("a (2).zip"));
        assert_eq!(unique(&dir, "b.zip"), dir.join("b.zip"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
