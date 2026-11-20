// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Categories: the kind of file decides its folder.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Video,
    Music,
    Documents,
    Compressed,
    Programs,
    Images,
    Other,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Video,
        Category::Music,
        Category::Documents,
        Category::Compressed,
        Category::Programs,
        Category::Images,
        Category::Other,
    ];

    /// The folder under the downloads folder, as a person reads it.
    pub fn folder(self) -> &'static str {
        match self {
            Category::Video => "Video",
            Category::Music => "Music",
            Category::Documents => "Documents",
            Category::Compressed => "Compressed",
            Category::Programs => "Programs",
            Category::Images => "Images",
            Category::Other => "",
        }
    }

    pub fn of(file_name: &str) -> Category {
        let ext = std::path::Path::new(file_name)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg"
            | "ts" | "m2ts" | "3gp" | "ogv" => Category::Video,
            "mp3" | "flac" | "wav" | "aac" | "m4a" | "ogg" | "opus" | "wma" | "aiff" | "alac" => {
                Category::Music
            }
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
            | "txt" | "rtf" | "epub" | "csv" | "md" => Category::Documents,
            "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "zst" | "lz" | "cab" => {
                Category::Compressed
            }
            "exe" | "msi" | "msix" | "iso" | "img" | "dmg" | "pkg" | "deb" | "rpm" | "appimage"
            | "apk" | "flatpak" | "snap" => Category::Programs,
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "svg" | "bmp" | "tiff" | "heic"
            | "raw" => Category::Images,
            _ => Category::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_go_where_people_expect() {
        assert_eq!(Category::of("ubuntu.iso"), Category::Programs);
        assert_eq!(Category::of("Talk.MP4"), Category::Video);
        assert_eq!(Category::of("archive.tar.gz"), Category::Compressed);
        assert_eq!(Category::of("notes"), Category::Other);
        assert_eq!(Category::Other.folder(), "");
    }
}
