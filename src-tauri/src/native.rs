// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The browser hand-off (eon-download-manager-spec, `docs/handoff.md`).
//!
//! The browser starts this executable as a native messaging host. In that mode
//! it reads one message, checks it, leaves it in the data folder, starts or
//! wakes the window with it, answers, and exits. It never downloads anything
//! itself. Registration — telling browsers where the host is — happens only
//! when the person presses *Connect browsers*.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const HOST: &str = "org.eon.download_manager";
/// The Chromium extension's id: fixed by the public key in its manifest.
pub const CHROMIUM_ID: &str = "piagpgnijlkmaacfoiknlbofkfnbjjgn";
pub const FIREFOX_ID: &str = "download-manager@eon-extensible-open-network.org";
const MAX_MESSAGE: u32 = 8 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Item {
    pub url: String,
    #[serde(default)]
    pub final_url: Option<String>,
    #[serde(default)]
    pub referrer: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub page_title: Option<String>,
    #[serde(default)]
    pub media: Option<Value>,
    #[serde(default)]
    pub replaces: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handoff {
    pub kind: String,
    #[serde(default = "ask")]
    pub start: String,
    #[serde(default)]
    pub items: Vec<Item>,
}

fn ask() -> String {
    "ask".into()
}

/// Was this process started by a browser as a native messaging host?
/// Chromium passes the caller's origin; Firefox the manifest path and the
/// extension id.
pub fn is_host_launch(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| {
        a.starts_with("chrome-extension://")
            || a == FIREFOX_ID
            || a.ends_with(&format!("{HOST}.json"))
    })
}

fn http_url(u: &str) -> bool {
    (u.starts_with("https://") || u.starts_with("http://")) && !u.contains(' ') && u.len() <= 16384
}

/// Check a message against the hand-off rules. The schema in the spec is the
/// reference; this is what the application refuses at its door.
pub fn check(message: &Value) -> Result<Handoff, String> {
    if message["handoff"] != 0 {
        return Err("not a version 0 hand-off".into());
    }
    let handoff: Handoff = serde_json::from_value(message.clone()).map_err(|e| e.to_string())?;
    let kinds = ["ping", "download", "links", "media", "refresh"];
    if !kinds.contains(&handoff.kind.as_str()) {
        return Err(format!("unknown kind {}", handoff.kind));
    }
    if !["ask", "now", "later"].contains(&handoff.start.as_str()) {
        return Err("start is ask, now or later".into());
    }
    let n = handoff.items.len();
    match handoff.kind.as_str() {
        "ping" if n > 0 => return Err("a ping carries no items".into()),
        "download" | "media" | "refresh" if n != 1 => return Err("exactly one item".into()),
        "links" if n == 0 || n > 5000 => return Err("1 to 5000 links".into()),
        _ => {}
    }
    for item in &handoff.items {
        if !http_url(&item.url) || item.final_url.as_deref().is_some_and(|u| !http_url(u)) {
            return Err("only http and https addresses".into());
        }
    }
    Ok(handoff)
}

fn read_message(input: &mut impl Read) -> Result<Value, String> {
    let mut len = [0u8; 4];
    input.read_exact(&mut len).map_err(|e| e.to_string())?;
    let len = u32::from_ne_bytes(len);
    if len > MAX_MESSAGE {
        return Err("too_large".into());
    }
    let mut body = vec![0u8; len as usize];
    input.read_exact(&mut body).map_err(|e| e.to_string())?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

fn write_message(output: &mut impl Write, value: &Value) {
    let body = value.to_string();
    let _ = output.write_all(&(body.len() as u32).to_ne_bytes());
    let _ = output.write_all(body.as_bytes());
    let _ = output.flush();
}

/// The host: one message in, the window told, one reply out.
pub fn run_host(data_dir: &Path) -> i32 {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let reply = match read_message(&mut stdin) {
        Err(e) if e == "too_large" => {
            json!({ "ok": false, "error": { "code": "too_large", "message": "the message is over 8 MiB" } })
        }
        Err(e) => json!({ "ok": false, "error": { "code": "invalid", "message": e } }),
        Ok(message) => match check(&message) {
            Err(e) => json!({ "ok": false, "error": { "code": "invalid", "message": e } }),
            Ok(h) if h.kind == "ping" => {
                json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })
            }
            Ok(h) => match deliver(data_dir, &h) {
                Ok(()) => json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }),
                Err(e) => json!({ "ok": false, "error": { "code": "unavailable", "message": e } }),
            },
        },
    };
    write_message(&mut stdout, &reply);
    0
}

