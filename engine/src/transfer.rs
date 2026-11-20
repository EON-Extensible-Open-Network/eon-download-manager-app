// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! One file, many connections.
//!
//! A conductor opens connections one at a time (every 150 ms, up to the
//! limit), hands each the work `plan::next` chooses, writes the resume state
//! at least once a second, and finishes the file when every byte is there.
//! Connections write at their own offsets into one pre-sized `.part` file.

use crate::http::{self, Source};
use crate::limiter::Limiter;
use crate::plan::{self, Assignment, Segment};
use crate::state::{self, ResumeState, Validators};
use crate::Error;
use futures_util::StreamExt;
use reqwest::header;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinSet;

/// Data is written in pieces of at most this size.
const FLUSH: usize = 512 * 1024;
/// A new connection opens at most this often.
const RAMP: Duration = Duration::from_millis(150);
/// Give up after this many failures in a row with no byte arriving in between.
const MAX_FAILURES: u32 = 12;

/// What the interface can watch while a file downloads.
#[derive(Default)]
pub struct Live {
    pub segments: Mutex<Vec<Segment>>,
    /// Bytes received from the network, all connections, ever.
    pub received: AtomicU64,
    pub connections: AtomicU32,
}

impl Live {
    pub fn segments(&self) -> std::sync::MutexGuard<'_, Vec<Segment>> {
        self.segments.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Everything a transfer needs.
pub struct Job {
    pub client: reqwest::Client,
    pub source: Source,
    /// Where the bytes are fetched from (after redirects).
    pub url: String,
    /// The finished file; `.part` and `.eondl` sit beside it.
    pub target: PathBuf,
    pub length: Option<u64>,
    pub resumable: bool,
    pub validators: Validators,
    pub connections: u32,
    /// Trust a resumed file even if the server says it changed.
    pub ignore_changes: bool,
    pub global: Arc<Limiter>,
    pub own: Arc<Limiter>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every byte is in the `.part` file, which has been renamed into place.
    Done(PathBuf),
    /// Stopped on request; the state on disk is current.
    Stopped,
}

impl Job {
    fn resume_state(&self, segments: Vec<Segment>) -> ResumeState {
        ResumeState {
            eondl: 0,
            url: self.source.url.clone(),
            final_url: (self.url != self.source.url).then(|| self.url.clone()),
            referrer: self.source.referrer.clone(),
            cookies: self.source.cookies.clone(),
            user_agent: self.source.user_agent.clone(),
            file: self
                .target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            length: self.length,
            resumable: self.resumable,
            validators: Some(self.validators.clone()).filter(|v| v != &Validators::default()),
            segments,
            hls: None,
        }
    }
}

/// The segments to start from: the state on disk if it matches this file and
/// the `.part` file is there, otherwise a fresh plan.
pub fn starting_plan(job: &Job) -> Vec<Segment> {
    let Some(length) = job.length.filter(|_| job.resumable) else {
        return Vec::new();
    };
    let part = state::part_path(&job.target);
    let resumed = ResumeState::load(&state::state_path(&job.target))
        .filter(|s| s.length == Some(length) && s.resumable && part.exists())
        .and_then(|s| plan::restore(s.segments, length));
    resumed.unwrap_or_else(|| plan::initial(length))
}

/// Download the file. `stop` turning `true` pauses: connections end, the state
/// is written, and `Outcome::Stopped` comes back.
pub async fn run(
    job: Job,
    live: Arc<Live>,
    mut stop: watch::Receiver<bool>,
) -> Result<Outcome, Error> {
    if let Some(dir) = job.target.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(io)?;
    }
    let outcome = if job.resumable && job.length.is_some() {
        segmented(&job, &live, &mut stop).await?
    } else {
        single(&job, &live, &mut stop).await?
    };
    if outcome == Outcome::Stopped {
        return Ok(outcome);
    }
    finish(&job).await.map(Outcome::Done)
}

/// Truncate, rename into place (never over another file), forget the state.
async fn finish(job: &Job) -> Result<PathBuf, Error> {
    let part = state::part_path(&job.target);
    let target = job.target.clone();
    let length = job.length;
    let done = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
        let file = std::fs::OpenOptions::new().write(true).open(&part)?;
        if let Some(length) = length {
            file.set_len(length)?;
        }
        file.sync_all()?;
        drop(file);
        let dest = if target.exists() {
            let folder = target.parent().map(PathBuf::from).unwrap_or_default();
            let name = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            crate::name::unique(&folder, &name)
        } else {
            target.clone()
        };
        std::fs::rename(&part, &dest)?;
        let _ = std::fs::remove_file(state::state_path(&target));
        Ok(dest)
    })
    .await
    .map_err(|e| Error::Io(e.to_string()))?
    .map_err(io)?;
    Ok(done)
}

