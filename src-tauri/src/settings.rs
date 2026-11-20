// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The person's settings, in `settings.json` in the data folder.

use eon_dm_engine::Settings as EngineSettings;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum After {
    Nothing,
    Sleep,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Scheduler {
    pub enabled: bool,
    /// `HH:MM`, local time.
    pub start: String,
    /// `HH:MM`, or empty for "until the queue is empty".
    pub stop: String,
    /// 0 = Monday … 6 = Sunday.
    pub days: Vec<u8>,
    pub after: After,
}

impl Default for Scheduler {
    fn default() -> Self {
        Scheduler {
            enabled: false,
            start: "02:00".into(),
            stop: "07:00".into(),
            days: (0..7).collect(),
            after: After::Nothing,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // independent on/off settings, as the person sees them
pub struct AppSettings {
    /// `system`, `en` or `tr`.
    pub language: String,
    /// `system`, `dark` or `light`.
    pub theme: String,
    /// Where downloads go; empty means the system's downloads folder.
    pub folder: String,
    /// One folder per category (Video, Music, …) under `folder`.
    pub sort_by_category: bool,
    pub engine: EngineSettings,
    /// Offer to download links copied to the clipboard.
    pub watch_clipboard: bool,
    /// Closing the window keeps downloads going from the tray.
    pub close_to_tray: bool,
    pub notify: bool,
    pub scheduler: Scheduler,
    /// The tray hint is shown once.
    pub tray_hint_shown: bool,
    /// Browsers know where this program is (native messaging, per user). On by
    /// default and renewed at every start; "Disconnect" turns it off.
    pub browser_integration: bool,
    /// The browser walkthrough has been seen (or dismissed) once.
    pub onboarded: bool,
    /// Ask, when adding, which format to save in (sound only, a still picture
    /// from a GIF, ...); otherwise the original is kept.
    pub ask_format: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            language: "system".into(),
            theme: "system".into(),
            folder: String::new(),
            sort_by_category: true,
            engine: EngineSettings::default(),
            watch_clipboard: true,
            close_to_tray: true,
            notify: true,
            scheduler: Scheduler::default(),
            tray_hint_shown: false,
            browser_integration: true,
            onboarded: false,
            ask_format: false,
        }
    }
}

impl AppSettings {
    pub fn load(dir: &Path) -> AppSettings {
        std::fs::read(dir.join("settings.json"))
            .ok()
            .and_then(|t| serde_json::from_slice(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) {
        if let Ok(text) = serde_json::to_vec_pretty(self) {
            let tmp = dir.join("settings.json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, dir.join("settings.json"));
            }
        }
    }

    /// Clamp what a hand-edited file or an old version might hold.
    pub fn sane(mut self) -> AppSettings {
        self.engine.connections = self.engine.connections.clamp(1, 32);
        self.engine.max_active = self.engine.max_active.clamp(1, 20);
        if !["system", "en", "tr"].contains(&self.language.as_str()) {
            self.language = "system".into();
        }
        if !["system", "dark", "light"].contains(&self.theme.as_str()) {
            self.theme = "system".into();
        }
        self.scheduler.days.retain(|d| *d < 7);
        self
    }

    pub fn root(&self, fallback: &Path) -> PathBuf {
        if self.folder.trim().is_empty() {
            fallback.to_owned()
        } else {
            PathBuf::from(self.folder.trim())
        }
    }
}
