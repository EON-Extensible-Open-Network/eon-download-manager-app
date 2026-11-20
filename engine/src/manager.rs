// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The list of downloads and the queue.
//!
//! The engine owns its own runtime, so a front end can call it from anywhere.
//! The list is saved to `downloads.json` in the data folder; downloads that
//! were running when the program stopped — closed, crashed, power cut — are
//! queued again at the next start and continue from their resume state.

use crate::category::Category;
use crate::hls::{self, Inspection};
use crate::http::{self, Source};
use crate::limiter::Limiter;
use crate::state::{self, Validators};
use crate::transfer::{self, Live, Outcome};
use crate::{name, plan, Error};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Paused,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Hls,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    source: Source,
    /// What is fetched: the final address after redirects, or the chosen HLS
    /// media playlist.
    url: String,
    path: PathBuf,
    category: Category,
    kind: Kind,
    status: Status,
    length: Option<u64>,
    done: u64,
    resumable: bool,
    #[serde(default)]
    validators: Validators,
    /// The server has been asked about this file at least once.
    probed: bool,
    sha256: Option<String>,
    added: u64,
    finished: Option<u64>,
    error: Option<(String, String)>,
    connections: Option<u32>,
    #[serde(default)]
    speed_limit: u64,
}

/// What the engine is told by the front end's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    /// Downloads running at once from the queue.
    pub max_active: u32,
    /// Connections per download.
    pub connections: u32,
    /// Bytes per second for everything together; 0 is unlimited.
    pub speed_limit: u64,
    /// Resume even if the server says the file changed.
    pub ignore_changes: bool,
    /// When false, queued downloads wait (the scheduler's stop time).
    pub queue_running: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_active: 3,
            connections: 8,
            speed_limit: 0,
            ignore_changes: false,
            queue_running: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Start {
    /// Now, even if the queue is full.
    Now,
    /// When the queue has room.
    Queue,
    /// Not until the person says so.
    Later,
}

/// What the add sheet shows before anything is saved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeView {
    pub url: String,
    pub final_url: String,
    pub file_name: String,
    pub length: Option<u64>,
    pub resumable: bool,
    pub mime: Option<String>,
    pub category: Category,
    pub hls: Option<Inspection>,
    #[serde(skip)]
    validators: Validators,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddRequest {
    pub source: Source,
    /// From an earlier [`Engine::probe`]; probed at start when absent.
    pub probe: Option<ProbeView>,
    pub file_name: Option<String>,
    pub folder: PathBuf,
    pub start: Start,
    /// HLS: the media playlist of the chosen version.
    pub variant: Option<String>,
    pub connections: Option<u32>,
}

/// One connection's share of the file, for drawing.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Lane {
    pub start: u64,
    pub pos: u64,
    pub end: u64,
    pub active: bool,
}

/// A download as the interface sees it.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub id: String,
    pub url: String,
    pub name: String,
    pub path: String,
    pub folder: String,
    pub category: Category,
    pub kind: Kind,
    pub status: Status,
    pub length: Option<u64>,
    pub done: u64,
    /// Bytes per second, smoothed.
    pub speed: u64,
    /// Seconds left at the current speed.
    pub eta: Option<u64>,
    pub connections: u32,
    pub lanes: Vec<Lane>,
    /// One value a second, the last 60 seconds, bytes per second.
    pub history: Vec<u64>,
    pub resumable: bool,
    pub sha256: Option<String>,
    pub error: Option<ViewError>,
    pub added: u64,
    pub finished: Option<u64>,
    /// HLS: pieces done and in total.
    pub pieces: Option<(u64, u64)>,
    pub speed_limit: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewError {
    pub code: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Something in the list changed (status, added, removed).
    Changed,
    Done {
        id: String,
        name: String,
        path: String,
    },
    Failed {
        id: String,
        name: String,
        code: String,
        detail: String,
    },
    /// The queue ran out of work after running something.
    QueueFinished,
}