#[allow(clippy::needless_pass_by_value)] // the shape `map_err` wants
fn io(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

#[cfg(unix)]
fn write_at(file: &std::fs::File, data: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(data, offset)
}

#[cfg(windows)]
fn write_at(file: &std::fs::File, mut data: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !data.is_empty() {
        let n = file.seek_write(data, offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        data = &data[n..];
        offset += n as u64;
    }
    Ok(())
}

/// How a connection ended.
enum End {
    /// Its segment is complete.
    Finished,
    /// Asked to stop.
    Stopped,
    /// Failed; the segment goes back to the plan.
    Failed(Error),
}

struct Shared {
    file: Arc<std::fs::File>,
    live: Arc<Live>,
    url: String,
    source: Source,
    client: reqwest::Client,
    if_range: Option<String>,
    global: Arc<Limiter>,
    own: Arc<Limiter>,
}

#[allow(clippy::too_many_lines)] // the conductor loop reads best whole
async fn segmented(
    job: &Job,
    live: &Arc<Live>,
    stop: &mut watch::Receiver<bool>,
) -> Result<Outcome, Error> {
    let length = job.length.unwrap_or(0);
    let part = state::part_path(&job.target);
    let state_file = state::state_path(&job.target);
    let segments = starting_plan(job);
    let fresh = plan::done_bytes(&segments) == 0;
    let file = {
        let part = part.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&part)?;
            if fresh || f.metadata()?.len() != length {
                f.set_len(length)?;
            }
            Ok(f)
        })
        .await
        .map_err(|e| Error::Io(e.to_string()))?
        .map_err(io)?
    };
    *live.segments() = segments;
    job.resume_state(live.segments().clone())
        .save(&state_file)
        .map_err(io)?;

    let shared = Arc::new(Shared {
        file: Arc::new(file),
        live: Arc::clone(live),
        url: job.url.clone(),
        source: job.source.clone(),
        client: job.client.clone(),
        if_range: if job.ignore_changes {
            None
        } else {
            job.validators.if_range().map(str::to_owned)
        },
        global: Arc::clone(&job.global),
        own: Arc::clone(&job.own),
    });

    let mut limit = job.connections.clamp(1, 32);
    let mut workers: JoinSet<(u32, End)> = JoinSet::new();
    let mut next_conn = 1u32;
    let mut next_open = Instant::now();
    let mut last_save = Instant::now();
    let mut failures = 0u32;
    let mut cooldown: HashMap<u32, Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut stopping = *stop.borrow();
    let mut last_error = None;

    loop {
        tokio::select! {
            Some(joined) = workers.join_next(), if !workers.is_empty() => {
                let (seg, end) = joined.map_err(|e| Error::Io(e.to_string()))?;
                live.connections.store(workers.len() as u32, Ordering::Relaxed);
                {
                    let mut segs = live.segments();
                    if let Some(s) = segs.iter_mut().find(|s| s.id == seg) {
                        s.owner = None;
                    }
                }
                match end {
                    End::Finished => failures = 0,
                    End::Stopped => {}
                    End::Failed(Error::Changed) => {
                        stop_all(&mut workers).await;
                        return Err(Error::Changed);
                    }
                    End::Failed(Error::Busy) => {
                        // The server limits connections: settle one below where it
                        // complained (IDM's per-server exception, learnt).
                        limit = (workers.len() as u32).max(1);
                        cooldown.insert(seg, Instant::now() + Duration::from_secs(2));
                        failures += 1;
                        last_error = Some(Error::Busy);
                    }
                    End::Failed(e) => {
                        failures += 1;
                        let wait = Duration::from_secs(1u64 << failures.min(5)).min(Duration::from_secs(30));
                        cooldown.insert(seg, Instant::now() + wait);
                        last_error = Some(e);
                    }
                }
                if failures >= MAX_FAILURES {
                    stop_all(&mut workers).await;
                    save(job, live, &state_file);
                    return Err(last_error.unwrap_or_else(|| Error::Network("too many failures".into())));
                }
            }
            changed = stop.changed() => {
                stopping = changed.is_err() || *stop.borrow();
            }
            _ = tick.tick() => {}
        }

        if stopping {
            stop_all(&mut workers).await;
            live.connections.store(0, Ordering::Relaxed);
            save(job, live, &state_file);
            return Ok(Outcome::Stopped);
        }
        if plan::complete(&live.segments()) {
            stop_all(&mut workers).await;
            live.connections.store(0, Ordering::Relaxed);
            break;
        }
        let now = Instant::now();
        if (workers.len() as u32) < limit && now >= next_open {
            let chosen = {
                let mut segs = live.segments();
                let usable: Vec<Segment> = segs
                    .iter()
                    .map(|s| {
                        let mut s = s.clone();
                        // A segment cooling down after a failure counts as owned.
                        if cooldown.get(&s.id).is_some_and(|t| *t > now) {
                            s.owner = Some(u32::MAX);
                        }
                        s
                    })
                    .collect();
                match plan::next(&usable) {
                    Some(Assignment::Take(id)) => {
                        if let Some(s) = segs.iter_mut().find(|s| s.id == id) {
                            s.owner = Some(next_conn);
                        }
                        Some(id)
                    }
                    Some(Assignment::Split { id, at }) => {
                        plan::split(&mut segs, id, at).inspect(|&new| {
                            if let Some(s) = segs.iter_mut().find(|s| s.id == new) {
                                s.owner = Some(next_conn);
                            }
                        })
                    }
                    None => None,
                }
            };
            if let Some(seg) = chosen {
                let shared = Arc::clone(&shared);
                let stop = stop.clone();
                workers.spawn(async move { (seg, fetch(shared, seg, stop).await) });
                next_conn += 1;
                next_open = now + RAMP;
                live.connections
                    .store(workers.len() as u32, Ordering::Relaxed);
            }
        }
        if last_save.elapsed() >= Duration::from_secs(1) {
            save(job, live, &state_file);
            last_save = Instant::now();
        }
    }
    save(job, live, &state_file);
    Ok(Outcome::Done(job.target.clone()))
}

