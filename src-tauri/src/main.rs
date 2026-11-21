// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! EON Download Manager -- the window.
//!
//! ```text
//! eon-download-manager                      open the window (or bring it forward)
//! eon-download-manager --add URL            add a download (the window asks first)
//! eon-download-manager --selftest [FILE]    strings, interface, and a real download
//! eon-download-manager --version
//! eon-download-manager chrome-extension://… started by a browser: the hand-off host
//! ```

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod checks;
mod native;
mod selftest;
mod settings;

use eon_dm_engine::{batch, AddRequest, Category, Engine, Event, ProbeView, Source, Start, View};
use native::Handoff;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use settings::AppSettings;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

const INDEX: &str = include_str!("../../src/index.html");
const SCRIPT: &str = include_str!("../../src/app.js");
pub const EN: &str = include_str!("../i18n/en.json");
pub const TR: &str = include_str!("../i18n/tr.json");

/// The commands below, by name: the interface may invoke only these
/// (`--selftest` checks). Keep in step with `generate_handler!`.
const COMMANDS: &[&str] = &[
    "strings",
    "boot",
    "snapshot",
    "probe",
    "add",
    "expand_batch",
    "add_batch",
    "pause",
    "resume",
    "start_now",
    "remove",
    "pause_all",
    "resume_all",
    "clear_finished",
    "refresh",
    "set_speed_limit",
    "set_connections",
    "verify",
    "open_file",
    "show_in_folder",
    "pick_folder",
    "save_settings",
    "browsers",
    "connect_browsers",
    "disconnect_browsers",
    "add_extension",
    "take_handoffs",
    "set_queue_running",
    "power",
];

/// The per-user data folder. Computed without Tauri, because the hand-off host
/// runs before (and without) any window. `EON_DM_DATA` overrides it, for tests.
fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EON_DM_DATA") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map_or_else(|| home.join("AppData").join("Local"), PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map_or_else(|| home.join(".local").join("share"), PathBuf::from)
    };
    base.join("EON").join("Download Manager")
}

struct App {
    engine: Engine,
    settings: Mutex<AppSettings>,
    data: PathBuf,
    downloads: PathBuf,
    pending: Mutex<Vec<Handoff>>,
    /// The language the interface loaded, for notifications and the tray.
    lang: Mutex<String>,
    /// Held while the window runs: the browser's host reads it to know the
    /// inbox is being watched.
    _window_lock: Option<std::fs::File>,
}

impl App {
    fn settings(&self) -> AppSettings {
        self.settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Where a file of this name goes by default.
    fn folder_for(&self, file_name: &str) -> PathBuf {
        let s = self.settings();
        let root = s.root(&self.downloads);
        let sub = Category::of(file_name).folder();
        if s.sort_by_category && !sub.is_empty() {
            root.join(sub)
        } else {
            root
        }
    }
}

type State<'a> = tauri::State<'a, Arc<App>>;

#[derive(Debug, Serialize)]
struct Failure {
    code: String,
    message: String,
}

impl From<eon_dm_engine::Error> for Failure {
    fn from(e: eon_dm_engine::Error) -> Self {
        Failure {
            code: e.code().to_owned(),
            message: e.to_string(),
        }
    }
}

fn failure(code: &str, message: impl Into<String>) -> Failure {
    Failure {
        code: code.to_owned(),
        message: message.into(),
    }
}

fn snapshot_of(engine: &Engine) -> Value {
    let views = engine.views();
    json!({ "views": views, "total": engine.total_speed() })
}

#[tauri::command(async)]
fn strings(state: State<'_>, language: String) -> Value {
    let lang = if language.starts_with("tr") {
        "tr"
    } else {
        "en"
    };
    lang.clone_into(&mut state.lang.lock().unwrap_or_else(PoisonError::into_inner));
    let text = if lang == "tr" { TR } else { EN };
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[tauri::command(async)]
fn boot(state: State<'_>) -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "settings": state.settings(),
        "downloads": state.downloads,
        "snapshot": snapshot_of(&state.engine),
        "browsers": browser_status(&state.data),
        "platform": std::env::consts::OS,
    })
}

#[tauri::command(async)]
fn snapshot(state: State<'_>) -> Value {
    snapshot_of(&state.engine)
}