struct Item {
    record: Record,
    live: Arc<Live>,
    stop: Option<watch::Sender<bool>>,
    own: Arc<Limiter>,
    history: VecDeque<u64>,
    last_received: u64,
    speed: f64,
}

type Listener = Box<dyn Fn(Event) + Send + Sync>;

struct Inner {
    rt: tokio::runtime::Handle,
    client: reqwest::Client,
    global: Arc<Limiter>,
    list_file: PathBuf,
    settings: Mutex<Settings>,
    items: Mutex<Vec<Item>>,
    dirty: AtomicBool,
    queue_was_busy: AtomicBool,
    listener: Listener,
}

pub struct Engine {
    inner: Arc<Inner>,
    /// Owned here, not by `Inner`: tasks hold `Inner`, and a runtime must never
    /// be dropped by one of its own tasks.
    runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_id() -> String {
    format!(
        "{:x}{:03x}",
        now_ms(),
        COUNTER.fetch_add(1, Ordering::Relaxed) % 4096
    )
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Inner {
    fn items(&self) -> MutexGuard<'_, Vec<Item>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn settings(&self) -> Settings {
        self.settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn changed(&self) {
        self.dirty.store(true, Ordering::Relaxed);
        (self.listener)(Event::Changed);
    }

    fn save(&self) {
        let records: Vec<Record> = self.items().iter().map(|i| i.record.clone()).collect();
        let tmp = self.list_file.with_extension("json.tmp");
        if let Ok(text) = serde_json::to_vec_pretty(&records) {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &self.list_file);
            }
        }
    }
}

