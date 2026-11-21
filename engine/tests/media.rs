// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Pages and formats against the real network: the helpers are fetched and
//! checked, a page's video is downloaded whole and as sound alone, and files
//! change format. Run on purpose, not in every CI job:
//!
//! ```text
//! EON_DM_MEDIA_URL=<a short public video page> cargo test --test media -- --ignored
//! ```
//!
//! The helpers are kept in `target/media-test-tools` between runs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use eon_dm_engine::media::{MediaSpec, Pick};
use eon_dm_engine::{
    convert, http, tools, AddRequest, Engine, Kind, Settings, Source, Start, Status,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/media-test-tools")
}

fn out_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("eon-dm-media-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 2×2, two-frame animated GIF.
const GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x02, 0x00, 0x02, 0x00, 0x80, 0x00, 0x00, 0xff, 0x00, 0x00,
    0x00, 0x00, 0xff, 0x21, 0xff, 0x0b, 0x4e, 0x45, 0x54, 0x53, 0x43, 0x41, 0x50, 0x45, 0x32, 0x2e,
    0x30, 0x03, 0x01, 0x00, 0x00, 0x00, 0x21, 0xf9, 0x04, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x2c, 0x00,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x02, 0x00, 0x00, 0x02, 0x02, 0x84, 0x51, 0x00, 0x21, 0xf9, 0x04,
    0x00, 0x0a, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x02, 0x00, 0x00, 0x02,
    0x02, 0x8c, 0x0b, 0x00, 0x3b,
];

#[test]
#[ignore = "network: fetches the converter"]
fn a_gif_becomes_a_picture_and_a_video() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = {
        let _guard = rt.enter();
        http::client().unwrap()
    };
    let ffmpeg = rt.block_on(tools::ffmpeg(&client, &data_dir())).unwrap();
    let out = out_dir("gif");
    for target in ["png", "mp4"] {
        let gif = out.join(format!("cat-{target}.gif"));
        std::fs::write(&gif, GIF).unwrap();
        let made = rt
            .block_on(convert::convert(&ffmpeg, &gif, target))
            .unwrap();
        assert_eq!(made.extension().unwrap(), target);
        assert!(std::fs::metadata(&made).unwrap().len() > 0);
        assert!(
            !gif.exists(),
            "the original goes once the new file is there"
        );
    }
    let _ = std::fs::remove_dir_all(&out);
}

