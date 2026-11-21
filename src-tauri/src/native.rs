// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The browser hand-off (eon-download-manager-spec, `docs/handoff.md`).
//!
//! The browser starts this executable as a native messaging host. In that mode
//! it reads one message, checks it, leaves it in the data folder's inbox,
//! starts the window if none is running, waits until the window has taken it,
//! answers, and exits. The window reads the inbox itself. Passing the hand-off
//! through a second copy of the program lost it under Firefox (which runs the
//! host in a job object), so nothing depends on such a copy any more, and the
//! answer is not given until the window has it. It never downloads anything
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

/// The extension, carried inside the executable: written out on request so a
/// person can add it to a browser without downloading anything else.
pub const EXTENSION: &[(&str, &[u8])] = &[
    (
        "manifest.json",
        include_bytes!("../../extension/manifest.json"),
    ),
    (
        "background.js",
        include_bytes!("../../extension/background.js"),
    ),
    ("content.js", include_bytes!("../../extension/content.js")),
    ("popup.html", include_bytes!("../../extension/popup.html")),
    ("popup.js", include_bytes!("../../extension/popup.js")),
    ("popup.css", include_bytes!("../../extension/popup.css")),
    (
        "_locales/en/messages.json",
        include_bytes!("../../extension/_locales/en/messages.json"),
    ),
    (
        "_locales/tr/messages.json",
        include_bytes!("../../extension/_locales/tr/messages.json"),
    ),
    (
        "icons/icon-16.png",
        include_bytes!("../../extension/icons/icon-16.png"),
    ),
    (
        "icons/icon-32.png",
        include_bytes!("../../extension/icons/icon-32.png"),
    ),
    (
        "icons/icon-48.png",
        include_bytes!("../../extension/icons/icon-48.png"),
    ),
    (
        "icons/icon-128.png",
        include_bytes!("../../extension/icons/icon-128.png"),
    ),
];

/// Write the extension to `<data>/extension` (replacing an older copy) and
/// return that folder.
pub fn write_extension(data_dir: &Path) -> Result<PathBuf, String> {
    let dir = data_dir.join("extension");
    let _ = std::fs::remove_dir_all(&dir);
    for (rel, bytes) in EXTENSION {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    }
    Ok(dir)
}

/// The extension said hello (on install and on every browser start): remember
/// which browser and when, so the window can say "connected".
fn mark_seen(data_dir: &Path, message: &Value) {
    let browser = message["source"]["browser"].as_str().unwrap_or("other");
    let version = message["source"]["extension"].as_str().unwrap_or("");
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let path = data_dir.join("extension-seen.json");
    let mut seen: Value = std::fs::read(&path)
        .ok()
        .and_then(|t| serde_json::from_slice(&t).ok())
        .unwrap_or_else(|| json!({}));
    seen[browser] = json!({ "version": version, "at": at });
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(path, seen.to_string());
}

/// Browsers whose extension has said hello, with when.
pub fn seen(data_dir: &Path) -> Value {
    std::fs::read(data_dir.join("extension-seen.json"))
        .ok()
        .and_then(|t| serde_json::from_slice(&t).ok())
        .unwrap_or_else(|| json!({}))
}

/// Open a browser's extensions page, where "Load unpacked" is.
pub fn open_extensions_page(browser: &str) -> Result<(), String> {
    let (exe, page) = match browser {
        "edge" => ("msedge.exe", "edge://extensions/"),
        "brave" => ("brave.exe", "brave://extensions/"),
        _ => ("chrome.exe", "chrome://extensions/"),
    };
    let program = browser_path(exe).ok_or_else(|| format!("{browser} is not installed"))?;
    std::process::Command::new(program)
        .arg(page)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(windows)]