impl Engine {
    /// Open the engine with its list in `data_dir`. `listener` hears about
    /// changes; it is called from the engine's threads.
    pub fn new(
        data_dir: &Path,
        settings: Settings,
        listener: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<Engine, Error> {
        std::fs::create_dir_all(data_dir).map_err(|e| Error::Io(e.to_string()))?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("eon-dm-engine")
            .enable_all()
            .build()
            .map_err(|e| Error::Io(e.to_string()))?;
        let list_file = data_dir.join("downloads.json");
        let records: Vec<Record> = std::fs::read(&list_file)
            .ok()
            .and_then(|t| serde_json::from_slice(&t).ok())
            .unwrap_or_default();
        let items = records
            .into_iter()
            .map(|mut record| {
                // Running when the program stopped: continue.
                if record.status == Status::Downloading {
                    record.status = Status::Queued;
                }
                Item {
                    own: Arc::new(Limiter::new(record.speed_limit)),
                    record,
                    live: Arc::new(Live::default()),
                    stop: None,
                    history: VecDeque::new(),
                    last_received: 0,
                    speed: 0.0,
                }
            })
            .collect();
        let inner = Arc::new(Inner {
            client: {
                let _guard = rt.enter();
                http::client()?
            },
            rt: rt.handle().clone(),
            global: Arc::new(Limiter::new(settings.speed_limit)),
            list_file,
            settings: Mutex::new(settings),
            items: Mutex::new(items),
            dirty: AtomicBool::new(false),
            queue_was_busy: AtomicBool::new(false),
            listener: Box::new(listener),
        });
        let weak = Arc::downgrade(&inner);
        inner.rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            let mut n = 0u64;
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else { break };
                n += 1;
                sample(&inner, n.is_multiple_of(2));
                schedule(&inner);
                if n.is_multiple_of(4) && inner.dirty.swap(false, Ordering::Relaxed) {
                    let i = Arc::clone(&inner);
                    let _ = tokio::task::spawn_blocking(move || i.save()).await;
                }
            }
        });
        Ok(Engine {
            inner,
            runtime: Some(rt),
        })
    }

    pub fn settings(&self) -> Settings {
        self.inner.settings()
    }

    pub fn set_settings(&self, settings: Settings) {
        self.inner.global.set(settings.speed_limit);
        *self
            .inner
            .settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = settings;
        schedule(&self.inner);
    }

    /// Learn what an address is before adding it: name, size, resumable, and
    /// for a video playlist its versions.
    pub async fn probe(&self, source: Source) -> Result<ProbeView, Error> {
        let client = self.inner.client.clone();
        self.inner
            .rt
            .spawn(async move {
                let p = http::probe(&client, &source).await?;
                let hls = if p.hls {
                    Some(hls::inspect(&client, &source, &p.final_url).await?)
                } else {
                    None
                };
                let mut file_name = p.file_name.clone();
                if hls.as_ref().is_some_and(|h| h.mp4) {
                    file_name = format!("{}.mp4", file_name.trim_end_matches(".ts"));
                }
                Ok(ProbeView {
                    url: source.url.clone(),
                    final_url: p.final_url,
                    category: Category::of(&file_name),
                    file_name,
                    length: if hls.is_some() { None } else { p.length },
                    resumable: hls.is_some() || p.resumable,
                    mime: p.mime,
                    hls,
                    validators: p.validators,
                })
            })
            .await
            .map_err(|e| Error::Io(e.to_string()))?
    }

    /// Put a download in the list. Its name is reserved at once, so two adds
    /// of the same file never pick the same name.
    pub fn add(&self, req: AddRequest) -> Result<String, Error> {
        let url = url::Url::parse(&req.source.url)
            .map_err(|_| Error::Invalid("not an address".into()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::Invalid("only http and https addresses".into()));
        }
        let probe = req.probe.as_ref();
        let wanted = req
            .file_name
            .as_deref()
            .or(probe.map(|p| p.file_name.as_str()))
            .map(name::clean)
            .filter(|n| !n.is_empty())
            .or_else(|| name::from_url(&url))
            .unwrap_or_else(|| "download".into());
        std::fs::create_dir_all(&req.folder).map_err(|e| Error::Io(e.to_string()))?;
        let path = name::unique(&req.folder, &wanted);
        let _ = std::fs::File::create(state::part_path(&path));
        let kind = if probe.is_some_and(|p| p.hls.is_some()) {
            Kind::Hls
        } else {
            Kind::File
        };
        let fetch = match kind {
            Kind::Hls => req
                .variant
                .clone()
                .or_else(|| {
                    probe
                        .and_then(|p| p.hls.as_ref())
                        .map(|h| h.media_url.clone())
                })
                .unwrap_or_else(|| req.source.url.clone()),
            Kind::File => probe.map_or_else(|| req.source.url.clone(), |p| p.final_url.clone()),
        };
        let id = new_id();
        let record = Record {
            id: id.clone(),
            source: req.source,
            url: fetch,
            category: Category::of(&wanted),
            path,
            kind,
            status: match req.start {
                Start::Now => Status::Downloading,
                Start::Queue => Status::Queued,
                Start::Later => Status::Paused,
            },
            length: probe.and_then(|p| p.length),
            done: 0,
            resumable: probe.is_some_and(|p| p.resumable),
            validators: probe.map(|p| p.validators.clone()).unwrap_or_default(),
            probed: probe.is_some(),
            sha256: None,
            added: now_ms(),
            finished: None,
            error: None,
            connections: req.connections,
            speed_limit: 0,
        };
        let start_now = record.status == Status::Downloading;
        self.inner.items().push(Item {
            record,
            live: Arc::new(Live::default()),
            stop: None,
            own: Arc::new(Limiter::new(0)),
            history: VecDeque::new(),
            last_received: 0,
            speed: 0.0,
        });
        if start_now {
            start(&self.inner, &id);
        }
        self.inner.changed();
        schedule(&self.inner);
        Ok(id)
    }

    pub fn pause(&self, id: &str) {
        let mut items = self.inner.items();
        if let Some(item) = items.iter_mut().find(|i| i.record.id == id) {
            if matches!(item.record.status, Status::Downloading | Status::Queued) {
                item.record.status = Status::Paused;
                if let Some(stop) = &item.stop {
                    let _ = stop.send(true);
                }
            }
        }
        drop(items);
        self.inner.changed();
    }

    /// Back into the queue; starts as soon as there is room.
    pub fn resume(&self, id: &str) {
        let mut items = self.inner.items();
        if let Some(item) = items.iter_mut().find(|i| i.record.id == id) {
            if matches!(item.record.status, Status::Paused | Status::Failed) {
                item.record.status = Status::Queued;
                item.record.error = None;
            }
        }
        drop(items);
        self.inner.changed();
        schedule(&self.inner);
    }

    /// Start at once, whatever the queue says.
    pub fn start_now(&self, id: &str) {
        let startable = self.inner.items().iter().any(|i| {
            i.record.id == id
                && matches!(
                    i.record.status,
                    Status::Paused | Status::Failed | Status::Queued
                )
        });
        if startable {
            start(&self.inner, id);
            self.inner.changed();
        }
    }

    pub fn pause_all(&self) {
        let ids: Vec<String> = self
            .inner
            .items()
            .iter()
            .map(|i| i.record.id.clone())
            .collect();
        for id in ids {
            self.pause(&id);
        }
    }

    pub fn resume_all(&self) {
        let ids: Vec<String> = self
            .inner
            .items()
            .iter()
            .map(|i| i.record.id.clone())
            .collect();
        for id in ids {
            self.resume(&id);
        }
    }

    /// Take a download out of the list. Its partial data always goes; the
    /// finished file only when `delete_file`.
    pub fn remove(&self, id: &str, delete_file: bool) {
        let item = {
            let mut items = self.inner.items();
            let Some(index) = items.iter().position(|i| i.record.id == id) else {
                return;
            };
            items.remove(index)
        };
        if let Some(stop) = &item.stop {
            let _ = stop.send(true);
        }
        let path = item.record.path.clone();
        let done = item.record.status == Status::Done;
        self.inner.rt.spawn(async move {
            // Let a running transfer notice and close its file first.
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = tokio::fs::remove_file(state::part_path(&path)).await;
            let _ = tokio::fs::remove_file(state::state_path(&path)).await;
            let _ = tokio::fs::remove_dir_all(state::pieces_dir(&path)).await;
            if delete_file && done {
                let _ = tokio::fs::remove_file(&path).await;
            }
        });
        self.inner.changed();
    }

    /// Remove every finished download from the list (files stay).
    pub fn clear_finished(&self) {
        self.inner
            .items()
            .retain(|i| i.record.status != Status::Done);
        self.inner.changed();
    }

    /// Give a download a new address — an expired link, a mirror. Every byte
    /// already downloaded is kept, if the new address serves a file of the
    /// same size.
    pub async fn refresh(&self, id: &str, source: Source) -> Result<(), Error> {
        let (length, kind) = {
            let items = self.inner.items();
            let item = items
                .iter()
                .find(|i| i.record.id == id)
                .ok_or(Error::NotFound)?;
            (item.record.length, item.record.kind)
        };
        if kind == Kind::Hls {
            return Err(Error::Unsupported(
                "a video stream cannot take a new address yet".into(),
            ));
        }
        let probe = self.probe(source.clone()).await?;
        if length.is_some() && probe.length != length {
            return Err(Error::Invalid(
                "that address leads to a different file (the size differs)".into(),
            ));
        }
        let was_running = {
            let mut items = self.inner.items();
            let item = items
                .iter_mut()
                .find(|i| i.record.id == id)
                .ok_or(Error::NotFound)?;
            item.record.source = source;
            item.record.url.clone_from(&probe.final_url);
            item.record.validators = probe.validators.clone();
            item.record.resumable = probe.resumable;
            item.record.length = probe.length;
            item.record.probed = true;
            item.record.error = None;
            let running = item.stop.is_some();
            if let Some(stop) = &item.stop {
                let _ = stop.send(true);
            }
            item.record.status = Status::Queued;
            running
        };
        if was_running {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        self.inner.changed();
        schedule(&self.inner);
        Ok(())
    }

    pub fn set_speed_limit(&self, id: &str, bytes_per_second: u64) {
        let mut items = self.inner.items();
        if let Some(item) = items.iter_mut().find(|i| i.record.id == id) {
            item.record.speed_limit = bytes_per_second;
            item.own.set(bytes_per_second);
        }
        drop(items);
        self.inner.changed();
    }

    /// Does the finished file match `expected` (hex SHA-256, any case)?
    pub fn verify(&self, id: &str, expected: &str) -> Option<bool> {
        let items = self.inner.items();
        let sum = items
            .iter()
            .find(|i| i.record.id == id)?
            .record
            .sha256
            .clone()?;
        Some(sum.eq_ignore_ascii_case(expected.trim()))
    }

    pub fn views(&self) -> Vec<View> {
        self.inner.items().iter().map(view).collect()
    }

    pub fn view(&self, id: &str) -> Option<View> {
        self.inner
            .items()
            .iter()
            .find(|i| i.record.id == id)
            .map(view)
    }

    /// Bytes per second, all downloads together.
    pub fn total_speed(&self) -> u64 {
        self.inner.items().iter().map(|i| i.speed as u64).sum()
    }

    /// Write the list now (on exit).
    pub fn flush(&self) {
        self.inner.save();
    }

    /// Stop every transfer and write the list; used on exit so that every
    /// resume state is current.
    pub fn shutdown(&self) {
        for item in self.inner.items().iter() {
            if let Some(stop) = &item.stop {
                let _ = stop.send(true);
            }
        }
        std::thread::sleep(Duration::from_millis(600));
        self.inner.save();
    }
}

