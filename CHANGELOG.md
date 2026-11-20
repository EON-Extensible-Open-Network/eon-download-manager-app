<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# Changelog

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
