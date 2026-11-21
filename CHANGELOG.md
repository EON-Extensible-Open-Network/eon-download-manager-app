<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# Changelog

## v0.0.1 — 21 November 2026

**Downloads from pages that play video or sound.** This covers YouTube, Instagram, X
(Twitter), TikTok and many others: paste the page's address, choose a quality, sound only
or picture only, and it downloads like any other file.

Tested against YouTube for real:
- the whole video as MP4, and the sound alone as MP3, with the finished file's SHA-256;
- 1080p on offer, up to 2160p.

An Instagram reel was also recognised, with its qualities. Pages that need an account, or
that a site blocks for your address, say so instead of failing quietly.


- **Settings apply to running downloads.** Connections per download, downloads at the same
  time and the speed limit change the transfer in place: fewer connections means the
  newest finish what they hold and step back; fewer downloads at once sends the newest back
  to the queue. Nothing downloaded is lost. Each download can also have its own number of
  connections.
- **Catching browser downloads sets itself up.** The native host is registered at every
  start; the extension ships inside the program, and a first-start walkthrough opens the
  browser's extensions page and the folder, then shows the extension as connected the
  moment it says hello.
- The licence exception now names this product's own interface as its boundary (it named
  EON Office's, copied by mistake).
- **Formats.** *Settings → Format* keeps the original or asks each time; asked, the add
  sheet offers what the file can become: sound only (MP3, M4A), picture without sound,
  MP4 or MKV, a GIF as a still picture or a video. The converter is fetched when first
  needed and checked against its published SHA-256.
- **Pages that play video or sound** can be added like a file: the add sheet shows the
  qualities on offer, and the download pauses, resumes and shows its progress like any
  other.
- Tests run under their own application identity and never touch the person's registry.

## v0.0.0 — first alpha

- **Engine:** dynamic segmentation (IDM's rule: a free connection halves the largest
  remaining part), up to 32 connections ramped one every 150 ms, keep-alive reuse, servers
  that limit connections learnt from 429/503; resume from an atomic `.eondl` state written
  every second, `If-Range` against changed files; single-connection fallback for servers
  without ranges; *give it a new link* keeping every byte; a global and per-download speed
  limit; SHA-256 on completion; HLS with master playlists, AES-128 and fMP4; a queue that
  survives restarts.
- **Window:** the lane map, the add sheet, numbered series, refresh, compare checksum,
  queue and scheduler with sleep or shut down after a cancellable countdown, clipboard
  watching, tray, notifications, keyboard, English and Turkish, dark and light.
- **Browser:** the native messaging host in the same executable, registered on request;
  an MV3 extension for Chromium and Firefox — takeover by type and size, Alt to skip,
  context menu, *Download video* over the player.
- **Checks:** 32 engine tests against a local server (including a crash mid-download, a
  changed file and an expired link), `--selftest` with a real download, 26 window checks.