fn view(item: &Item) -> View {
    let r = &item.record;
    let segments = item.live.segments().clone();
    let running = item.stop.is_some();
    let (lanes, done, pieces) = match r.kind {
        Kind::File => {
            let done = if segments.is_empty() {
                r.done
            } else {
                plan::done_bytes(&segments)
            };
            let lanes = segments
                .iter()
                .take(512)
                .map(|s| Lane {
                    start: s.start,
                    pos: s.pos,
                    end: s.end,
                    active: running && s.owner.is_some(),
                })
                .collect();
            (lanes, done, None)
        }
        Kind::Hls => {
            let total = segments.len() as u64;
            let finished = segments.iter().filter(|s| s.pos >= s.end).count() as u64;
            // Pieces drawn as up to 64 cells, each filled by its share done.
            let cells = total.clamp(1, 64);
            let lanes = (0..cells)
                .map(|c| {
                    let (a, b) = (c * total / cells, (c + 1) * total / cells);
                    let got = segments[a as usize..b as usize]
                        .iter()
                        .filter(|s| s.pos >= s.end)
                        .count() as u64;
                    Lane {
                        start: a,
                        pos: a + got,
                        end: b,
                        active: running && got < b - a,
                    }
                })
                .filter(|l| l.end > l.start)
                .collect();
            let done = item.live.received.load(Ordering::Relaxed).max(r.done);
            (lanes, done, (total > 0).then_some((finished, total)))
        }
    };
    let speed = if running { item.speed as u64 } else { 0 };
    let eta = match (r.length, r.kind, pieces) {
        (Some(len), Kind::File, _) if speed > 0 => Some(len.saturating_sub(done) / speed),
        (_, Kind::Hls, Some((f, t))) if speed > 0 && f > 0 => {
            let per_piece = done / f;
            Some((t - f) * per_piece / speed)
        }
        _ => None,
    };
    View {
        id: r.id.clone(),
        url: r.source.url.clone(),
        name: file_name_of(&r.path),
        path: r.path.to_string_lossy().into_owned(),
        folder: r
            .path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        category: r.category,
        kind: r.kind,
        status: r.status,
        length: r.length,
        done: if r.status == Status::Done {
            r.length.unwrap_or(done)
        } else {
            done
        },
        speed,
        eta,
        connections: if running {
            item.live.connections.load(Ordering::Relaxed)
        } else {
            0
        },
        lanes,
        history: item.history.iter().copied().collect(),
        resumable: r.resumable,
        sha256: r.sha256.clone(),
        error: r.error.as_ref().map(|(code, detail)| ViewError {
            code: code.clone(),
            detail: detail.clone(),
        }),
        added: r.added,
        finished: r.finished,
        pieces,
        speed_limit: r.speed_limit,
    }
}

