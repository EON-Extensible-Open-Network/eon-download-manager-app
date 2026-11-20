// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The EON Download Manager engine.
//!
//! [`Engine`] keeps the list of downloads, runs the queue and hands each file
//! to [`transfer`] (many connections, dynamic segmentation, resume) or [`hls`]
//! (web video). No GUI dependency: the window in `src-tauri` and any other
//! front end drive it through [`Engine`]'s methods and [`View`]s.

pub mod batch;
pub mod category;
pub mod convert;
pub mod hls;
pub mod http;
pub mod limiter;
pub mod manager;
pub mod media;
pub mod name;
pub mod plan;
pub mod state;
pub mod tools;
pub mod transfer;

pub use category::Category;
pub use http::Source;
pub use manager::{
    AddRequest, Engine, Event, Kind, Lane, ProbeView, Settings, Start, Status, View,
};

use std::fmt;

/// What went wrong, in a form the interface can name in its own language
/// ([`Error::code`]) and explain ([`fmt::Display`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The server answered with this status.
    Http(u16),
    /// `429`/`503`: the server is busy or limits connections.
    Busy,
    Network(String),
    Io(String),
    /// The file on the server is not the one the download started with.
    Changed,
    Invalid(String),
    Unsupported(String),
    NotFound,
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::Http(401 | 403) => "refused",
            Error::Http(404 | 410) => "gone",
            Error::Http(_) => "http",
            Error::Busy => "busy",
            Error::Network(_) => "network",
            Error::Io(_) => "disk",
            Error::Changed => "changed",
            Error::Invalid(_) => "invalid",
            Error::Unsupported(_) => "unsupported",
            Error::NotFound => "not_found",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(code @ (401 | 403)) => {
                write!(f, "the server refused ({code}); the link may have expired")
            }
            Error::Http(code @ (404 | 410)) => write!(f, "the file is no longer there ({code})"),
            Error::Http(code) => write!(f, "the server answered {code}"),
            Error::Busy => f.write_str("the server is busy or limits connections"),
            Error::Network(m) | Error::Invalid(m) | Error::Unsupported(m) => f.write_str(m),
            Error::Io(m) => write!(f, "could not write the file: {m}"),
            Error::Changed => f.write_str("the file changed on the server"),
            Error::NotFound => f.write_str("no such download"),
        }
    }
}

impl std::error::Error for Error {}

/// SHA-256 of a file, lower-case hex.
pub fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        }))
}
