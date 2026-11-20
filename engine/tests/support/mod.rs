// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! A small HTTP/1.1 server for the engine's tests: byte ranges, `If-Range`,
//! keep-alive, per-connection throttling, a connection limit answered with
//! 503, a mode that ignores ranges, a file that changes, expiring addresses,
//! and an AES-128 HLS stream.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub fn content(length: u64, version: u64) -> Vec<u8> {
    (0..length)
        .map(|i| {
            ((i.wrapping_mul(2_654_435_761) >> 13) ^ (i >> 7) ^ version.wrapping_mul(97)) as u8
        })
        .collect()
}

pub fn sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Default)]
pub struct Stats {
    pub requests: AtomicUsize,
    pub open: AtomicUsize,
    pub peak: AtomicUsize,
    pub served: AtomicU64,
    pub refused: AtomicUsize,
}

pub struct Options {
    pub length: u64,
    /// Bytes per second per connection; 0 is unthrottled.
    pub throttle: u64,
    /// More simultaneous connections than this get 503.
    pub max_connections: usize,
    pub ranges: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            length: 8 << 20,
            throttle: 0,
            max_connections: 64,
            ranges: true,
        }
    }
}

pub struct Server {
    pub base: String,
    pub stats: Arc<Stats>,
    version: Arc<AtomicU64>,
    expired: Arc<Mutex<HashSet<String>>>,
    pub ranges: Arc<AtomicBool>,
    _rt: tokio::runtime::Runtime,
}

pub const HLS_KEY: [u8; 16] = *b"0123456789abcdef";
pub const HLS_PIECES: u64 = 5;
pub const HLS_PIECE: u64 = 40_000;

impl Server {
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// The file changes on the server: new bytes, new ETag.
    pub fn change(&self) {
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// Requests to this path are refused with 403 from now on.
    pub fn expire(&self, path: &str) {
        self.expired.lock().unwrap().insert(path.to_owned());
    }

    pub fn start(options: Options) -> Server {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stats = Arc::new(Stats::default());
        let version = Arc::new(AtomicU64::new(1));
        let expired = Arc::new(Mutex::new(HashSet::new()));
        let ranges = Arc::new(AtomicBool::new(options.ranges));
        let options = Arc::new(options);
        let (s, v, e, r) = (
            Arc::clone(&stats),
            Arc::clone(&version),
            Arc::clone(&expired),
            Arc::clone(&ranges),
        );
        rt.spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let (s, v, e, r, o) = (
                    Arc::clone(&s),
                    Arc::clone(&v),
                    Arc::clone(&e),
                    Arc::clone(&r),
                    Arc::clone(&options),
                );
                tokio::spawn(async move {
                    let now = s.open.fetch_add(1, Ordering::SeqCst) + 1;
                    s.peak.fetch_max(now, Ordering::SeqCst);
                    let _ = serve(socket, &s, &v, &e, &r, &o, now).await;
                    s.open.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Server {
            base,
            stats,
            version,
            expired,
            ranges,
            _rt: rt,
        }
    }
}

async fn serve(
    mut socket: TcpStream,
    stats: &Stats,
    version: &AtomicU64,
    expired: &Mutex<HashSet<String>>,
    ranges: &AtomicBool,
    options: &Options,
    open_at_start: usize,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    loop {
        // Read one request head.
        let head = loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                buf.drain(..end + 4);
                break head;
            }
            let mut chunk = [0u8; 4096];
            let n = socket.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        stats.requests.fetch_add(1, Ordering::SeqCst);
        let mut lines = head.lines();
        let path = lines
            .next()
            .unwrap_or_default()
            .split(' ')
            .nth(1)
            .unwrap_or("/")
            .to_owned();
        let header = |name: &str| {
            head.lines().skip(1).find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.trim().to_owned())
            })
        };
        if stats.open.load(Ordering::SeqCst) > options.max_connections
            || open_at_start > options.max_connections
        {
            stats.refused.fetch_add(1, Ordering::SeqCst);
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
            return Ok(());
        }
        if expired.lock().unwrap().contains(&path) {
            socket
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .await?;
            continue;
        }
        let v = version.load(Ordering::SeqCst);
        let (body, mime, etag): (Vec<u8>, &str, String) = if path.starts_with("/file") {
            (
                content(options.length, v),
                "application/octet-stream",
                format!("\"v{v}\""),
            )
        } else if path == "/video/index.m3u8" {
            let mut p = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n");
            for i in 0..HLS_PIECES {
                p.push_str(&format!("#EXTINF:4.0,\npiece{i}.ts\n"));
            }
            p.push_str("#EXT-X-ENDLIST\n");
            (
                p.into_bytes(),
                "application/vnd.apple.mpegurl",
                "\"p\"".into(),
            )
        } else if path == "/video/key.bin" {
            (HLS_KEY.to_vec(), "application/octet-stream", "\"k\"".into())
        } else if let Some(i) = path
            .strip_prefix("/video/piece")
            .and_then(|p| p.strip_suffix(".ts"))
            .and_then(|n| n.parse::<u64>().ok())
        {
            let plain = hls_piece(i);
            let mut iv = [0u8; 16];
            iv[8..].copy_from_slice(&i.to_be_bytes());
            let enc = cbc::Encryptor::<aes::Aes128>::new((&HLS_KEY).into(), (&iv).into())
                .encrypt_padded_vec_mut::<Pkcs7>(&plain);
            (enc, "video/mp2t", "\"t\"".into())
        } else {
            socket
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await?;
            continue;
        };
        let total = body.len() as u64;
        let mut range = None;
        if ranges.load(Ordering::SeqCst) {
            if let Some(r) =
                header("range").and_then(|r| r.strip_prefix("bytes=").map(str::to_owned))
            {
                let if_range_ok = header("if-range").is_none_or(|t| t == etag);
                if if_range_ok {
                    let (a, b) = r.split_once('-').unwrap();
                    let a: u64 = a.parse().unwrap();
                    let b: u64 = if b.is_empty() {
                        total - 1
                    } else {
                        b.parse::<u64>().unwrap().min(total - 1)
                    };
                    range = Some((a, b));
                }
            }
        }
        let (status, slice, extra) = match range {
            Some((a, b)) => (
                "206 Partial Content",
                &body[a as usize..=b as usize],
                format!("Content-Range: bytes {a}-{b}/{total}\r\n"),
            ),
            None => ("200 OK", &body[..], String::new()),
        };
        let accept = if ranges.load(Ordering::SeqCst) {
            "Accept-Ranges: bytes\r\n"
        } else {
            ""
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nETag: {etag}\r\n{accept}{extra}\r\n",
            slice.len()
        );
        socket.write_all(head.as_bytes()).await?;
        // Send in 16 KiB pieces, throttled per connection.
        for chunk in slice.chunks(16 * 1024) {
            socket.write_all(chunk).await?;
            stats.served.fetch_add(chunk.len() as u64, Ordering::SeqCst);
            if options.throttle > 0 {
                tokio::time::sleep(Duration::from_secs_f64(
                    chunk.len() as f64 / options.throttle as f64,
                ))
                .await;
            }
        }
    }
}

pub fn hls_piece(i: u64) -> Vec<u8> {
    content(HLS_PIECE, 1000 + i)
}
