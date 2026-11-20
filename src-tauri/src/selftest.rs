// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! `--selftest`: what a machine can check about the binary it is given —
//! including a real download, through the real engine, from a server this
//! process starts on 127.0.0.1. CI runs it on the binary it is about to publish.

use crate::checks;
use eon_dm_engine::{AddRequest, Engine, Settings, Source, Start, Status};
use std::fmt::Write as _;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7919) ^ (i >> 9)) as u8).collect()
}

/// A tiny HTTP/1.1 server with byte ranges, enough to make the engine split.
fn serve(body: Vec<u8>) -> Option<(String, tokio::runtime::Runtime)> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .ok()?;
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .ok()?;
    let base = format!("http://{}", listener.local_addr().ok()?);
    let body = std::sync::Arc::new(body);
    rt.spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let body = std::sync::Arc::clone(&body);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                loop {
                    let head = loop {
                        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                            buf.drain(..end + 4);
                            break head;
                        }
                        let mut chunk = [0u8; 2048];
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let total = body.len();
                    let range = head
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes="))
                        .and_then(|r| r.split_once('-'))
                        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().unwrap_or(total - 1).min(total - 1))));
                    let (status, slice, extra) = match range {
                        Some((a, b)) => ("206 Partial Content", &body[a..=b], format!("Content-Range: bytes {a}-{b}/{total}\r\n")),
                        None => ("200 OK", &body[..], String::new()),
                    };
                    let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"selftest\"\r\n{extra}\r\n", slice.len());
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    for piece in slice.chunks(32 * 1024) {
                        if socket.write_all(piece).await.is_err() {
                            return;
                        }
                        // Slow enough that more than one connection opens.
                        tokio::time::sleep(Duration::from_millis(4)).await;
                    }
                }
            });
        }
    });
    Some((base, rt))
}

fn download() -> Vec<String> {
    let body = data(6 << 20);
    let expected = {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("eon-dm-selftest-src-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("x");
        let ok = std::fs::File::create(&p)
            .and_then(|mut f| f.write_all(&body))
            .is_ok();
        let sum = if ok {
            eon_dm_engine::sha256_file(&p).ok()
        } else {
            None
        };
        let _ = std::fs::remove_dir_all(&dir);
        sum
    };
    let Some((base, _rt)) = serve(body) else {
        return vec!["could not start a local server".into()];
    };
    let dir = std::env::temp_dir().join(format!("eon-dm-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let result = (|| -> Result<(), String> {
        let engine = Engine::new(
            &dir.join("data"),
            Settings {
                connections: 6,
                ..Settings::default()
            },
            |_| {},
        )
        .map_err(|e| e.to_string())?;
        let id = engine
            .add(AddRequest {
                source: Source {
                    url: format!("{base}/selftest.bin"),
                    ..Source::default()
                },
                probe: None,
                file_name: None,
                folder: dir.join("out"),
                start: Start::Now,
                variant: None,
                connections: None,
                media: None,
                convert: None,
            })
            .map_err(|e| e.to_string())?;
        let end = Instant::now() + Duration::from_secs(60);
        let mut peak = 0;
        loop {
            let v = engine.view(&id).ok_or("the download vanished")?;
            peak = peak.max(v.connections);
            if v.status == Status::Failed {
                return Err(format!("failed: {:?}", v.error.map(|e| e.detail)));
            }
            if v.status == Status::Done && v.sha256.is_some() {
                if v.sha256 != expected {
                    return Err("the downloaded bytes differ from the source".into());
                }
                if peak < 2 {
                    return Err(format!("only {peak} connection(s) opened"));
                }
                return Ok(());
            }
            if Instant::now() > end {
                return Err("took longer than a minute".into());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result.err().into_iter().collect()
}

pub fn run(index: &str, script: &str, commands: &[&str]) -> String {
    let mut report = format!(
        "EON Download Manager {} self-test\n",
        env!("CARGO_PKG_VERSION")
    );
    let mut failed = false;
    let mut section = |name: &str, problems: Vec<String>| {
        if problems.is_empty() {
            let _ = writeln!(report, "  ok    {name}");
        } else {
            failed = true;
            for p in problems {
                let _ = writeln!(report, "  FAIL  {name}: {p}");
            }
        }
    };
    section(
        "strings (en, tr)",
        checks::catalogue_problems(crate::EN, crate::TR),
    );
    section("interface", checks::ui_problems(index, script));
    section("commands", checks::unknown_commands(script, commands));
    let conf: serde_json::Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).unwrap_or_default();
    let wanted = env!("CARGO_PKG_VERSION")
        .split('-')
        .next()
        .unwrap_or_default();
    section(
        "version",
        if conf["version"] == wanted {
            Vec::new()
        } else {
            vec![format!(
                "tauri.conf.json says {}, the build is {}",
                conf["version"],
                env!("CARGO_PKG_VERSION")
            )]
        },
    );
    section(
        "a real download (6 MiB, several connections, SHA-256)",
        download(),
    );
    report.push_str(if failed { "FAILED\n" } else { "ok\n" });
    report
}
