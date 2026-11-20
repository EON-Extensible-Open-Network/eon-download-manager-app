// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The HTTP side: one shared client, requests that carry what the browser
//! would have sent, and the probe that learns everything before downloading.

use crate::state::Validators;
use crate::{name, Error};
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::sync::Once;
use std::time::Duration;

pub const USER_AGENT: &str = concat!("EON Download Manager/", env!("CARGO_PKG_VERSION"));

/// Where a download comes from, and what to send with it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referrer: Option<String>,
    /// The `Cookie` header the browser would send to `url`. Sent to that host
    /// only; reqwest drops it on a redirect to another host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

static PROVIDER: Once = Once::new();

/// The one client every download shares: its keep-alive pool is what lets a
/// connection that finished a segment carry the next request to that server.
pub fn client() -> Result<reqwest::Client, Error> {
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(40))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| Error::Network(e.to_string()))
}

/// A GET for `url` carrying the source's referrer, cookies and user agent,
/// asking for the bytes as they are (no compression: ranges are byte offsets
/// of the file, not of a compressed stream).
pub fn get(client: &reqwest::Client, source: &Source, url: &str) -> reqwest::RequestBuilder {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    if let Some(v) = source
        .referrer
        .as_deref()
        .and_then(|r| HeaderValue::from_str(r).ok())
    {
        headers.insert(header::REFERER, v);
    }
    if let Some(v) = source.cookies.as_deref().filter(|c| !c.is_empty()) {
        if same_host(&source.url, url) {
            if let Ok(v) = HeaderValue::from_str(v) {
                headers.insert(header::COOKIE, v);
            }
        }
    }
    if let Some(v) = source
        .user_agent
        .as_deref()
        .and_then(|u| HeaderValue::from_str(u).ok())
    {
        headers.insert(header::USER_AGENT, v);
    }
    client.get(url).headers(headers)
}

fn same_host(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.host_str() == b.host_str(),
        _ => false,
    }
}

/// Everything known about a file before its first byte is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub final_url: String,
    pub length: Option<u64>,
    /// The server honours byte ranges: several connections, and resume.
    pub resumable: bool,
    pub file_name: String,
    pub mime: Option<String>,
    pub validators: Validators,
    /// An HLS playlist rather than a file.
    pub hls: bool,
}

pub fn status_error(status: reqwest::StatusCode) -> Error {
    match status.as_u16() {
        429 | 503 => Error::Busy,
        code => Error::Http(code),
    }
}

/// Ask for the first byte only. A `206` with `Content-Range` tells the length
/// and that ranges work; a `200` means the server ignores ranges. `HEAD` is
/// not used: many servers answer it differently from `GET`, or not at all.
pub async fn probe(client: &reqwest::Client, source: &Source) -> Result<Probe, Error> {
    let url = url::Url::parse(&source.url).map_err(|_| Error::Invalid("not an address".into()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Invalid("only http and https addresses".into()));
    }
    let mut response = get(client, source, url.as_str())
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(network)?;
    if response.status().as_u16() == 416 {
        // An empty file has no byte 0. Ask for it whole.
        response = get(client, source, url.as_str())
            .send()
            .await
            .map_err(network)?;
    }
    let status = response.status();
    if !status.is_success() {
        return Err(status_error(status));
    }
    let headers = response.headers();
    let text = |h: header::HeaderName| {
        headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let final_url = response.url().clone();
    let (length, resumable) = if status.as_u16() == 206 {
        let total = text(header::CONTENT_RANGE).and_then(|r| {
            r.rsplit('/')
                .next()
                .and_then(|t| t.trim().parse::<u64>().ok())
        });
        (total, total.is_some())
    } else {
        (response.content_length(), false)
    };
    let mime = text(header::CONTENT_TYPE);
    let disposition = text(header::CONTENT_DISPOSITION).and_then(|d| name::from_disposition(&d));
    let hls = mime
        .as_deref()
        .is_some_and(|m| m.to_ascii_lowercase().contains("mpegurl"))
        || final_url.path().to_ascii_lowercase().ends_with(".m3u8");
    let mut file_name = disposition
        .or_else(|| name::from_url(&final_url))
        .or_else(|| name::from_url(&url))
        .unwrap_or_else(|| "download".to_owned());
    if std::path::Path::new(&file_name).extension().is_none() {
        if let Some(ext) = mime.as_deref().and_then(name::extension_for) {
            file_name = format!("{file_name}.{ext}");
        }
    }
    if hls {
        let stem = file_name.trim_end_matches(".m3u8").to_owned();
        file_name = format!("{stem}.ts");
    }
    let validators = Validators {
        etag: text(header::ETAG),
        last_modified: text(header::LAST_MODIFIED),
    };
    // Dropping the response here closes a 200 that would otherwise send the
    // whole file.
    drop(response);
    Ok(Probe {
        final_url: final_url.to_string(),
        length,
        resumable,
        file_name,
        mime,
        validators,
        hls,
    })
}

#[allow(clippy::needless_pass_by_value)] // the shape `map_err` wants
pub fn network(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        Error::Network("the server stopped answering".into())
    } else if e.is_connect() {
        Error::Network("could not connect".into())
    } else if let Some(status) = e.status() {
        status_error(status)
    } else {
        Error::Network(e.to_string())
    }
}