#[derive(Debug, Deserialize)]
struct SourceArgs {
    url: String,
    #[serde(default)]
    referrer: Option<String>,
    #[serde(default)]
    cookies: Option<String>,
    #[serde(default)]
    user_agent: Option<String>,
}

impl From<SourceArgs> for Source {
    fn from(s: SourceArgs) -> Source {
        Source {
            url: s.url.trim().to_owned(),
            referrer: s.referrer,
            cookies: s.cookies,
            user_agent: s.user_agent,
        }
    }
}

#[derive(Debug, Serialize)]
struct Probed {
    #[serde(flatten)]
    probe: ProbeView,
    folder: PathBuf,
}

#[tauri::command(async)]
async fn probe(state: State<'_>, source: SourceArgs) -> Result<Probed, Failure> {
    let probe = state.engine.probe(source.into()).await?;
    let folder = state.folder_for(&probe.file_name);
    Ok(Probed { probe, folder })
}

#[derive(Debug, Deserialize)]
struct AddArgs {
    source: SourceArgs,
    #[serde(default)]
    probe: Option<ProbeView>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    start: Start,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    connections: Option<u32>,
    #[serde(default)]
    media: Option<eon_dm_engine::media::MediaSpec>,
    #[serde(default)]
    convert: Option<String>,
}

#[tauri::command(async)]
fn add(state: State<'_>, args: AddArgs) -> Result<String, Failure> {
    let guess = args
        .file_name
        .clone()
        .or_else(|| args.probe.as_ref().map(|p| p.file_name.clone()))
        .unwrap_or_default();
    let folder = args
        .folder
        .filter(|f| !f.trim().is_empty())
        .map_or_else(|| state.folder_for(&guess), PathBuf::from);
    Ok(state.engine.add(AddRequest {
        source: args.source.into(),
        probe: args.probe,
        file_name: args.file_name,
        folder,
        start: args.start,
        variant: args.variant,
        connections: args.connections,
        media: args.media,
        convert: args.convert,
    })?)
}

#[tauri::command(async)]
fn expand_batch(pattern: String) -> Result<Vec<String>, Failure> {
    batch::expand(pattern.trim()).map_err(|m| failure("invalid", m))
}

#[tauri::command(async)]
fn add_batch(state: State<'_>, urls: Vec<String>, start: Start) -> Result<usize, Failure> {
    let mut added = 0;
    for url in urls.into_iter().take(batch::LIMIT) {
        let name = url::Url::parse(&url)
            .ok()
            .and_then(|u| eon_dm_engine::name::from_url(&u))
            .unwrap_or_default();
        let folder = state.folder_for(&name);
        state.engine.add(AddRequest {
            source: Source {
                url,
                ..Source::default()
            },
            probe: None,
            file_name: None,
            folder,
            start,
            variant: None,
            connections: None,
            media: None,
            convert: None,
        })?;
        added += 1;
    }
    Ok(added)
}

#[tauri::command(async)]
fn pause(state: State<'_>, id: String) {
    state.engine.pause(&id);
}

#[tauri::command(async)]
fn resume(state: State<'_>, id: String) {
    state.engine.resume(&id);
}

#[tauri::command(async)]
fn start_now(state: State<'_>, id: String) {
    state.engine.start_now(&id);
}

#[tauri::command(async)]
fn remove(state: State<'_>, id: String, delete_file: bool) {
    state.engine.remove(&id, delete_file);
}

#[tauri::command(async)]
fn pause_all(state: State<'_>) {
    state.engine.pause_all();
}

#[tauri::command(async)]
fn resume_all(state: State<'_>) {
    state.engine.resume_all();
}

#[tauri::command(async)]
fn clear_finished(state: State<'_>) {
    state.engine.clear_finished();
}

#[tauri::command(async)]
async fn refresh(state: State<'_>, id: String, source: SourceArgs) -> Result<(), Failure> {
    Ok(state.engine.refresh(&id, source.into()).await?)
}

#[tauri::command(async)]
fn set_speed_limit(state: State<'_>, id: String, bytes_per_second: u64) {
    state.engine.set_speed_limit(&id, bytes_per_second);
}

/// This download's own connections (`None` follows the settings), applied to
/// the running transfer at once.
#[tauri::command(async)]
fn set_connections(state: State<'_>, id: String, connections: Option<u32>) {
    state.engine.set_connections(&id, connections);
}

