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