/// Speeds, smoothed; a history point every second.
fn sample(inner: &Inner, record_history: bool) {
    let mut items = inner.items();
    for item in items.iter_mut() {
        let received = item.live.received.load(Ordering::Relaxed);
        let delta = received.saturating_sub(item.last_received);
        item.last_received = received;
        let instant = delta as f64 * 2.0;
        item.speed = if item.stop.is_some() {
            item.speed * 0.6 + instant * 0.4
        } else {
            0.0
        };
        if record_history && item.stop.is_some() {
            item.history.push_back(item.speed as u64);
            while item.history.len() > 60 {
                item.history.pop_front();
            }
        }
    }
}

/// Start queued downloads while there is room; say when the queue empties.
fn schedule(inner: &Arc<Inner>) {
    let settings = inner.settings();
    let mut to_start = Vec::new();
    let (running, queued) = {
        let items = inner.items();
        let running = items.iter().filter(|i| i.stop.is_some()).count();
        let mut room = (settings.max_active as usize).saturating_sub(running);
        if settings.queue_running {
            for item in items
                .iter()
                .filter(|i| i.record.status == Status::Queued && i.stop.is_none())
            {
                if room == 0 {
                    break;
                }
                to_start.push(item.record.id.clone());
                room -= 1;
            }
        }
        (
            running,
            items
                .iter()
                .filter(|i| i.record.status == Status::Queued)
                .count(),
        )
    };
    for id in &to_start {
        start(inner, id);
    }
    if running + to_start.len() > 0 {
        inner.queue_was_busy.store(true, Ordering::Relaxed);
    } else if queued == 0 && inner.queue_was_busy.swap(false, Ordering::Relaxed) {
        (inner.listener)(Event::QueueFinished);
    }
    if !to_start.is_empty() {
        inner.changed();
    }
}