#[tauri::command(async)]
fn verify(state: State<'_>, id: String, expected: String) -> Option<bool> {
    state.engine.verify(&id, &expected)
}

fn path_of(state: &App, id: &str) -> Result<PathBuf, Failure> {
    state
        .engine
        .view(id)
        .map(|v| PathBuf::from(v.path))
        .ok_or_else(|| failure("not_found", "no such download"))
}

#[tauri::command(async)]
fn open_file(state: State<'_>, id: String) -> Result<(), Failure> {
    let path = path_of(&state, &id)?;
    tauri_plugin_opener::open_path(&path, None::<&str>).map_err(|e| failure("open", e.to_string()))
}

#[tauri::command(async)]
fn show_in_folder(state: State<'_>, id: String) -> Result<(), Failure> {
    let path = path_of(&state, &id)?;
    let target = if path.exists() {
        path
    } else {
        eon_dm_engine::state::part_path(&path)
    };
    if target.exists() {
        tauri_plugin_opener::reveal_item_in_dir(&target).map_err(|e| failure("open", e.to_string()))
    } else {
        let folder = target.parent().map(Path::to_path_buf).unwrap_or_default();
        tauri_plugin_opener::open_path(&folder, None::<&str>)
            .map_err(|e| failure("open", e.to_string()))
    }
}

#[tauri::command(async)]
fn pick_folder(app: tauri::AppHandle, current: Option<String>) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let mut dialog = app.dialog().file();
    if let Some(c) = current.filter(|c| Path::new(c).is_dir()) {
        dialog = dialog.set_directory(c);
    }
    dialog
        .blocking_pick_folder()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command(async)]
fn save_settings(state: State<'_>, settings: AppSettings) -> AppSettings {
    let settings = settings.sane();
    state.engine.set_settings(settings.engine.clone());
    settings.save(&state.data);
    *state
        .settings
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = settings.clone();
    settings
}

fn browser_status(data: &Path) -> Value {
    let seen = native::seen(data);
    let list: Vec<Value> = native::status(data)
        .into_iter()
        .map(|(b, connected)| json!({ "browser": b, "connected": connected }))
        .collect();
    json!({ "browsers": list, "seen": seen })
}

#[tauri::command(async)]
fn browsers(state: State<'_>) -> Value {
    browser_status(&state.data)
}

fn set_integration(state: &App, on: bool) {
    let mut s = state.settings();
    s.browser_integration = on;
    s.save(&state.data);
    *state
        .settings
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = s;
}

#[tauri::command(async)]
fn connect_browsers(state: State<'_>) -> Result<Value, Failure> {
    native::connect(&state.data).map_err(|e| failure("browsers", e))?;
    set_integration(&state, true);
    Ok(browser_status(&state.data))
}

#[tauri::command(async)]
fn disconnect_browsers(state: State<'_>) -> Value {
    native::disconnect(&state.data);
    set_integration(&state, false);
    browser_status(&state.data)
}

/// Write the extension out, open the browser's extensions page and the folder
/// to load: the two things "Load unpacked" needs.
#[tauri::command(async)]
fn add_extension(state: State<'_>, browser: String) -> Result<String, Failure> {
    let dir = native::write_extension(&state.data).map_err(|e| failure("extension", e))?;
    let _ = tauri_plugin_opener::open_path(&dir, None::<&str>);
    native::open_extensions_page(&browser).map_err(|e| failure("browser", e))?;
    Ok(dir.to_string_lossy().into_owned())
}

/// Hand-offs that arrived before the interface was listening.
#[tauri::command(async)]
fn take_handoffs(state: State<'_>) -> Vec<Handoff> {
    std::mem::take(&mut *state.pending.lock().unwrap_or_else(PoisonError::into_inner))
}

/// The scheduler, which lives in the interface (it knows the person's clock),
/// starts and stops the queue.
#[tauri::command(async)]
fn set_queue_running(state: State<'_>, running: bool) {
    let mut s = state.settings();
    s.engine.queue_running = running;
    state.engine.set_settings(s.engine.clone());
    *state
        .settings
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = s;
}

