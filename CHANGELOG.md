<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# Changelog

## v0.0.3 — 22 November 2026

**Video caught on a page comes with its sound.** Large video sites send the picture and
the sound as two separate playlists. The *Download video* button downloaded only the
picture, so the file was silent. Now both are downloaded and joined into one file without
re-encoding. Each half resumes on its own.

**With sound or without is always asked.** For a video the add sheet always offers
picture and sound, picture only, and sound only (M4A, or MP3 for a page). Before, the
last two appeared only with *Ask for the format* turned on, and never for a caught video.
The first choice now says "picture and sound".

**A caught video goes through its page when the page is known.** If the page that played
it is one the extractor knows, the page is used, with its title, every quality and the
sound. Otherwise the caught stream is used and named after the tab's title, not `index`.

**Names with Turkish letters no longer fail a finished download.** The extractor reported
the finished file's name in the console's code page. A name with "ğ" or "ş" could not be
read, the download that had in fact finished was marked failed, and the reason was shown
as a full disk. The extractor now writes UTF-8. If the name it reports is not on disk, the
file is found by the name chosen in the sheet. Downloads marked failed this way finish
when retried: the file is already complete.

**MP4 gets AAC sound.** A video saved as MP4 or as M4A sound now takes the AAC sound
track where the page offers one. The system players of Windows play AAC, and they play
many Opus tracks in MP4 as silence.

Checked against the real network: a 19-second video downloaded under a name with
Turkish letters, with AAC sound. A short-video page with a Turkish title downloaded whole
and as MP3. A video playlist with its sound apart, built with FFmpeg and served locally,
downloaded as picture and sound, picture only and sound only, with nothing half-made left
over. The 30 window checks pass.

## v0.0.2 — 22 November 2026

**Firefox hands downloads over.** In Firefox the extension caught a download, cancelled
it in the browser and passed it on, and the window never received it: the download was
lost. The host passed the hand-off to a second copy of the program, which was to give
it to the window, and was told *ok* before that had happened. Under Firefox, which runs
the host in a Windows job object, that second copy never delivered it. The exact way it
failed was not isolated; the route is gone, so it no longer matters. The hand-off had
only ever been tested end to end through a Chromium browser. Now:

- the window reads the hand-offs waiting for it itself, a few times a second, and holds a
  lock while it runs so the host knows it is there;
- the host answers *ok* only once the window has taken the hand-off. If it does not
  within 15 seconds, the host withdraws it and answers *unavailable*, and the extension
  leaves the download to the browser. A download is no longer cancelled on the strength
  of a hand-off nobody received (eon-download-manager-spec, `docs/handoff.md`);
- when no window is running, the host starts it outside the browser's job, or, where
  the job does not allow that, through the Windows shell.

Verified in a real Firefox 160: a 40 MB download on a page was taken from Firefox and the
window opened its sheet. Firefox kept nothing. This was checked twice: once with a window
already running (a separately built copy), and once with no window running (the released
v0.0.2). In the second case the host started the window, and the window stayed after the
host exited. The 30 window checks pass.

The extension's minimum Firefox version is 140, the first that reads its data-collection
statement.

**The extension installs in Firefox.** Release Firefox keeps only add-ons Mozilla has
signed; an unpacked one is gone at the next start, so until now it could not be installed
there at all. A new `firefox` workflow has each release's extension signed by Mozilla as
*unlisted* (checked and signed, shown in no catalogue) and attaches
`eon-download-manager-extension-<tag>-firefox.xpi` to the release with its checksum.
Opening that file in Firefox installs it. Verified first in a separate Firefox 160
profile: the extension loads and the application records its hello.

## v0.0.1 — 21 November 2026

**Downloads from pages that play video or sound.** Paste a page's address instead of a
file's, choose a quality, sound only or picture only, and it downloads like any other
file: paused, resumed and shown in the list. This works on most sites that play video,
through a general-purpose open-source extractor and FFmpeg. Both are fetched on first use
and checked against their published SHA-256. DRM-protected streams are refused (plan
madde 44b).

Tested against a real video page:
- the whole video as MP4, and the sound alone as MP3, with the finished file's SHA-256;
- qualities from 144p up to 2160p on offer.

Pages that need an account, or that a site blocks for your address, say so instead of
failing quietly.

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