fn wait_done(engine: &Engine, id: &str) -> PathBuf {
    let end = Instant::now() + Duration::from_secs(600);
    loop {
        let v = engine.view(id).unwrap();
        match v.status {
            Status::Done => return PathBuf::from(v.path),
            Status::Failed => panic!("failed: {:?}", v.error),
            _ => {}
        }
        assert!(
            Instant::now() < end,
            "timed out at {} of {:?}",
            v.done,
            v.length
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
#[ignore = "network: a real page, set EON_DM_MEDIA_URL"]
fn a_page_downloads_whole_and_as_sound_alone() {
    let Ok(url) = std::env::var("EON_DM_MEDIA_URL") else {
        eprintln!("EON_DM_MEDIA_URL is not set; nothing to do");
        return;
    };
    let engine = Engine::new(&data_dir(), Settings::default(), |_| {}).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let source = Source {
        url: url.clone(),
        ..Source::default()
    };
    let probe = rt.block_on(engine.probe(source.clone())).unwrap();
    let info = probe
        .media
        .clone()
        .expect("the page is recognised as playing something");
    assert!(info.has_video && info.has_audio, "{info:?}");
    assert!(!info.title.is_empty());
    let out = out_dir("page");
    let mut made = Vec::new();
    for (pick, container, ext) in [
        (Pick::Best, Some("mp4"), "mp4"),
        (Pick::AudioOnly, Some("mp3"), "mp3"),
    ] {
        let id = engine
            .add(AddRequest {
                source: source.clone(),
                probe: Some(probe.clone()),
                file_name: None,
                folder: out.clone(),
                start: Start::Now,
                variant: None,
                connections: None,
                media: Some(MediaSpec {
                    pick,
                    container: container.map(str::to_owned),
                }),
                convert: None,
            })
            .unwrap();
        assert_eq!(engine.view(&id).unwrap().kind, Kind::Media);
        let path = wait_done(&engine, &id);
        assert_eq!(path.extension().unwrap(), ext, "{path:?}");
        let v = engine.view(&id).unwrap();
        assert!(v.length.unwrap() > 10_000, "{v:?}");
        // The finished file's SHA-256 is computed after it is done, like any download.
        let end = Instant::now() + Duration::from_secs(30);
        while engine.view(&id).unwrap().sha256.is_none() {
            assert!(Instant::now() < end, "no SHA-256 for the finished file");
            std::thread::sleep(Duration::from_millis(100));
        }
        made.push(path);
        engine.remove(&id, false);
    }
    // A file that is already here, saved as sound.
    let ffmpeg = tools::ffmpeg_path(&data_dir()).unwrap();
    let copy = out.join("copy.mp4");
    std::fs::copy(&made[0], &copy).unwrap();
    let sound = rt
        .block_on(convert::convert(&ffmpeg, &copy, "m4a"))
        .unwrap();
    assert!(std::fs::metadata(&sound).unwrap().len() > 1000);
    drop(engine);
    let _ = std::fs::remove_dir_all(&out);
}

/// Serve a folder over HTTP on loopback, one thread per request.
fn serve(root: PathBuf) -> String {
    use std::io::{BufRead, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let root = root.clone();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    return;
                }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let mut parts = request.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let path = parts.next().unwrap_or("/").split('?').next().unwrap_or("/");
                let file = root.join(path.trim_start_matches('/'));
                let mut out = stream;
                match std::fs::read(&file) {
                    Ok(body) => {
                        let kind = if Path::new(path)
                            .extension()
                            .is_some_and(|e| e.eq_ignore_ascii_case("m3u8"))
                        {
                            "application/vnd.apple.mpegurl"
                        } else {
                            "application/octet-stream"
                        };
                        let _ = write!(
                            out,
                            "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        if method != "HEAD" {
                            let _ = out.write_all(&body);
                        }
                    }
                    Err(_) => {
                        let _ = out.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
            });
        }
    });
    base
}

/// The streams a file holds, as the converter reports them ("Video", "Audio").
fn streams(ffmpeg: &Path, file: &Path) -> Vec<String> {
    let out = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.trim_start().starts_with("Stream #"))
        .filter_map(|l| l.split_once(": ").map(|(_, rest)| rest.to_owned()))
        .collect()
}

