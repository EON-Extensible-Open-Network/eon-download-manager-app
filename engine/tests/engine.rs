// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The engine against a real HTTP server on this machine: what people value in
//! a download manager, checked byte for byte.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::pedantic
)]

mod support;

use eon_dm_engine::{AddRequest, Engine, Settings, Source, Start, Status, View};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use support::{content, sha256, Options, Server};

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(name: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("eon-dm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Dir(d)
}

fn engine(data: &Path, settings: Settings) -> Engine {
    Engine::new(&data.join("data"), settings, |_| {}).unwrap()
}

fn add(engine: &Engine, url: &str, folder: &Path, start: Start) -> String {
    engine
        .add(AddRequest {
            source: Source {
                url: url.to_owned(),
                ..Source::default()
            },
            probe: None,
            file_name: None,
            folder: folder.to_owned(),
            start,
            variant: None,
            connections: None,
            media: None,
            convert: None,
        })
        .unwrap()
}

fn wait(engine: &Engine, id: &str, secs: u64, until: impl Fn(&View) -> bool) -> View {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let v = engine.view(id).unwrap();
        if until(&v) {
            return v;
        }
        assert!(
            Instant::now() < end,
            "timed out; last view: status {:?}, {} of {:?}, error {:?}",
            v.status,
            v.done,
            v.length,
            v.error
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn done(engine: &Engine, id: &str, secs: u64) -> View {
    let v = wait(engine, id, secs, |v| {
        matches!(v.status, Status::Done | Status::Failed)
    });
    assert_eq!(v.status, Status::Done, "{:?}", v.error);
    // The checksum is computed just after.
    wait(engine, id, 20, |v| v.sha256.is_some())
}

#[test]
fn several_connections_split_the_file_and_the_bytes_are_exact() {
    let d = dir("multi");
    let server = Server::start(Options {
        length: 12 << 20,
        throttle: 3 << 20,
        ..Options::default()
    });
    let e = engine(
        &d.0,
        Settings {
            connections: 8,
            ..Settings::default()
        },
    );
    let id = add(
        &e,
        &format!("{}/file/ubuntu.iso", server.base),
        &d.0.join("out"),
        Start::Now,
    );
    // While it runs, the lanes show the splits.
    let busy = wait(&e, &id, 20, |v| v.lanes.len() >= 4);
    assert!(busy.connections >= 2, "connections: {}", busy.connections);
    let v = done(&e, &id, 60);
    assert_eq!(v.name, "ubuntu.iso");
    assert_eq!(v.sha256.as_deref().unwrap(), sha256(&content(12 << 20, 1)));
    assert_eq!(std::fs::read(&v.path).unwrap(), content(12 << 20, 1));
    assert!(
        server.stats.peak.load(Ordering::SeqCst) >= 4,
        "peak connections {}",
        server.stats.peak.load(Ordering::SeqCst)
    );
    // Nothing fetched twice beyond a few split overlaps.
    let served = server.stats.served.load(Ordering::SeqCst);
    assert!(served < (12 << 20) + (4 << 20), "served {served}");
    assert!(
        !Path::new(&format!("{}.part", v.path)).exists()
            && !Path::new(&format!("{}.eondl", v.path)).exists()
    );
}

#[test]
fn a_crash_mid_download_resumes_from_the_saved_state() {
    let d = dir("resume");
    let length = 16 << 20;
    let server = Server::start(Options {
        length,
        throttle: 1 << 20,
        ..Options::default()
    });
    let out = d.0.join("out");
    let url = format!("{}/file/big.bin", server.base);
    let id = {
        let e = engine(
            &d.0,
            Settings {
                connections: 4,
                ..Settings::default()
            },
        );
        let id = add(&e, &url, &out, Start::Now);
        wait(&e, &id, 30, |v| v.done > length / 4);
        std::thread::sleep(Duration::from_millis(1200));
        e.flush();
        id
        // Dropped without pausing: as if the power went.
    };
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        out.join("big.bin.eondl").exists(),
        "the resume state is on disk"
    );
    let before = server.stats.served.load(Ordering::SeqCst);
    let e = engine(
        &d.0,
        Settings {
            connections: 4,
            ..Settings::default()
        },
    );
    let v = done(&e, &id, 90);
    assert_eq!(v.sha256.as_deref().unwrap(), sha256(&content(length, 1)));
    let after = server.stats.served.load(Ordering::SeqCst) - before;
    assert!(
        after < length * 4 / 5,
        "only the rest is fetched again: {after} of {length}"
    );
}

#[test]
fn pause_and_resume_keep_every_byte() {
    let d = dir("pause");
    let length = 8 << 20;
    let server = Server::start(Options {
        length,
        throttle: 2 << 20,
        ..Options::default()
    });
    let e = engine(&d.0, Settings::default());
    let id = add(
        &e,
        &format!("{}/file/a.bin", server.base),
        &d.0.join("out"),
        Start::Now,
    );
    wait(&e, &id, 30, |v| v.done > length / 3);
    e.pause(&id);
    let paused = wait(&e, &id, 10, |v| {
        v.status == Status::Paused && v.connections == 0
    });
    assert!(paused.done > length / 3);
    e.resume(&id);
    let v = done(&e, &id, 60);
    assert_eq!(v.sha256.as_deref().unwrap(), sha256(&content(length, 1)));
}

#[test]
fn a_server_without_ranges_still_delivers_the_file() {
    let d = dir("noranges");
    let server = Server::start(Options {
        length: 3 << 20,
        ranges: false,
        ..Options::default()
    });
    let e = engine(&d.0, Settings::default());
    let id = add(
        &e,
        &format!("{}/file/plain.bin", server.base),
        &d.0.join("out"),
        Start::Now,
    );
    let v = done(&e, &id, 30);
    assert!(!v.resumable);
    assert_eq!(v.sha256.as_deref().unwrap(), sha256(&content(3 << 20, 1)));
}

#[test]
fn a_file_that_changed_on_the_server_is_fetched_again_whole() {
    let d = dir("changed");
    let length = 6 << 20;
    let server = Server::start(Options {
        length,
        throttle: 2 << 20,
        ..Options::default()
    });
    let e = engine(&d.0, Settings::default());
    let id = add(
        &e,
        &format!("{}/file/c.bin", server.base),
        &d.0.join("out"),
        Start::Now,
    );
    wait(&e, &id, 30, |v| v.done > length / 4);
    e.pause(&id);
    wait(&e, &id, 10, |v| v.status == Status::Paused);
    server.change();
    e.resume(&id);
    let v = done(&e, &id, 60);
    assert_eq!(
        v.sha256.as_deref().unwrap(),
        sha256(&content(length, 2)),
        "never a mix of old and new"
    );
}

#[test]
fn an_expired_link_takes_a_new_one_and_keeps_its_bytes() {
    let d = dir("refresh");
    let length = 8 << 20;
    let server = Server::start(Options {
        length,
        throttle: 2 << 20,
        ..Options::default()
    });
    let e = engine(&d.0, Settings::default());
    let id = add(
        &e,
        &format!("{}/file/old-link.bin", server.base),
        &d.0.join("out"),
        Start::Now,
    );
    wait(&e, &id, 30, |v| v.done > length / 3);
    e.pause(&id);
    wait(&e, &id, 10, |v| v.status == Status::Paused);
    server.expire("/file/old-link.bin");
    e.resume(&id);
    let failed = wait(&e, &id, 60, |v| v.status == Status::Failed);
    assert_eq!(failed.error.unwrap().code, "refused");
    let before = server.stats.served.load(Ordering::SeqCst);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(e.refresh(
        &id,
        Source {
            url: format!("{}/file/new-link.bin", server.base),
            ..Source::default()
        },
    ))
    .unwrap();
    let v = done(&e, &id, 60);
    assert_eq!(v.sha256.as_deref().unwrap(), sha256(&content(length, 1)));
    let after = server.stats.served.load(Ordering::SeqCst) - before;
    assert!(
        after < length * 3 / 4,
        "the bytes already there were kept: {after}"
    );
}

#[test]
fn a_server_that_limits_connections_is_respected() {
    let d = dir("limit");
    let server = Server::start(Options {
        length: 6 << 20,
        throttle: 3 << 20,
        max_connections: 2,
        ..Options::default()
    });
    let e = engine(
        &d.0,
        Settings {
            connections: 8,
            ..Settings::default()
        },
    );
    let id = add(
        &e,
        &format!("{}/file/strict.bin", server.base),
        &d.0.join("out"),
        Start::Now,
    );
    let v = done(&e, &id, 60);
    assert_eq!(v.sha256.as_deref().unwrap(), sha256(&content(6 << 20, 1)));
}

#[test]
fn the_speed_limit_holds() {
    let d = dir("speed");
    let server = Server::start(Options {
        length: 3 << 20,
        ..Options::default()
    });
    let e = engine(
        &d.0,
        Settings {
            speed_limit: 1 << 20,
            ..Settings::default()
        },
    );
    let started = Instant::now();
    let id = add(
        &e,
        &format!("{}/file/slow.bin", server.base),
        &d.0.join("out"),
        Start::Now,
    );
    done(&e, &id, 30);
    assert!(
        started.elapsed() >= Duration::from_millis(2300),
        "3 MiB at 1 MiB/s took {:?}",
        started.elapsed()
    );
}

#[test]
fn an_encrypted_hls_video_downloads_and_decrypts() {
    let d = dir("hls");
    let server = Server::start(Options::default());
    let e = engine(&d.0, Settings::default());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let source = Source {
        url: format!("{}/video/index.m3u8", server.base),
        ..Source::default()
    };
    let probe = rt.block_on(e.probe(source.clone())).unwrap();
    let hls = probe.hls.clone().unwrap();
    assert_eq!(hls.pieces as u64, support::HLS_PIECES);
    assert_eq!(probe.file_name, "index.ts");
    let id = e
        .add(AddRequest {
            source,
            probe: Some(probe),
            file_name: Some("talk.ts".into()),
            folder: d.0.join("out"),
            start: Start::Now,
            variant: None,
            connections: None,
            media: None,
            convert: None,
        })
        .unwrap();
    let v = done(&e, &id, 30);
    let expected: Vec<u8> = (0..support::HLS_PIECES)
        .flat_map(support::hls_piece)
        .collect();
    assert_eq!(std::fs::read(&v.path).unwrap(), expected);
    assert_eq!(v.pieces, Some((support::HLS_PIECES, support::HLS_PIECES)));
}

#[test]
fn the_queue_runs_downloads_a_few_at_a_time_and_survives_a_restart() {
    let d = dir("queue");
    let server = Server::start(Options {
        length: 1 << 20,
        throttle: 2 << 20,
        ..Options::default()
    });
    let out = d.0.join("out");
    let ids: Vec<String> = {
        let e = engine(
            &d.0,
            Settings {
                max_active: 2,
                queue_running: false,
                ..Settings::default()
            },
        );
        let ids: Vec<String> = (0..5)
            .map(|i| {
                add(
                    &e,
                    &format!("{}/file/q{i}.bin", server.base),
                    &out,
                    Start::Queue,
                )
            })
            .collect();
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            e.views().iter().all(|v| v.status == Status::Queued),
            "the queue is stopped"
        );
        e.flush();
        ids
    };
    let e = engine(
        &d.0,
        Settings {
            max_active: 2,
            ..Settings::default()
        },
    );
    let mut peak = 0;
    let end = Instant::now() + Duration::from_secs(60);
    while e.views().iter().any(|v| v.status != Status::Done) {
        peak = peak.max(
            e.views()
                .iter()
                .filter(|v| v.status == Status::Downloading)
                .count(),
        );
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(peak <= 2, "at most two at a time: {peak}");
    assert_eq!(e.views().len(), ids.len());
}

#[test]
fn connections_change_while_downloading_without_losing_a_byte() {
    let d = dir("liveconn");
    let length = 24 << 20;
    let server = Server::start(Options {
        length,
        throttle: 1 << 20,
        ..Options::default()
    });
    let e = engine(
        &d.0,
        Settings {
            connections: 8,
            ..Settings::default()
        },
    );
    let id = add(
        &e,
        &format!("{}/file/live.bin", server.base),
        &d.0.join("out"),
        Start::Now,
    );
    wait(&e, &id, 30, |v| v.connections >= 6);
    // Fewer: the newest connections finish what they hold and step back.
    e.set_settings(Settings {
        connections: 2,
        ..Settings::default()
    });
    let fewer = wait(&e, &id, 20, |v| v.connections <= 2);
    assert_eq!(fewer.max_connections, 2);
    std::thread::sleep(Duration::from_millis(600));
    assert!(e.view(&id).unwrap().connections <= 2, "it stays at two");
    // More again, for this download only.
    e.set_connections(&id, Some(6));
    wait(&e, &id, 20, |v| v.connections >= 5);
    let v = done(&e, &id, 120);
    assert_eq!(v.sha256.as_deref().unwrap(), sha256(&content(length, 1)));
    let served = server.stats.served.load(Ordering::SeqCst);
    assert!(
        served < length + (6 << 20),
        "retired connections waste little: {served}"
    );
}

#[test]
fn fewer_downloads_at_once_sends_the_newest_back_to_the_queue() {
    let d = dir("liveactive");
    let length = 6 << 20;
    let server = Server::start(Options {
        length,
        throttle: 1 << 20,
        ..Options::default()
    });
    let e = engine(
        &d.0,
        Settings {
            max_active: 3,
            connections: 2,
            ..Settings::default()
        },
    );
    let ids: Vec<String> = (0..3)
        .map(|i| {
            add(
                &e,
                &format!("{}/file/a{i}.bin", server.base),
                &d.0.join("out"),
                Start::Queue,
            )
        })
        .collect();
    let end = Instant::now() + Duration::from_secs(20);
    while e
        .views()
        .iter()
        .filter(|v| v.status == Status::Downloading)
        .count()
        < 3
    {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(50));
    }
    e.set_settings(Settings {
        max_active: 1,
        connections: 2,
        ..Settings::default()
    });
    std::thread::sleep(Duration::from_millis(800));
    let downloading = e
        .views()
        .iter()
        .filter(|v| v.status == Status::Downloading)
        .count();
    assert_eq!(downloading, 1, "one at a time now");
    for id in &ids {
        let v = done(&e, id, 120);
        assert_eq!(
            v.sha256.as_deref().unwrap(),
            sha256(&content(length, 1)),
            "{}",
            v.name
        );
    }
}