fn browser_path(exe: &str) -> Option<PathBuf> {
    let key = format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{exe}");
    for hive in [
        winreg::enums::HKEY_CURRENT_USER,
        winreg::enums::HKEY_LOCAL_MACHINE,
    ] {
        if let Ok(k) = winreg::RegKey::predef(hive).open_subkey(&key) {
            if let Ok(path) = k.get_value::<String, _>("") {
                let p = PathBuf::from(path.trim_matches('"'));
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn browser_path(exe: &str) -> Option<PathBuf> {
    let names: &[&str] = match exe {
        "msedge.exe" => &["microsoft-edge", "microsoft-edge-stable"],
        "brave.exe" => &["brave-browser", "brave"],
        _ => &[
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
        ],
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for n in names {
            let p = dir.join(n);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
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
                mark_seen(data_dir, &message);
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

/// How long the host waits for the window to take a hand-off, a cold start of
/// the window included.
const TAKE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Leave the hand-off in the inbox, start the window if none is running, and
/// answer only once the window has taken it. If it never does, take the
/// hand-off back and say so, so the browser keeps the download instead of
/// losing it.
fn deliver(data_dir: &Path, handoff: &Handoff) -> Result<(), String> {
    deliver_within(data_dir, handoff, TAKE_WAIT)
}

fn deliver_within(
    data_dir: &Path,
    handoff: &Handoff,
    wait: std::time::Duration,
) -> Result<(), String> {
    let inbox = data_dir.join("inbox");
    std::fs::create_dir_all(&inbox).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // Written aside and renamed, so the window never reads half a file.
    let part = inbox.join(format!("{stamp}.part"));
    let file = inbox.join(format!("{stamp}.json"));
    std::fs::write(
        &part,
        serde_json::to_vec(handoff).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&part, &file).map_err(|e| e.to_string())?;
    if !window_running(data_dir) {
        let started = std::env::current_exe()
            .map_err(|e| e.to_string())
            .and_then(|exe| start_window(&exe));
        if let Err(e) = started {
            let _ = std::fs::remove_file(&file);
            return Err(e);
        }
    }
    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline {
        if !file.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    match std::fs::remove_file(&file) {
        Ok(()) => Err("the window did not take the download".into()),
        // Taken at the last moment.
        Err(_) => Ok(()),
    }
}

/// The window holds this file locked for as long as it runs.
const WINDOW_LOCK: &str = "window.lock";

/// Taken by the window when it starts and held until it exits: how the host
/// knows there is a window reading the inbox.
pub fn hold_window_lock(data_dir: &Path) -> Option<std::fs::File> {
    let _ = std::fs::create_dir_all(data_dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data_dir.join(WINDOW_LOCK))
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

fn window_running(data_dir: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .open(data_dir.join(WINDOW_LOCK))
    else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// Hand-offs waiting in the inbox, oldest first. One nobody took for minutes
/// is dropped: the host has long since told the browser, which kept the
/// download itself.
pub fn waiting(data_dir: &Path) -> Vec<PathBuf> {
    const STALE: std::time::Duration = std::time::Duration::from_secs(120);
    let Ok(dir) = std::fs::read_dir(data_dir.join("inbox")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let path = entry.path();
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .unwrap_or_default();
        if age > STALE {
            let _ = std::fs::remove_file(&path);
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Start the window, out of the browser's reach. It finds the hand-off in the
/// inbox itself.
#[cfg(windows)]
fn start_window(exe: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    // The browser reads the host's output until it closes: the window must not
    // inherit it, or the browser's side never ends.
    let spawned = std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
        .spawn();
    if spawned.is_ok() {
        return Ok(());
    }
    // A browser may run the host in a job object (Firefox does) and end the
    // job when the host exits. Leaving the job is allowed under Firefox today
    // (checked 22 Nov 2026); where it is not, a window started inside the job
    // could die with it, so the shell starts it, as if the person had opened it.
    let windir =
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    std::process::Command::new(windir.join("explorer.exe"))
        .arg(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn start_window(exe: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    // Its own process group: a browser that ends the host's group does not
    // end the window.
    std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Read and delete a hand-off left in the inbox — and only there: `--handoff`
/// on a command line can never make the program read or delete another file.
/// It is claimed by a rename first, so of two readers only one gets it.
pub fn take(data_dir: &Path, file: &Path) -> Option<Handoff> {
    let inbox = data_dir.join("inbox").canonicalize().ok()?;
    let inbox_file = file.canonicalize().ok()?;
    if inbox_file.parent() != Some(inbox.as_path())
        || inbox_file.extension().is_none_or(|e| e != "json")
    {
        return None;
    }
    let claimed = inbox_file.with_extension("taken");
    std::fs::rename(&inbox_file, &claimed).ok()?;
    let text = std::fs::read(&claimed);
    let _ = std::fs::remove_file(&claimed);
    serde_json::from_slice(&text.ok()?).ok()
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
    // Tests drive the real window without touching the person's registry.
    if std::env::var_os("EON_DM_NO_REGISTRATION").is_some() {
        return Ok(());
    }
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
    if std::env::var_os("EON_DM_NO_REGISTRATION").is_some() {
        return;
    }
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eon-dm-native-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("inbox")).unwrap();
        dir
    }

    fn a_download() -> Handoff {
        check(&json!({ "handoff": 0, "kind": "download", "items": [{ "url": "https://example.org/a.zip" }] })).unwrap()
    }

    #[test]
    fn a_handoff_is_taken_once_and_only_from_the_inbox() {
        let data = scratch("take");
        let file = data.join("inbox").join("1.json");
        std::fs::write(&file, serde_json::to_vec(&a_download()).unwrap()).unwrap();
        assert_eq!(waiting(&data), vec![file.clone()]);
        assert!(take(&data, &file).is_some());
        assert!(take(&data, &file).is_none(), "a second reader gets nothing");
        assert!(waiting(&data).is_empty());
        let outside = data.join("x.json");
        std::fs::write(&outside, serde_json::to_vec(&a_download()).unwrap()).unwrap();
        assert!(take(&data, &outside).is_none());
        assert!(
            outside.exists(),
            "a file outside the inbox is never deleted"
        );
    }

    #[test]
    fn the_window_lock_says_whether_a_window_runs() {
        let data = scratch("lock");
        assert!(!window_running(&data));
        let held = hold_window_lock(&data).unwrap();
        assert!(window_running(&data));
        assert!(hold_window_lock(&data).is_none(), "one window");
        drop(held);
        assert!(!window_running(&data));
    }

    #[test]
    fn the_host_answers_once_the_window_has_taken_it() {
        let data = scratch("deliver");
        let _window = hold_window_lock(&data).unwrap();
        let reader = {
            let data = data.clone();
            std::thread::spawn(move || loop {
                if let Some(file) = waiting(&data).first() {
                    return take(&data, file);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            })
        };
        deliver_within(&data, &a_download(), std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(
            reader.join().unwrap().unwrap().items[0].url,
            "https://example.org/a.zip"
        );
    }

    #[test]
    fn a_handoff_nobody_takes_is_taken_back() {
        let data = scratch("untaken");
        let _window = hold_window_lock(&data).unwrap();
        let answer = deliver_within(&data, &a_download(), std::time::Duration::from_millis(300));
        assert!(answer.is_err(), "the browser must keep the download");
        assert!(waiting(&data).is_empty());
    }

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
