<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# Security

Report privately through GitHub's **Security → Report a vulnerability** on this
repository; the organisation-wide policy is
[`.github/SECURITY.md`](https://github.com/EON-Extensible-Open-Network/.github/blob/main/SECURITY.md).

## What a download manager has to get right

- **Names from servers and pages are untrusted.** They never carry a directory, a
  reserved Windows device name, a leading dot or a trailing space, and a download never
  replaces an existing file (`engine/src/name.rs`, tested).
- **Cookies go where they came from.** The extension sends only the cookies of the
  address being downloaded; the engine sends them only to that host and reqwest drops
  them on a redirect elsewhere. They are deleted from the resume state when the download
  finishes.
- **The browser hand-off.** The executable doubles as a native messaging host, reachable
  only from the extension ids listed in its manifest. It is registered per user at every
  start (no administrator rights); *Disconnect* removes it and keeps it removed. It
  reads one message of at most 8 MiB, accepts only `http(s)` addresses, writes the message
  to the data folder's `inbox/`, and wakes the window. `--handoff` reads only files in
  that inbox, so a command line cannot make the program read or delete another file. The
  window it starts does not inherit the browser's pipes.
- **The interface has no network and no plugin.** It reaches the host only through the
  commands in `src-tauri/src/main.rs`; folder picking, opening files, notifications and the
  clipboard are done in Rust. CI fails the build on `fetch`/`XMLHttpRequest` in `src/`, on
  markup built from strings, and on any plugin permission granted to the page.
- **Video.** Only what the page itself loads; AES-128 is the only decryption, and DRM key
  systems are refused by name. Playlists are parsed, never executed.
- **No telemetry, no licensing code** — CI checks for both.

## Known

- Builds are not signed (no keys yet; plan madde 30, 34). Check `SHA256SUMS.txt`.
- The Linux build inherits Tauri's GTK 3 stack and its `glib` advisory, as described in
  eon-office-launcher's `SECURITY.md`; `deny.toml` records the unmaintained-crate notices
  that reach Linux only.
- The extension asks for `<all_urls>`: it needs the cookies and the responses of whatever
  site a download or a video comes from. It reads response *headers* to notice media and
  never page content, except the links you ask it to collect.
