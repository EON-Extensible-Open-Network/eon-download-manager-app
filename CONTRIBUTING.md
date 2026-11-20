<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# Contributing to EON Download Manager

The organisation-wide guide,
[`.github/CONTRIBUTING.md`](https://github.com/EON-Extensible-Open-Network/.github/blob/main/CONTRIBUTING.md),
applies: DCO sign-off (`git commit -s`), Conventional Commits (`feat(engine): ...`,
`fix(window): ...`, `feat(extension): ...`). By contributing you license your work under
`GPL-3.0-or-later` with the EON Module ABI Exception.

- **The engine is tested against a real server.** A change to how bytes move comes with a
  test in `engine/tests/engine.rs` that would fail without it.
- **Every string in both languages** (`src-tauri/i18n/en.json`, `tr.json`, the
  extension's `_locales`); `--selftest` fails otherwise.
- **A new command is registered twice:** in `generate_handler!` and in `COMMANDS`; the
  interface calls it by its Rust name.
- **Look at it.** A change to the window comes with what `tools/ui-check.mjs --shots`
  produced, before and after.
- **No licensing code, ever** (plan madde 44c), and no telemetry (madde 36).