fn save(job: &Job, live: &Live, path: &std::path::Path) {
    let segments = live.segments().clone();
    let _ = job.resume_state(segments).save(path);
}

async fn stop_all(workers: &mut JoinSet<(u32, End)>) {
    // Workers watch the stop signal or finish on their own; give them a moment,
    // then end the rest. Their written bytes are already counted.
    let deadline = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            next = workers.join_next() => if next.is_none() { break },
            () = &mut deadline => { workers.abort_all(); while workers.join_next().await.is_some() {} break }
        }
    }
}

/// One connection working on one segment until it is complete, cut short by
/// a split, stopped, or failed.
async fn fetch(shared: Arc<Shared>, seg: u32, mut stop: watch::Receiver<bool>) -> End {
    let (pos, end) = {
        let segs = shared.live.segments();
        match segs.iter().find(|s| s.id == seg) {
            Some(s) => (s.pos, s.end),
            None => return End::Finished,
        }
    };
    if pos >= end {
        return End::Finished;
    }
    let mut request = http::get(&shared.client, &shared.source, &shared.url)
        .header(header::RANGE, format!("bytes={pos}-{}", end - 1));
    if let Some(v) = &shared.if_range {
        request = request.header(header::IF_RANGE, v.as_str());
    }
    let response = tokio::select! {
        r = request.send() => match r { Ok(r) => r, Err(e) => return End::Failed(http::network(e)) },
        _ = stop.changed() => return End::Stopped,
    };
    let status = response.status().as_u16();
    if status == 200 {
        // The validator no longer matches: the file on the server changed.
        return End::Failed(Error::Changed);
    }
    if status != 206 {
        return End::Failed(http::status_error(response.status()));
    }
    let starts_at = response
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes "))
        .and_then(|v| v.split('-').next())
        .and_then(|v| v.parse::<u64>().ok());
    if starts_at != Some(pos) {
        return End::Failed(Error::Network("the server sent a different range".into()));
    }
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::with_capacity(FLUSH);
    let mut write_pos = pos;
    loop {
        let chunk = tokio::select! {
            c = stream.next() => c,
            _ = stop.changed() => {
                let _ = flush(&shared, seg, &mut write_pos, &mut buffer).await;
                return End::Stopped;
            }
        };
        match chunk {
            Some(Ok(bytes)) => {
                let n = bytes.len() as u64;
                shared.live.received.fetch_add(n, Ordering::Relaxed);
                shared.global.take(n).await;
                shared.own.take(n).await;
                buffer.extend_from_slice(&bytes);
                if buffer.len() >= FLUSH {
                    match flush(&shared, seg, &mut write_pos, &mut buffer).await {
                        Ok(true) => return End::Finished,
                        Ok(false) => {}
                        Err(e) => return End::Failed(e),
                    }
                }
            }
            Some(Err(e)) => {
                let _ = flush(&shared, seg, &mut write_pos, &mut buffer).await;
                return End::Failed(http::network(e));
            }
            None => {
                return match flush(&shared, seg, &mut write_pos, &mut buffer).await {
                    Ok(true) => End::Finished,
                    Ok(false) => End::Failed(Error::Network("the connection closed early".into())),
                    Err(e) => End::Failed(e),
                };
            }
        }
    }
}