/// Leave the hand-off in the inbox and start the window with it; a window that
/// is already running receives it through the single-instance plugin.
fn deliver(data_dir: &Path, handoff: &Handoff) -> Result<(), String> {
    let inbox = data_dir.join("inbox");
    std::fs::create_dir_all(&inbox).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let file = inbox.join(format!("{stamp}.json"));
    std::fs::write(
        &file,
        serde_json::to_vec(handoff).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    spawn_detached(
        &exe,
        &["--handoff".into(), file.to_string_lossy().into_owned()],
    )
}

#[cfg(windows)]
fn spawn_detached(exe: &Path, args: &[String]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    // The browser reads the host's output until it closes: the window must not
    // inherit it, or the browser's side never ends.
    let spawn = |flags: u32| {
        std::process::Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    // Out of the browser's job object if it allows that, so closing the
    // browser never closes the downloads.
    spawn(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| spawn(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn spawn_detached(exe: &Path, args: &[String]) -> Result<(), String> {
    std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Read and delete a hand-off left in the inbox — and only there: `--handoff`
/// on a command line can never make the program read or delete another file.
pub fn take(data_dir: &Path, file: &Path) -> Option<Handoff> {
    let inbox = data_dir.join("inbox").canonicalize().ok()?;
    let inbox_file = file.canonicalize().ok()?;
    if inbox_file.parent() != Some(inbox.as_path()) {
        return None;
    }
    let text = std::fs::read(&inbox_file).ok()?;
    let _ = std::fs::remove_file(&inbox_file);
    serde_json::from_slice(&text).ok()
}

// ---- registration --------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Browser {
    Chrome,
    Edge,
    Brave,
    Vivaldi,
    Chromium,
    Firefox,
}

impl Browser {
    const ALL: [Browser; 6] = [
        Browser::Chrome,
        Browser::Edge,
        Browser::Brave,
        Browser::Vivaldi,
        Browser::Chromium,
        Browser::Firefox,
    ];

    fn firefox(self) -> bool {
        matches!(self, Browser::Firefox)
    }

    #[cfg(windows)]
    fn key(self) -> String {
        let base = match self {
            Browser::Chrome => r"Software\Google\Chrome",
            Browser::Edge => r"Software\Microsoft\Edge",
            Browser::Brave => r"Software\BraveSoftware\Brave-Browser",
            Browser::Vivaldi => r"Software\Vivaldi",
            Browser::Chromium => r"Software\Chromium",
            Browser::Firefox => r"Software\Mozilla",
        };
        format!(r"{base}\NativeMessagingHosts\{HOST}")
    }

    #[cfg(not(windows))]
    fn folder(self, home: &Path) -> PathBuf {
        let mac = cfg!(target_os = "macos");
        let rel = match (self, mac) {
            (Browser::Chrome, true) => {
                "Library/Application Support/Google/Chrome/NativeMessagingHosts"
            }
            (Browser::Edge, true) => {
                "Library/Application Support/Microsoft Edge/NativeMessagingHosts"
            }
            (Browser::Brave, true) => {
                "Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts"
            }
            (Browser::Vivaldi, true) => "Library/Application Support/Vivaldi/NativeMessagingHosts",
            (Browser::Chromium, true) => {
                "Library/Application Support/Chromium/NativeMessagingHosts"
            }
            (Browser::Firefox, true) => "Library/Application Support/Mozilla/NativeMessagingHosts",
            (Browser::Chrome, false) => ".config/google-chrome/NativeMessagingHosts",
            (Browser::Edge, false) => ".config/microsoft-edge/NativeMessagingHosts",
            (Browser::Brave, false) => ".config/BraveSoftware/Brave-Browser/NativeMessagingHosts",
            (Browser::Vivaldi, false) => ".config/vivaldi/NativeMessagingHosts",
            (Browser::Chromium, false) => ".config/chromium/NativeMessagingHosts",
            (Browser::Firefox, false) => ".mozilla/native-messaging-hosts",
        };
        home.join(rel)
    }
}

fn manifest(browser: Browser, exe: &Path) -> Value {
    let mut m = json!({
        "name": HOST,
        "description": "EON Download Manager",
        "path": exe.to_string_lossy(),
        "type": "stdio",
    });
    if browser.firefox() {
        m["allowed_extensions"] = json!([FIREFOX_ID]);
    } else {
        m["allowed_origins"] = json!([format!("chrome-extension://{CHROMIUM_ID}/")]);
    }
    m
}

/// Which browsers are connected to this executable.
pub fn status(data_dir: &Path) -> Vec<(Browser, bool)> {
    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    Browser::ALL
        .iter()
        .map(|&b| (b, registered(b, data_dir, &exe)))
        .collect()
}

#[cfg(windows)]
fn registered(browser: Browser, data_dir: &Path, exe: &Path) -> bool {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey(browser.key()) else {
        return false;
    };
    let Ok(path): Result<String, _> = key.get_value("") else {
        return false;
    };
    let wanted = manifest_path(data_dir, browser);
    Path::new(&path) == wanted && points_at(&wanted, exe)
}

#[cfg(not(windows))]
fn registered(browser: Browser, _data_dir: &Path, exe: &Path) -> bool {
    std::env::var_os("HOME").is_some_and(|h| {
        points_at(
            &browser.folder(Path::new(&h)).join(format!("{HOST}.json")),
            exe,
        )
    })
}

fn points_at(manifest: &Path, exe: &Path) -> bool {
    std::fs::read(manifest)
        .ok()
        .and_then(|t| serde_json::from_slice::<Value>(&t).ok())
        .is_some_and(|m| m["path"].as_str().is_some_and(|p| Path::new(p) == exe))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn manifest_path(data_dir: &Path, browser: Browser) -> PathBuf {
    let flavour = if browser.firefox() {
        "firefox"
    } else {
        "chromium"
    };
    data_dir
        .join("native-host")
        .join(format!("{HOST}.{flavour}.json"))
}

/// Tell every supported browser where the host is. Per user; no
/// administrator rights.
pub fn connect(data_dir: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    for browser in Browser::ALL {
        install(browser, data_dir, &exe)?;
    }
    Ok(())
}

#[cfg(windows)]
fn install(browser: Browser, data_dir: &Path, exe: &Path) -> Result<(), String> {
    let path = manifest_path(data_dir, browser);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&manifest(browser, exe)).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(browser.key())
        .map_err(|e| e.to_string())?;
    key.set_value("", &path.to_string_lossy().into_owned())
        .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn install(browser: Browser, _data_dir: &Path, exe: &Path) -> Result<(), String> {
    let home = std::env::var_os("HOME").ok_or("no home folder")?;
    let dir = browser.folder(Path::new(&home));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(
        dir.join(format!("{HOST}.json")),
        serde_json::to_vec_pretty(&manifest(browser, exe)).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// Undo [`connect`].
pub fn disconnect(data_dir: &Path) {
    for browser in Browser::ALL {
        #[cfg(windows)]
        {
            let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
            let _ = hkcu.delete_subkey(browser.key());
            let _ = std::fs::remove_file(manifest_path(data_dir, browser));
        }
        #[cfg(not(windows))]
        {
            let _ = data_dir;
            if let Some(home) = std::env::var_os("HOME") {
                let _ = std::fs::remove_file(
                    browser
                        .folder(Path::new(&home))
                        .join(format!("{HOST}.json")),
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_checked_at_the_door() {
        let ok = json!({ "handoff": 0, "kind": "download", "items": [{ "url": "https://example.org/a.zip", "size": 5 }] });
        assert_eq!(check(&ok).unwrap().items[0].size, Some(5));
        for bad in [
            json!({ "handoff": 1, "kind": "ping" }),
            json!({ "handoff": 0, "kind": "ping", "items": [{ "url": "https://x.org" }] }),
            json!({ "handoff": 0, "kind": "download", "items": [] }),
            json!({ "handoff": 0, "kind": "links", "items": [{ "url": "file:///etc/passwd" }] }),
            json!({ "handoff": 0, "kind": "links", "items": [{ "url": "javascript:alert(1)" }] }),
            json!({ "handoff": 0, "kind": "steal", "items": [] }),
        ] {
            assert!(check(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_wire_format_is_length_then_json() {
        let mut wire = Vec::new();
        write_message(&mut wire, &json!({ "handoff": 0, "kind": "ping" }));
        assert_eq!(
            u32::from_ne_bytes(wire[..4].try_into().unwrap()) as usize,
            wire.len() - 4
        );
        assert_eq!(read_message(&mut wire.as_slice()).unwrap()["kind"], "ping");
        let mut huge = (MAX_MESSAGE + 1).to_ne_bytes().to_vec();
        huge.extend_from_slice(b"{}");
        assert_eq!(read_message(&mut huge.as_slice()).unwrap_err(), "too_large");
    }

    #[test]
    fn browsers_find_us_by_their_own_arguments() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        assert!(is_host_launch(&s(&[
            "eon",
            "chrome-extension://piagpgnijlkmaacfoiknlbofkfnbjjgn/",
            "--parent-window=0"
        ])));
        assert!(is_host_launch(&s(&[
            "eon",
            "/home/a/.mozilla/native-messaging-hosts/org.eon.download_manager.json",
            FIREFOX_ID
        ])));
        assert!(!is_host_launch(&s(&["eon", "--selftest"])));
        let chromium = manifest(Browser::Chrome, Path::new("/opt/eon"));
        assert_eq!(
            chromium["allowed_origins"][0],
            format!("chrome-extension://{CHROMIUM_ID}/")
        );
        assert!(manifest(Browser::Firefox, Path::new("/opt/eon"))["allowed_extensions"].is_array());
    }
}