/// After the queue: sleep or shut down, once the person's countdown ran out.
#[tauri::command(async)]
fn power(state: State<'_>, action: String) -> Result<(), Failure> {
    state.engine.flush();
    let run = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .spawn()
            .map(|_| ())
            .map_err(|e| failure("power", e.to_string()))
    };
    match (action.as_str(), std::env::consts::OS) {
        ("shutdown", "windows") => run("shutdown", &["/s", "/t", "0"]),
        ("sleep", "windows") => run("rundll32.exe", &["powrprof.dll,SetSuspendState", "0,1,0"]),
        ("shutdown", "macos") => run(
            "osascript",
            &["-e", "tell app \"System Events\" to shut down"],
        ),
        ("sleep", "macos") => run("pmset", &["sleepnow"]),
        ("shutdown", _) => run("systemctl", &["poweroff"]),
        ("sleep", _) => run("systemctl", &["suspend"]),
        _ => Err(failure("invalid", "sleep or shutdown")),
    }
}

/// What a hand-off asks for: start now, keep for later, or ask in the window.
fn deliver(app: &tauri::AppHandle, handoff: Handoff) {
    let state = app.state::<Arc<App>>();
    if handoff.kind == "refresh" {
        if let Some(item) = handoff.items.first() {
            if let Some(id) = item.replaces.clone() {
                let (app2, source) = (app.clone(), source_of(item));
                tauri::async_runtime::spawn(async move {
                    let state = app2.state::<Arc<App>>();
                    let result = state.engine.refresh(&id, source).await;
                    let _ = app2.emit("refreshed", json!({ "id": id, "ok": result.is_ok() }));
                });
            }
        }
        return;
    }
    if handoff.start != "ask" && handoff.kind != "media" {
        let start = if handoff.start == "now" {
            Start::Now
        } else {
            Start::Later
        };
        for item in &handoff.items {
            let name = item.file_name.clone().unwrap_or_default();
            let _ = state.engine.add(AddRequest {
                source: source_of(item),
                probe: None,
                file_name: item.file_name.clone(),
                folder: state.folder_for(&name),
                start,
                variant: None,
                connections: None,
                media: None,
                convert: None,
            });
        }
        return;
    }
    state
        .pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(handoff);
    show(app);
    let _ = app.emit("handoff", ());
}

fn source_of(item: &native::Item) -> Source {
    Source {
        url: item.url.clone(),
        referrer: item.referrer.clone(),
        cookies: item.cookies.clone(),
        user_agent: item.user_agent.clone(),
    }
}

