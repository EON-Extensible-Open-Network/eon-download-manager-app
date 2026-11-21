// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! HLS: the `.m3u8` playlists most web video is delivered as (RFC 8216).
//!
//! A master playlist lists versions (resolution, bitrate); a media playlist
//! lists the pieces. A version may carry no sound of its own and name a
//! separate sound playlist instead (`#EXT-X-MEDIA:TYPE=AUDIO`): then both are
//! downloaded and joined. Pieces download in parallel into a folder, AES-128 is
//! decrypted, and the pieces are joined in order at the end. Live streams and
//! anything with DRM (`SAMPLE-AES`, a key format other than plain AES) are
//! refused with a reason — EON downloads what the page plays in the open.

use crate::http::{self, Source};
use crate::limiter::Limiter;
use crate::plan::Segment;
use crate::state::{self, HlsProgress, ResumeState};
use crate::transfer::{Live, Outcome};
use crate::Error;
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinSet;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    pub resolution: Option<String>,
    /// The separate sound playlist this version plays with, if it has one.
    #[serde(default)]
    pub audio: Option<String>,
}

impl Variant {
    /// "1080p", or the bitrate when the playlist gives no resolution.
    pub fn label(&self) -> String {
        if let Some(h) = self.resolution.as_deref().and_then(|r| r.split('x').nth(1)) {
            return format!("{h}p");
        }
        format!("{} kbit/s", self.bandwidth / 1000)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub uri: String,
    pub iv: Option<[u8; 16]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub url: String,
    pub duration: f64,
    pub key: Option<Key>,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Media {
    pub pieces: Vec<Piece>,
    /// fMP4: the initialisation section every piece needs in front of it.
    pub map: Option<String>,
    pub ended: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Master(Vec<Variant>),
    Media(Media),
}

fn attributes(text: &str) -> HashMap<String, String> {
    let (mut out, mut key, mut value, mut in_value, mut quoted) =
        (HashMap::new(), String::new(), String::new(), false, false);
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            '=' if !in_value && !quoted => in_value = true,
            ',' if !quoted => {
                out.insert(
                    std::mem::take(&mut key).trim().to_ascii_uppercase(),
                    std::mem::take(&mut value),
                );
                in_value = false;
            }
            c if in_value => value.push(c),
            c => key.push(c),
        }
    }
    if !key.is_empty() {
        out.insert(key.trim().to_ascii_uppercase(), value);
    }
    out
}

fn iv(text: &str) -> Option<[u8; 16]> {
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))?;
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A separate sound playlist (`#EXT-X-MEDIA:TYPE=AUDIO,URI=...`): its group,
/// how much the playlist prefers it (0 = the default), and its address.
fn sound_playlist(text: &str) -> Option<(String, u8, String)> {
    let a = attributes(text);
    if a.get("TYPE").map(String::as_str) != Some("AUDIO") {
        return None;
    }
    let yes = |k: &str| a.get(k).is_some_and(|v| v == "YES");
    let rank = if yes("DEFAULT") {
        0
    } else if yes("AUTOSELECT") {
        1
    } else {
        2
    };
    Some((a.get("GROUP-ID")?.clone(), rank, a.get("URI")?.clone()))
}

