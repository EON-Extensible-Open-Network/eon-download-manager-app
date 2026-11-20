// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The `.eondl` resume state beside a `.part` file
//! (eon-download-manager-spec, `docs/resume-state.md`).

use crate::plan::Segment;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validators {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

impl Validators {
    /// The value for `If-Range`: a strong `ETag` if there is one, otherwise the
    /// date. A weak `ETag` (`W/"…"`) is not allowed in `If-Range`.
    pub fn if_range(&self) -> Option<&str> {
        self.etag
            .as_deref()
            .filter(|e| !e.starts_with("W/"))
            .or(self.last_modified.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HlsProgress {
    pub playlist: String,
    pub done: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeState {
    pub eondl: u8,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referrer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookies: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    pub file: String,
    pub length: Option<u64>,
    pub resumable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validators: Option<Validators>,
    pub segments: Vec<Segment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hls: Option<HlsProgress>,
}

/// `report.pdf` → `report.pdf.part`, `report.pdf.eondl`.
pub fn part_path(target: &Path) -> PathBuf {
    with_suffix(target, ".part")
}

pub fn state_path(target: &Path) -> PathBuf {
    with_suffix(target, ".eondl")
}

/// The folder an HLS download keeps its media segments in until they are joined.
pub fn pieces_dir(target: &Path) -> PathBuf {
    with_suffix(target, ".eondl.d")
}

fn with_suffix(target: &Path, suffix: &str) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

impl ResumeState {
    /// Write atomically: a temporary file, flushed to disk, renamed over the
    /// old state. After a crash there is always a whole state file, old or new.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = with_suffix(path, ".tmp");
        let text = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&text)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    pub fn load(path: &Path) -> Option<ResumeState> {
        let text = std::fs::read(path).ok()?;
        let state: ResumeState = serde_json::from_slice(&text).ok()?;
        (state.eondl == 0).then_some(state)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_state_survives_the_round_trip_and_matches_the_spec_shape() {
        let dir = std::env::temp_dir().join(format!("eon-dm-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = ResumeState {
            eondl: 0,
            url: "https://example.org/big.iso".into(),
            final_url: None,
            referrer: None,
            cookies: Some("a=b".into()),
            user_agent: None,
            file: "big.iso".into(),
            length: Some(100),
            resumable: true,
            validators: Some(Validators {
                etag: Some("\"x\"".into()),
                last_modified: None,
            }),
            segments: crate::plan::initial(100),
            hls: None,
        };
        let path = state_path(&dir.join("big.iso"));
        state.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("final_url") && !text.contains("owner") && !text.contains("\"id\""),
            "{text}"
        );
        assert_eq!(ResumeState::load(&path).unwrap(), state);
        assert_eq!(part_path(&dir.join("big.iso")), dir.join("big.iso.part"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn weak_etags_are_not_used_for_if_range() {
        let v = Validators {
            etag: Some("W/\"x\"".into()),
            last_modified: Some("Mon".into()),
        };
        assert_eq!(v.if_range(), Some("Mon"));
    }
}