fn start(inner: &Arc<Inner>, id: &str) {
    let (tx, rx) = watch::channel(false);
    {
        let mut items = inner.items();
        let Some(item) = items.iter_mut().find(|i| i.record.id == id) else {
            return;
        };
        if item.stop.is_some() {
            return;
        }
        item.record.status = Status::Downloading;
        item.record.error = None;
        item.stop = Some(tx);
        item.live = Arc::new(Live::default());
        item.last_received = 0;
    }
    let (inner2, id) = (Arc::clone(inner), id.to_owned());
    inner.rt.spawn(async move { drive(inner2, id, rx).await });
}

#[allow(clippy::too_many_lines)] // one download from start to end, read best whole
async fn drive(inner: Arc<Inner>, id: String, stop: watch::Receiver<bool>) {
    let mut restarted = false;
    let result = loop {
        let Some((mut record, live, own)) = ({
            let items = inner.items();
            items
                .iter()
                .find(|i| i.record.id == id)
                .map(|i| (i.record.clone(), Arc::clone(&i.live), Arc::clone(&i.own)))
        }) else {
            return;
        };
        let settings = inner.settings();
        if record.kind == Kind::File && !record.probed {
            match http::probe(&inner.client, &record.source).await {
                Ok(p) => {
                    record.url = p.final_url;
                    record.length = p.length;
                    record.resumable = p.resumable;
                    record.validators = p.validators;
                    record.probed = true;
                    let mut items = inner.items();
                    if let Some(i) = items.iter_mut().find(|i| i.record.id == id) {
                        i.record.url.clone_from(&record.url);
                        i.record.length = record.length;
                        i.record.resumable = record.resumable;
                        i.record.validators = record.validators.clone();
                        i.record.probed = true;
                    }
                    drop(items);
                    inner.changed();
                }
                Err(e) => break Err(e),
            }
        }
        let connections = record.connections.unwrap_or(settings.connections);
        let outcome = match record.kind {
            Kind::File => {
                transfer::run(
                    transfer::Job {
                        client: inner.client.clone(),
                        source: record.source.clone(),
                        url: record.url.clone(),
                        target: record.path.clone(),
                        length: record.length,
                        resumable: record.resumable,
                        validators: record.validators.clone(),
                        connections,
                        ignore_changes: settings.ignore_changes,
                        global: Arc::clone(&inner.global),
                        own,
                    },
                    live,
                    stop.clone(),
                )
                .await
            }
            Kind::Hls => {
                hls::run(
                    hls::HlsJob {
                        client: inner.client.clone(),
                        source: record.source.clone(),
                        playlist: record.url.clone(),
                        target: record.path.clone(),
                        connections,
                        global: Arc::clone(&inner.global),
                        own,
                    },
                    live,
                    stop.clone(),
                )
                .await
            }
        };
        match outcome {
            Err(Error::Changed) if !restarted => {
                // The file on the server changed: start it again from nothing.
                restarted = true;
                let _ = tokio::fs::remove_file(state::part_path(&record.path)).await;
                let _ = tokio::fs::remove_file(state::state_path(&record.path)).await;
                let mut items = inner.items();
                if let Some(i) = items.iter_mut().find(|i| i.record.id == id) {
                    i.record.probed = false;
                    i.record.done = 0;
                }
            }
            other => break other,
        }
    };

    let mut event = None;
    let mut hash_path = None;
    {
        let mut items = inner.items();
        let Some(item) = items.iter_mut().find(|i| i.record.id == id) else {
            return;
        };
        item.stop = None;
        let segments = item.live.segments().clone();
        if item.record.kind == Kind::File && !segments.is_empty() {
            item.record.done = plan::done_bytes(&segments);
        }
        match result {
            Ok(Outcome::Done(path)) => {
                item.record.status = Status::Done;
                item.record.finished = Some(now_ms());
                if item.record.length.is_none() || item.record.kind == Kind::Hls {
                    item.record.length = std::fs::metadata(&path).ok().map(|m| m.len());
                }
                item.record.done = item.record.length.unwrap_or(item.record.done);
                item.record.path.clone_from(&path);
                // Cookies are kept only while they are needed.
                item.record.source.cookies = None;
                hash_path = Some(path.clone());
                event = Some(Event::Done {
                    id: id.clone(),
                    name: file_name_of(&path),
                    path: path.to_string_lossy().into_owned(),
                });
            }
            Ok(Outcome::Stopped) => {
                if item.record.status == Status::Downloading {
                    item.record.status = Status::Paused;
                }
            }
            Err(e) => {
                item.record.status = Status::Failed;
                item.record.error = Some((e.code().to_owned(), e.to_string()));
                event = Some(Event::Failed {
                    id: id.clone(),
                    name: file_name_of(&item.record.path),
                    code: e.code().to_owned(),
                    detail: e.to_string(),
                });
            }
        }
    }
    inner.changed();
    if let Some(event) = event {
        (inner.listener)(event);
    }
    schedule(&inner);
    if let Some(path) = hash_path {
        let inner = Arc::clone(&inner);
        let sum = tokio::task::spawn_blocking(move || crate::sha256_file(&path).ok())
            .await
            .ok()
            .flatten();
        let mut items = inner.items();
        if let Some(item) = items.iter_mut().find(|i| i.record.id == id) {
            item.record.sha256 = sum;
        }
        drop(items);
        inner.changed();
    }
}