/// Read a playlist. Relative addresses resolve against `base`.
#[allow(clippy::too_many_lines)] // one pass over the lines, one branch per tag
pub fn parse(text: &str, base: &Url) -> Result<Playlist, Error> {
    let resolve = |u: &str| {
        base.join(u.trim())
            .map(|u| u.to_string())
            .map_err(|_| Error::Invalid(format!("bad address in playlist: {u}")))
    };
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") {
        return Err(Error::Invalid("not an HLS playlist".into()));
    }
    let mut variants = Vec::new();
    let mut pending_variant: Option<(u64, Option<String>, Option<String>)> = None;
    // Sound playlists by group: (preferred, address).
    let mut audio_groups: HashMap<String, Vec<(u8, String)>> = HashMap::new();
    let mut variant_groups: Vec<Option<String>> = Vec::new();
    let mut pieces = Vec::new();
    let mut duration = None;
    let mut key: Option<Key> = None;
    let mut map = None;
    let mut ended = false;
    let mut sequence = 0u64;
    for line in lines {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let a = attributes(rest);
            let bandwidth = a.get("BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0);
            pending_variant = Some((
                bandwidth,
                a.get("RESOLUTION").cloned(),
                a.get("AUDIO").cloned(),
            ));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            if let Some((group, rank, uri)) = sound_playlist(rest) {
                audio_groups
                    .entry(group)
                    .or_default()
                    .push((rank, resolve(&uri)?));
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            duration = rest
                .split(',')
                .next()
                .and_then(|d| d.trim().parse::<f64>().ok());
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let a = attributes(rest);
            match a.get("METHOD").map(String::as_str) {
                Some("NONE") => key = None,
                Some("AES-128") => {
                    let format = a.get("KEYFORMAT").map_or("identity", String::as_str);
                    if format != "identity" {
                        return Err(Error::Unsupported("this video is protected (DRM)".into()));
                    }
                    let uri = a
                        .get("URI")
                        .ok_or_else(|| Error::Invalid("a key without an address".into()))?;
                    key = Some(Key {
                        uri: resolve(uri)?,
                        iv: a.get("IV").and_then(|v| iv(v)),
                    });
                }
                _ => return Err(Error::Unsupported("this video is protected (DRM)".into())),
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let a = attributes(rest);
            if a.contains_key("BYTERANGE") {
                return Err(Error::Unsupported(
                    "byte-range playlists are not supported yet".into(),
                ));
            }
            map = a.get("URI").map(|u| resolve(u)).transpose()?;
        } else if line.starts_with("#EXT-X-BYTERANGE") {
            return Err(Error::Unsupported(
                "byte-range playlists are not supported yet".into(),
            ));
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if line.starts_with('#') {
            // Other tags do not change what is downloaded.
        } else if let Some((bandwidth, resolution, group)) = pending_variant.take() {
            variants.push(Variant {
                url: resolve(line)?,
                bandwidth,
                resolution,
                audio: None,
            });
            variant_groups.push(group);
        } else {
            pieces.push(Piece {
                url: resolve(line)?,
                duration: duration.take().unwrap_or(0.0),
                key: key.clone(),
                sequence,
            });
            sequence += 1;
        }
    }
    if !variants.is_empty() {
        for (v, group) in variants.iter_mut().zip(variant_groups) {
            v.audio = group
                .and_then(|g| audio_groups.get(&g))
                .and_then(|list| list.iter().min_by_key(|(rank, _)| *rank))
                .map(|(_, uri)| uri.clone());
        }
        variants.sort_by_key(|v| std::cmp::Reverse(v.bandwidth));
        return Ok(Playlist::Master(variants));
    }
    if pieces.is_empty() {
        return Err(Error::Invalid(
            "the playlist lists nothing to download".into(),
        ));
    }
    Ok(Playlist::Media(Media { pieces, map, ended }))
}

async fn text(client: &reqwest::Client, source: &Source, url: &str) -> Result<String, Error> {
    let response = http::get(client, source, url)
        .send()
        .await
        .map_err(http::network)?;
    if !response.status().is_success() {
        return Err(http::status_error(response.status()));
    }
    response.text().await.map_err(http::network)
}

async fn bytes(client: &reqwest::Client, source: &Source, url: &str) -> Result<Vec<u8>, Error> {
    let response = http::get(client, source, url)
        .send()
        .await
        .map_err(http::network)?;
    if !response.status().is_success() {
        return Err(http::status_error(response.status()));
    }
    Ok(response.bytes().await.map_err(http::network)?.to_vec())
}

/// What the add sheet shows for a playlist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inspection {
    /// Versions to choose from, best first; empty for a media playlist.
    pub variants: Vec<Variant>,
    /// The media playlist that will be downloaded (the best version by default).
    pub media_url: String,
    pub pieces: usize,
    pub seconds: f64,
    /// fMP4 pieces: the result is `.mp4`, not `.ts`.
    pub mp4: bool,
    /// The best version's separate sound playlist, when its sound is apart.
    #[serde(default)]
    pub audio: Option<String>,
}

pub async fn inspect(
    client: &reqwest::Client,
    source: &Source,
    url: &str,
) -> Result<Inspection, Error> {
    let base = Url::parse(url).map_err(|_| Error::Invalid("not an address".into()))?;
    match parse(&text(client, source, url).await?, &base)? {
        Playlist::Master(variants) => {
            let best = variants
                .first()
                .ok_or_else(|| Error::Invalid("no versions".into()))?
                .url
                .clone();
            let mut inner = Box::pin(inspect(client, source, &best)).await?;
            inner.audio = variants.first().and_then(|v| v.audio.clone());
            inner.variants = variants;
            Ok(inner)
        }
        Playlist::Media(media) => {
            if !media.ended {
                return Err(Error::Unsupported(
                    "live streams cannot be downloaded yet".into(),
                ));
            }
            Ok(Inspection {
                variants: Vec::new(),
                media_url: url.to_owned(),
                pieces: media.pieces.len(),
                seconds: media.pieces.iter().map(|p| p.duration).sum(),
                mp4: media.map.is_some(),
                audio: None,
            })
        }
    }
}

pub struct HlsJob {
    pub client: reqwest::Client,
    pub source: Source,
    /// The media playlist.
    pub playlist: String,
    pub target: PathBuf,
    /// Read live, as in `transfer::Job`.
    pub connections: Arc<std::sync::atomic::AtomicU32>,
    pub global: Arc<Limiter>,
    pub own: Arc<Limiter>,
}

fn decrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>, Error> {
    cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| Error::Invalid("a piece did not decrypt".into()))
}