fn show(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// `--handoff FILE` (from the host) and `--add URL` (from a person or script).
fn handoffs_in(args: &[String], data: &Path) -> Vec<Handoff> {
    let mut out = Vec::new();
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--handoff" => {
                if let Some(h) = it.next().and_then(|f| native::take(data, Path::new(f))) {
                    out.push(h);
                }
            }
            "--add" => {
                if let Some(url) = it
                    .next()
                    .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                {
                    out.push(Handoff {
                        kind: "links".into(),
                        start: "ask".into(),
                        items: vec![native::Item {
                            url: url.clone(),
                            final_url: None,
                            referrer: None,
                            file_name: None,
                            mime: None,
                            size: None,
                            cookies: None,
                            user_agent: None,
                            page_title: None,
                            media: None,
                            replaces: None,
                        }],
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn notify(app: &tauri::AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Text in the language the interface chose, from the same catalogue.
fn text(app: &tauri::AppHandle, key: &str) -> String {
    let state = app.state::<Arc<App>>();
    let lang = state
        .lang
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let catalogue: Value =
        serde_json::from_str(if lang == "tr" { TR } else { EN }).unwrap_or_default();
    catalogue[key].as_str().unwrap_or(key).to_owned()
}

fn on_engine_event(app: &tauri::AppHandle, dirty: &AtomicBool, event: &Event) {
    match event {
        Event::Changed => dirty.store(true, Ordering::Relaxed),
        Event::Done { name, .. } => {
            dirty.store(true, Ordering::Relaxed);
            if app.state::<Arc<App>>().settings().notify {
                notify(app, &text(app, "notify.done"), name);
            }
        }
        Event::Failed { name, detail, .. } => {
            dirty.store(true, Ordering::Relaxed);
            if app.state::<Arc<App>>().settings().notify {
                notify(
                    app,
                    &text(app, "notify.failed"),
                    &format!("{name}: {detail}"),
                );
            }
        }
        Event::QueueFinished => {
            let _ = app.emit("queue-finished", ());
        }
    }
}

fn speed_text(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    if mb >= 1.0 {
        format!("{mb:.1} MB/s")
    } else {
        format!("{:.0} KB/s", bytes as f64 / 1024.0)
    }
}

fn main_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    #[allow(unused_mut)]
    let mut builder =
        tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
            .title("EON Download Manager")
            .inner_size(1280.0, 800.0)
            .min_inner_size(880.0, 560.0)
            .decorations(false)
            .shadow(true)
            .visible(true);
    // Debug builds only: a DevTools port for tools/ui-check.mjs.
    #[cfg(debug_assertions)]
    if let Ok(extra) = std::env::var("EON_WEBVIEW_ARGS") {
        builder = builder.additional_browser_args(&format!(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection {extra}"
        ));
    }
    let window = builder.build()?;
    let handle = app.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            let state = handle.state::<Arc<App>>();
            let mut s = state.settings();
            if s.close_to_tray {
                api.prevent_close();
                if let Some(w) = handle.get_webview_window("main") {
                    let _ = w.hide();
                }
                if !s.tray_hint_shown {
                    s.tray_hint_shown = true;
                    s.save(&state.data);
                    *state
                        .settings
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = s;
                    notify(
                        &handle,
                        &text(&handle, "tray.hint.title"),
                        &text(&handle, "tray.hint.body"),
                    );
                }
            } else {
                state.engine.shutdown();
            }
        }
    });
    Ok(())
}

fn tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", text(app, "tray.show"), true, None::<&str>)?;
    let pause_item = MenuItem::with_id(app, "pause", text(app, "tray.pause"), true, None::<&str>)?;
    let resume_item =
        MenuItem::with_id(app, "resume", text(app, "tray.resume"), true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", text(app, "tray.quit"), true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show_item,
            &PredefinedMenuItem::separator(app)?,
            &pause_item,
            &resume_item,
            &PredefinedMenuItem::separator(app)?,
            &quit_item,
        ],
    )?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("EON Download Manager")
        .menu(&menu)
        .show_menu_on_left_click(false);
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder
        .on_menu_event(|app, event| {
            let state = app.state::<Arc<App>>();
            match event.id.as_ref() {
                "show" => show(app),
                "pause" => state.engine.pause_all(),
                "resume" => state.engine.resume_all(),
                "quit" => {
                    state.engine.shutdown();
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Live numbers to the window four times a second while anything moves, and
/// the tray's tooltip once a second.
fn ticker(app: tauri::AppHandle, dirty: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut n = 0u64;
        loop {
            std::thread::sleep(Duration::from_millis(250));
            n += 1;
            let state = app.state::<Arc<App>>();
            let views: Vec<View> = state.engine.views();
            let moving = views
                .iter()
                .any(|v| v.status == eon_dm_engine::Status::Downloading);
            if moving || dirty.swap(false, Ordering::Relaxed) {
                let total: u64 = views.iter().map(|v| v.speed).sum();
                let _ = app.emit("snapshot", json!({ "views": views, "total": total }));
                if n.is_multiple_of(4) {
                    if let Some(tray) = app.tray_by_id("main") {
                        let tip = if moving {
                            format!("EON Download Manager\n↓ {}", speed_text(total))
                        } else {
                            "EON Download Manager".into()
                        };
                        let _ = tray.set_tooltip(Some(tip));
                    }
                }
            }
        }
    });
}

/// The browser's hand-offs: the host leaves each in the inbox and waits until
/// it is taken. Read here rather than handed over by a second copy of the
/// program, which lost hand-offs under Firefox (see native.rs).
/// `early`: the ones named on this start's command line.
fn inbox_watch(app: tauri::AppHandle, early: Vec<Handoff>) {
    std::thread::spawn(move || {
        for handoff in early {
            deliver(&app, handoff);
        }
        let data = app.state::<Arc<App>>().data.clone();
        loop {
            for file in native::waiting(&data) {
                if let Some(handoff) = native::take(&data, &file) {
                    deliver(&app, handoff);
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
}

/// Download links copied to the clipboard are offered (setting, on by default).
/// Read in Rust: the interface never sees the clipboard.
fn clipboard_watch(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return;
        };
        let mut last = clipboard.get_text().unwrap_or_default();
        loop {
            std::thread::sleep(Duration::from_millis(900));
            if !app.state::<Arc<App>>().settings().watch_clipboard {
                continue;
            }
            let Ok(text) = clipboard.get_text() else {
                continue;
            };
            if text == last {
                continue;
            }
            last.clone_from(&text);
            let t = text.trim();
            if t.len() > 4096
                || t.contains(char::is_whitespace)
                || !(t.starts_with("http://") || t.starts_with("https://"))
            {
                continue;
            }
            let Ok(url) = url::Url::parse(t) else {
                continue;
            };
            let name = eon_dm_engine::name::from_url(&url).unwrap_or_default();
            let wanted = Category::of(&name) != Category::Other
                || name.to_ascii_lowercase().ends_with(".m3u8");
            if wanted {
                let _ = app.emit("clipboard", json!({ "url": t, "name": name }));
            }
        }
    });
}

fn window(args: Vec<String>) {
    let data = data_dir();
    let app = tauri::Builder::default()
        // First: a second start hands over its arguments and exits.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            let data = app.state::<Arc<App>>().data.clone();
            let handoffs = handoffs_in(&args, &data);
            if handoffs.is_empty() {
                show(app);
            }
            for h in handoffs {
                deliver(app, h);
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            // Only the first instance gets here: a second start has already
            // handed its arguments over and exited, leaving the inbox alone.
            let early = handoffs_in(&args, &data);
            let handle = app.handle().clone();
            let settings = AppSettings::load(&data).sane();
            let settings_lang = settings.language.clone();
            let downloads = app
                .path()
                .download_dir()
                .unwrap_or_else(|_| data.join("Downloads"));
            let dirty = Arc::new(AtomicBool::new(true));
            let (h, d) = (handle.clone(), Arc::clone(&dirty));
            let engine = Engine::new(&data, settings.engine.clone(), move |event| {
                on_engine_event(&h, &d, &event);
            })
            .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;
            app.manage(Arc::new(App {
                _window_lock: native::hold_window_lock(&data),
                engine,
                settings: Mutex::new(settings),
                data: data.clone(),
                downloads,
                pending: Mutex::new(Vec::new()),
                lang: Mutex::new(if settings_lang.starts_with("tr") {
                    "tr".into()
                } else {
                    "en".into()
                }),
            }));
            // Browsers learn where this program is at every start (per user, no
            // administrator rights), unless the person disconnected them.
            if app.state::<Arc<App>>().settings().browser_integration {
                let _ = native::connect(&data);
            }
            main_window(&handle)?;
            tray(&handle)?;
            ticker(handle.clone(), dirty);
            clipboard_watch(handle.clone());
            inbox_watch(handle.clone(), early);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            strings,
            boot,
            snapshot,
            probe,
            add,
            expand_batch,
            add_batch,
            pause,
            resume,
            start_now,
            remove,
            pause_all,
            resume_all,
            clear_finished,
            refresh,
            set_speed_limit,
            set_connections,
            verify,
            open_file,
            show_in_folder,
            pick_folder,
            save_settings,
            browsers,
            connect_browsers,
            disconnect_browsers,
            add_extension,
            take_handoffs,
            set_queue_running,
            power,
        ])
        .build(tauri::generate_context!());
    match app {
        Ok(app) => app.run(|app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                if let Some(state) = app.try_state::<Arc<App>>() {
                    state.engine.flush();
                }
            }
        }),
        Err(e) => {
            eprintln!("EON Download Manager could not start: {e}");
            std::process::exit(1);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if native::is_host_launch(&args) {
        std::process::exit(native::run_host(&data_dir()));
    }
    match args.get(1).map(String::as_str) {
        Some("--version" | "-V") => println!("EON Download Manager {}", env!("CARGO_PKG_VERSION")),
        Some("--selftest") => {
            let report = selftest::run(INDEX, SCRIPT, COMMANDS);
            match args.get(2) {
                Some(path) => {
                    if let Err(e) = std::fs::write(path, &report) {
                        eprintln!("could not write {path}: {e}");
                        std::process::exit(2);
                    }
                }
                None => print!("{report}"),
            }
            if !report.ends_with("ok\n") {
                std::process::exit(1);
            }
        }
        _ => window(args),
    }
}