/// Write what is buffered, but never past the segment's end — it may have been
/// split while the bytes were arriving. `Ok(true)` when the segment is done.
async fn flush(
    shared: &Shared,
    seg: u32,
    write_pos: &mut u64,
    buffer: &mut Vec<u8>,
) -> Result<bool, Error> {
    let end = {
        let segs = shared.live.segments();
        segs.iter()
            .find(|s| s.id == seg)
            .map_or(*write_pos, |s| s.end)
    };
    let room = end.saturating_sub(*write_pos) as usize;
    let n = buffer.len().min(room);
    if n > 0 {
        let data: Vec<u8> = buffer.drain(..n).collect();
        let file = Arc::clone(&shared.file);
        let offset = *write_pos;
        tokio::task::spawn_blocking(move || write_at(&file, &data, offset))
            .await
            .map_err(|e| Error::Io(e.to_string()))?
            .map_err(io)?;
        *write_pos += n as u64;
    }
    buffer.clear();
    let mut segs = shared.live.segments();
    let Some(s) = segs.iter_mut().find(|s| s.id == seg) else {
        return Ok(true);
    };
    // Bytes written past a split are identical to what the other connection
    // fetches, so clamping is enough.
    s.pos = (*write_pos).min(s.end).max(s.pos);
    Ok(s.pos >= s.end)
}

/// One connection, no ranges: the server does not support them, or did not
/// say how big the file is. A stop means starting again next time.
async fn single(
    job: &Job,
    live: &Arc<Live>,
    stop: &mut watch::Receiver<bool>,
) -> Result<Outcome, Error> {
    let part = state::part_path(&job.target);
    let file = std::fs::File::create(&part).map_err(io)?;
    let file = Arc::new(file);
    *live.segments() = vec![Segment {
        id: 0,
        start: 0,
        pos: 0,
        end: job.length.unwrap_or(0),
        owner: Some(1),
    }];
    live.connections.store(1, Ordering::Relaxed);
    let response = http::get(&job.client, &job.source, &job.url)
        .send()
        .await
        .map_err(http::network)?;
    if !response.status().is_success() {
        return Err(http::status_error(response.status()));
    }
    let mut stream = response.bytes_stream();
    let mut written = 0u64;
    loop {
        let chunk = tokio::select! {
            c = stream.next() => c,
            _ = stop.changed() => {
                live.connections.store(0, Ordering::Relaxed);
                return Ok(Outcome::Stopped);
            }
        };
        match chunk {
            Some(Ok(bytes)) => {
                let n = bytes.len() as u64;
                live.received.fetch_add(n, Ordering::Relaxed);
                job.global.take(n).await;
                job.own.take(n).await;
                let f = Arc::clone(&file);
                let offset = written;
                tokio::task::spawn_blocking(move || write_at(&f, &bytes, offset))
                    .await
                    .map_err(|e| Error::Io(e.to_string()))?
                    .map_err(io)?;
                written += n;
                let mut segs = live.segments();
                if let Some(s) = segs.first_mut() {
                    s.pos = written;
                    s.end = s.end.max(written);
                }
            }
            Some(Err(e)) => return Err(http::network(e)),
            None => break,
        }
    }
    live.connections.store(0, Ordering::Relaxed);
    if job.length.is_some_and(|l| l != written) {
        return Err(Error::Network("the connection closed early".into()));
    }
    Ok(Outcome::Done(job.target.clone()))
}