fn sequence_iv(sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}

fn piece_file(dir: &std::path::Path, i: usize) -> PathBuf {
    dir.join(format!("{i:06}.seg"))
}

/// Download every piece, then join them into the target.
#[allow(clippy::too_many_lines)] // fetch, retry, save, join: one flow, read best whole
pub async fn run(
    job: HlsJob,
    live: Arc<Live>,
    mut stop: watch::Receiver<bool>,
) -> Result<Outcome, Error> {
    let base = Url::parse(&job.playlist).map_err(|_| Error::Invalid("not an address".into()))?;
    let media = match parse(&text(&job.client, &job.source, &job.playlist).await?, &base)? {
        Playlist::Media(m) => m,
        Playlist::Master(_) => return Err(Error::Invalid("choose a version first".into())),
    };
    if !media.ended {
        return Err(Error::Unsupported(
            "live streams cannot be downloaded yet".into(),
        ));
    }
    let dir = state::pieces_dir(&job.target);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| Error::Io(e.to_string()))?;
    let state_file = state::state_path(&job.target);
    let mut done: BTreeSet<usize> = ResumeState::load(&state_file)
        .and_then(|s| s.hls)
        .filter(|h| h.playlist == job.playlist)
        .map(|h| h.done.into_iter().map(|i| i as usize).collect())
        .unwrap_or_default();
    done.retain(|&i| piece_file(&dir, i).exists());
    let total = media.pieces.len();
    let show = |done: &BTreeSet<usize>, live: &Live| {
        *live.segments() = (0..total as u64)
            .map(|i| {
                let d = done.contains(&(i as usize));
                Segment {
                    id: i as u32,
                    start: i,
                    pos: if d { i + 1 } else { i },
                    end: i + 1,
                    owner: None,
                }
            })
            .collect();
    };
    show(&done, &live);
    let save = |done: &BTreeSet<usize>| {
        let state = ResumeState {
            eondl: 0,
            url: job.source.url.clone(),
            final_url: None,
            referrer: job.source.referrer.clone(),
            cookies: job.source.cookies.clone(),
            user_agent: job.source.user_agent.clone(),
            file: job
                .target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            length: None,
            resumable: true,
            validators: None,
            segments: Vec::new(),
            hls: Some(HlsProgress {
                playlist: job.playlist.clone(),
                done: done.iter().map(|&i| i as u32).collect(),
            }),
        };
        let _ = state.save(&state_file);
    };
    save(&done);

    let mut keys: HashMap<String, [u8; 16]> = HashMap::new();
    let mut todo: Vec<usize> = (0..total).filter(|i| !done.contains(i)).rev().collect();
    let mut attempts: HashMap<usize, u32> = HashMap::new();
    let mut workers: JoinSet<(usize, Result<(), Error>)> = JoinSet::new();
    let mut last_save = Instant::now();
    loop {
        let limit = job.connections.load(Ordering::Relaxed).clamp(1, 16) as usize;
        while workers.len() < limit {
            let Some(i) = todo.pop() else { break };
            let piece = media.pieces[i].clone();
            let key = match &piece.key {
                Some(k) => {
                    if !keys.contains_key(&k.uri) {
                        let raw = bytes(&job.client, &job.source, &k.uri).await?;
                        let raw: [u8; 16] = raw
                            .try_into()
                            .map_err(|_| Error::Invalid("a key is not 16 bytes".into()))?;
                        keys.insert(k.uri.clone(), raw);
                    }
                    keys.get(&k.uri)
                        .map(|raw| (*raw, k.iv.unwrap_or_else(|| sequence_iv(piece.sequence))))
                }
                None => None,
            };
            let (client, source, path) =
                (job.client.clone(), job.source.clone(), piece_file(&dir, i));
            let (global, own, live) = (
                Arc::clone(&job.global),
                Arc::clone(&job.own),
                Arc::clone(&live),
            );
            workers.spawn(async move {
                let result = async {
                    let data = bytes(&client, &source, &piece.url).await?;
                    let n = data.len() as u64;
                    live.received.fetch_add(n, Ordering::Relaxed);
                    global.take(n).await;
                    own.take(n).await;
                    let data = match key {
                        Some((k, iv)) => decrypt(&data, &k, &iv)?,
                        None => data,
                    };
                    let tmp = path.with_extension("tmp");
                    tokio::fs::write(&tmp, &data)
                        .await
                        .map_err(|e| Error::Io(e.to_string()))?;
                    tokio::fs::rename(&tmp, &path)
                        .await
                        .map_err(|e| Error::Io(e.to_string()))
                }
                .await;
                (i, result)
            });
        }
        live.connections
            .store(workers.len() as u32, Ordering::Relaxed);
        if workers.is_empty() {
            break;
        }
        tokio::select! {
            Some(joined) = workers.join_next() => {
                let (i, result) = joined.map_err(|e| Error::Io(e.to_string()))?;
                match result {
                    Ok(()) => { done.insert(i); }
                    Err(e) => {
                        let n = attempts.entry(i).or_insert(0);
                        *n += 1;
                        if *n > 5 {
                            workers.abort_all();
                            save(&done);
                            return Err(e);
                        }
                        tokio::time::sleep(Duration::from_millis(500 * u64::from(*n))).await;
                        todo.push(i);
                    }
                }
                show(&done, &live);
            }
            _ = stop.changed() => {
                workers.abort_all();
                while workers.join_next().await.is_some() {}
                live.connections.store(0, Ordering::Relaxed);
                save(&done);
                return Ok(Outcome::Stopped);
            }
        }
        if last_save.elapsed() >= Duration::from_secs(1) {
            save(&done);
            last_save = Instant::now();
        }
    }
    live.connections.store(0, Ordering::Relaxed);
    save(&done);

    // Join: the initialisation section (fMP4), then every piece in order.
    let init = match &media.map {
        Some(u) => Some(bytes(&job.client, &job.source, u).await?),
        None => None,
    };
    let part = state::part_path(&job.target);
    let target = job.target.clone();
    let joined = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::fs::File::create(&part)?);
        if let Some(init) = init {
            out.write_all(&init)?;
        }
        for i in 0..total {
            out.write_all(&std::fs::read(piece_file(&dir, i))?)?;
        }
        out.into_inner()
            .map_err(std::io::Error::other)?
            .sync_all()?;
        let dest = if target.exists() {
            let folder = target.parent().map(PathBuf::from).unwrap_or_default();
            crate::name::unique(
                &folder,
                &target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        } else {
            target.clone()
        };
        std::fs::rename(&part, &dest)?;
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(state::state_path(&target));
        Ok(dest)
    })
    .await
    .map_err(|e| Error::Io(e.to_string()))?
    .map_err(|e| Error::Io(e.to_string()))?;
    Ok(Outcome::Done(joined))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example.org/v/master.m3u8").unwrap()
    }

    #[test]
    fn a_master_playlist_lists_versions_best_first() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"\n360p/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nhttps://other.example.org/1080.m3u8\n";
        let Playlist::Master(v) = parse(text, &base()).unwrap() else {
            panic!("not a master")
        };
        assert_eq!(v[0].label(), "1080p");
        assert_eq!(v[1].url, "https://cdn.example.org/v/360p/index.m3u8");
    }

    #[test]
    fn a_version_whose_sound_is_apart_names_its_sound_playlist() {
        // The shape large video sites use: every version silent, sound in groups.
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA:URI=\"a/low.m3u8\",TYPE=AUDIO,GROUP-ID=\"233\",NAME=\"Default\",AUTOSELECT=YES\n\
            #EXT-X-MEDIA:URI=\"a/high.m3u8\",TYPE=AUDIO,GROUP-ID=\"234\",NAME=\"Default\",DEFAULT=YES,AUTOSELECT=YES\n\
            #EXT-X-MEDIA:URI=\"a/other.m3u8\",TYPE=AUDIO,GROUP-ID=\"234\",NAME=\"Other\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=300000,RESOLUTION=640x360,AUDIO=\"233\"\nv/360.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=3840x2160,AUDIO=\"234\"\nv/2160.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=100000,RESOLUTION=256x144\nv/144.m3u8\n";
        let Playlist::Master(v) = parse(text, &base()).unwrap() else {
            panic!("not a master")
        };
        assert_eq!(v[0].label(), "2160p");
        assert_eq!(
            v[0].audio.as_deref(),
            Some("https://cdn.example.org/v/a/high.m3u8"),
            "the default of its group"
        );
        assert_eq!(
            v[1].audio.as_deref(),
            Some("https://cdn.example.org/v/a/low.m3u8")
        );
        assert_eq!(v[2].audio, None, "sound inside the version itself");
    }

    #[test]
    fn a_media_playlist_with_keys_and_sequence_numbers() {
        let text = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\na.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x000102030405060708090A0B0C0D0E0F\n#EXTINF:3.5,\nb.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:2,\nc.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media(m) = parse(text, &base()).unwrap() else {
            panic!("not media")
        };
        assert!(m.ended);
        assert_eq!(m.pieces.len(), 3);
        assert_eq!(m.pieces[0].sequence, 7);
        assert_eq!(
            m.pieces[0].key.as_ref().unwrap().uri,
            "https://cdn.example.org/v/key.bin"
        );
        assert_eq!(m.pieces[1].key.as_ref().unwrap().iv.unwrap()[15], 0x0f);
        assert!(m.pieces[2].key.is_none());
        assert_eq!(sequence_iv(7)[15], 7);
    }

    #[test]
    fn protected_and_unsupported_streams_are_refused_by_name() {
        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:4,\na.ts\n";
        assert!(matches!(parse(drm, &base()), Err(Error::Unsupported(m)) if m.contains("DRM")));
        let widevine = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"x\",KEYFORMAT=\"urn:uuid:edef8ba9\"\n#EXTINF:4,\na.ts\n";
        assert!(matches!(
            parse(widevine, &base()),
            Err(Error::Unsupported(_))
        ));
        assert!(parse("<html>", &base()).is_err());
    }

    #[test]
    fn aes_128_round_trip() {
        use aes::cipher::BlockEncryptMut;
        let (key, iv) = ([7u8; 16], sequence_iv(3));
        let plain = b"forty-two bytes of transport stream data..".to_vec();
        let enc = cbc::Encryptor::<aes::Aes128>::new((&key).into(), (&iv).into())
            .encrypt_padded_vec_mut::<Pkcs7>(&plain);
        assert_eq!(decrypt(&enc, &key, &iv).unwrap(), plain);
    }
}