#[test]
#[ignore = "network: fetches the converter"]
#[allow(clippy::too_many_lines)] // build a site, serve it, download it three ways
fn a_video_playlist_with_its_sound_apart_keeps_the_sound() {
    // The shape large video sites play: every version silent, the sound a
    // separate playlist. Built here with the converter, served on loopback.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = {
        let _guard = rt.enter();
        http::client().unwrap()
    };
    let ffmpeg = rt.block_on(tools::ffmpeg(&client, &data_dir())).unwrap();
    let site = out_dir("hls-site");
    for (folder, input) in [
        (
            "v",
            [
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=4",
                "-an",
                "-c:v",
                "libx264",
                "-g",
                "25",
            ]
            .as_slice(),
        ),
        (
            "a",
            [
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=4",
                "-vn",
                "-c:a",
                "aac",
            ]
            .as_slice(),
        ),
    ] {
        std::fs::create_dir_all(site.join(folder)).unwrap();
        // Run inside the folder: the muxer writes its init section beside
        // the working directory, not beside the playlist.
        let status = std::process::Command::new(&ffmpeg)
            .current_dir(site.join(folder))
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(input)
            .args([
                "-f",
                "hls",
                "-hls_time",
                "1",
                "-hls_playlist_type",
                "vod",
                "-hls_segment_type",
                "fmp4",
            ])
            .arg("index.m3u8")
            .status()
            .unwrap();
        assert!(status.success(), "building the {folder} playlist");
    }
    std::fs::write(
        site.join("master.m3u8"),
        "#EXTM3U\n\
         #EXT-X-MEDIA:URI=\"a/index.m3u8\",TYPE=AUDIO,GROUP-ID=\"snd\",NAME=\"Default\",DEFAULT=YES,AUTOSELECT=YES\n\
         #EXT-X-STREAM-INF:BANDWIDTH=500000,RESOLUTION=320x240,AUDIO=\"snd\"\n\
         v/index.m3u8\n",
    )
    .unwrap();
    let base = serve(site.clone());

    // The engine finds the converter where the test fetched it.
    let engine = Engine::new(&data_dir(), Settings::default(), |_| {}).unwrap();
    let source = Source {
        url: format!("{base}/master.m3u8"),
        ..Source::default()
    };
    let probe = rt.block_on(engine.probe(source.clone())).unwrap();
    let hls = probe.hls.clone().expect("a video playlist");
    assert!(hls.audio.is_some(), "its sound playlist is found: {hls:?}");

    let out = out_dir("hls-out");
    for (pick, name, want) in [
        (None, "talk.mp4", vec!["Video", "Audio"]),
        (Some(Pick::VideoOnly), "silent.mp4", vec!["Video"]),
        (Some(Pick::AudioOnly), "sound.m4a", vec!["Audio"]),
    ] {
        let id = engine
            .add(AddRequest {
                source: source.clone(),
                probe: Some(probe.clone()),
                file_name: Some(name.into()),
                folder: out.clone(),
                start: Start::Now,
                variant: None,
                connections: None,
                media: pick.map(|pick| MediaSpec {
                    pick,
                    container: None,
                }),
                convert: None,
            })
            .unwrap();
        assert_eq!(engine.view(&id).unwrap().kind, Kind::Hls);
        let path = wait_done(&engine, &id);
        let kinds = streams(&ffmpeg, &path);
        let got: Vec<&str> = kinds
            .iter()
            .map(|s| s.split(':').next().unwrap_or_default())
            .collect();
        assert_eq!(got, want, "{name}: {kinds:?}");
        let leftovers: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                n.contains(".eon-")
                    || Path::new(n)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("part"))
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "nothing half-made is left: {leftovers:?}"
        );
    }
    drop(engine);
    let _ = std::fs::remove_dir_all(&site);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
#[ignore = "network: a real page, set EON_DM_MEDIA_URL"]
fn a_name_with_turkish_letters_comes_back_whole_with_aac_sound() {
    // The extractor once wrote the finished file's name in the console's code
    // page, and a name with "ğ" in it failed a finished download.
    let Ok(url) = std::env::var("EON_DM_MEDIA_URL") else {
        eprintln!("EON_DM_MEDIA_URL is not set; nothing to do");
        return;
    };
    let engine = Engine::new(&data_dir(), Settings::default(), |_| {}).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let source = Source {
        url,
        ..Source::default()
    };
    let probe = rt.block_on(engine.probe(source.clone())).unwrap();
    let out = out_dir("turkish");
    let id = engine
        .add(AddRequest {
            source,
            probe: Some(probe),
            file_name: Some("Buzdağı şarkısı ığüşöç.mp4".into()),
            folder: out.clone(),
            start: Start::Now,
            variant: None,
            connections: None,
            media: Some(MediaSpec {
                pick: Pick::Height(240),
                container: Some("mp4".into()),
            }),
            convert: None,
        })
        .unwrap();
    let path = wait_done(&engine, &id);
    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        "Buzdağı şarkısı ığüşöç.mp4"
    );
    let kinds = streams(&tools::ffmpeg_path(&data_dir()).unwrap(), &path);
    assert!(
        kinds.iter().any(|s| s.starts_with("Audio: aac")),
        "MP4 gets AAC sound: {kinds:?}"
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(&out);
}
