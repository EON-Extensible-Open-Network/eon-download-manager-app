// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Drives the real EON Download Manager window through the WebView2 DevTools
// protocol, against a download server this script starts on 127.0.0.1. The
// program runs with a private data folder and downloads into a temporary
// folder; browser registration is never touched. No dependencies (Node 22+),
// Windows, debug build.
//
//   node tools/ui-check.mjs --app <eon-download-manager.exe> [--shots <dir>]

import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const arg = (name) => { const at = process.argv.indexOf(name); return at > 0 ? process.argv[at + 1] : undefined; };
const appExe = arg("--app") && resolve(arg("--app"));
const shots = arg("--shots");
if (!appExe) { console.error("usage: node tools/ui-check.mjs --app <exe> [--shots <dir>]"); process.exit(2); }
if (shots) mkdirSync(shots, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const root = mkdtempSync(join(tmpdir(), "eon-dm-ui-"));
const data = join(root, "data");
const downloads = join(root, "downloads");
mkdirSync(data, { recursive: true });
const port = 9300 + Math.floor(Math.random() * 500);
let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`  ${ok ? "ok  " : "FAIL"}  ${name}${ok || !detail ? "" : `: ${detail}`}`);
  if (!ok) failures++;
};

// ---- the download server ----------------------------------------------------------
const files = new Map();
function file(name, bytes) {
  if (!files.has(name)) {
    const buf = Buffer.alloc(bytes);
    for (let i = 0; i < bytes; i++) buf[i] = ((i * 2654435761) >>> 13) ^ (i >>> 7) ^ name.length;
    files.set(name, buf);
  }
  return files.get(name);
}
const sizes = { "ubuntu-24.04-desktop-amd64.iso": 48 << 20, "s1.bin": 2 << 20, "s2.bin": 2 << 20, "s3.bin": 2 << 20, "handoff.zip": 6 << 20, "added.pdf": 3 << 20 };
const server = createServer(async (req, res) => {
  const name = decodeURIComponent(req.url.split("/").pop().split("?")[0]);
  if (!(name in sizes)) { res.writeHead(404).end(); return; }
  const body = file(name, sizes[name]);
  const range = /bytes=(\d+)-(\d*)/.exec(req.headers.range || "");
  let a = 0, b = body.length - 1, status = 200;
  const headers = { "Accept-Ranges": "bytes", "ETag": `"${name}"`, "Content-Type": "application/octet-stream" };
  if (range) { a = Number(range[1]); b = range[2] ? Math.min(Number(range[2]), b) : b; status = 206; headers["Content-Range"] = `bytes ${a}-${b}/${body.length}`; }
  headers["Content-Length"] = String(b - a + 1);
  res.writeHead(status, headers);
  // 1.5 MB/s per connection: slow enough to watch connections split.
  for (let off = a; off <= b; off += 32768) {
    if (!res.write(body.subarray(off, Math.min(off + 32768, b + 1)))) await new Promise((r) => res.once("drain", r));
    await sleep(20);
    if (res.destroyed) return;
  }
  res.end();
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;
const sha = (name) => createHash("sha256").update(file(name, sizes[name])).digest("hex");

writeFileSync(join(data, "settings.json"), JSON.stringify({
  language: "en", theme: "dark", folder: downloads, sort_by_category: true,
  engine: { max_active: 3, connections: 8, speed_limit: 0, ignore_changes: false, queue_running: true },
  watch_clipboard: false, close_to_tray: false, notify: false, tray_hint_shown: true,
}));
const env = { ...process.env, EON_DM_DATA: data, EON_WEBVIEW_ARGS: `--remote-debugging-port=${port}` };

let cdp;
async function devtools() {
  for (let i = 0; i < 300; i++) {
    try {
      const pages = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
      const page = pages.find((p) => p.type === "page" && p.webSocketDebuggerUrl);
      if (page) {
        const ws = new WebSocket(page.webSocketDebuggerUrl);
        await new Promise((ok, no) => { ws.onopen = ok; ws.onerror = no; });
        let next = 1; const waiting = new Map();
        ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && waiting.has(m.id)) { waiting.get(m.id)(m); waiting.delete(m.id); } };
        return { send: (method, params = {}) => new Promise((ok) => { const id = next++; waiting.set(id, ok); ws.send(JSON.stringify({ id, method, params })); }), close: () => ws.close() };
      }
    } catch { /* not yet */ }
    await sleep(200);
  }
  throw new Error("the window's DevTools endpoint never appeared");
}
async function js(expression) {
  const r = await cdp.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails));
  return r.result?.result?.value;
}
async function until(expression, ms = 8000) {
  const end = Date.now() + ms;
  while (Date.now() < end) { if (await js(expression).catch(() => false)) return true; await sleep(100); }
  return false;
}
async function key(keyName, code = keyName, modifiers = 0, text) {
  const vk = { ArrowDown: 40, ArrowUp: 38, Home: 36, End: 35, ArrowRight: 39, ArrowLeft: 37, Enter: 13 }[keyName] ?? keyName.toUpperCase().charCodeAt(0);
  // Shortcuts with modifiers are raw key presses, as CDP expects; plain keys may carry text.
  await cdp.send("Input.dispatchKeyEvent", { type: modifiers ? "rawKeyDown" : "keyDown", key: keyName, code, modifiers, windowsVirtualKeyCode: vk, text });
  await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", key: keyName, code, modifiers, windowsVirtualKeyCode: vk });
}
// Ctrl+ shortcuts are sent as page-level keyboard events. Synthetic OS-level
// presses through CDP reach a background-started webview only intermittently,
// which tests the delivery, not this program; arrows still go through CDP below.
async function shortcut(letter) {
  await js(`document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: '${letter}', ctrlKey: true, bubbles: true, cancelable: true }))`);
}
async function type(text) { await cdp.send("Input.insertText", { text }); }
async function shot(name) {
  if (!shots) return;
  const r = await cdp.send("Page.captureScreenshot", { format: "png" });
  writeFileSync(join(shots, `dm-${name}.png`), Buffer.from(r.result.data, "base64"));
}


function host(message) {
  // The browser's side of native messaging: start the program as a host,
  // write one framed message, read one framed reply.
  return new Promise((done, fail) => {
    const child = spawn(appExe, ["chrome-extension://piagpgnijlkmaacfoiknlbofkfnbjjgn/"], { env, stdio: ["pipe", "pipe", "ignore"] });
    const chunks = [];
    child.stdout.on("data", (c) => chunks.push(c));
    child.on("error", fail);
    child.on("exit", () => {
      const out = Buffer.concat(chunks);
      if (out.length < 4) { fail(new Error("no reply")); return; }
      const len = out.readUInt32LE(0);
      done(JSON.parse(out.subarray(4, 4 + len).toString("utf8")));
    });
    const body = Buffer.from(JSON.stringify(message));
    const head = Buffer.alloc(4);
    head.writeUInt32LE(body.length);
    child.stdin.end(Buffer.concat([head, body]));
  });
}

let app;
try {
  console.log("EON Download Manager interface check");
  app = spawn(appExe, [], { env, stdio: "ignore" });
  cdp = await devtools();
  await cdp.send("Emulation.setFocusEmulationEnabled", { enabled: true });
  const view = (name) => `[...document.querySelectorAll('#list .row')].find(r => r.querySelector('.name').textContent === ${JSON.stringify(name)})`;
  const state = (expr) => js(`(async () => { const s = await window.__TAURI__.core.invoke('snapshot'); return ${expr}; })()`);

  check("the window opens on its own, empty, ready to paste", await until(`document.body.dataset.ready === '1' && !document.getElementById('empty').hidden && document.activeElement.id === 'add-url'`, 25000));
  await shot("01-empty");

  // One link: paste, look, download.
  const iso = "ubuntu-24.04-desktop-amd64.iso";
  await js(`(() => { document.getElementById('add-url').value = ${JSON.stringify(`${base}/releases/${iso}`)}; document.getElementById('add-form').requestSubmit(); })()`);
  check("a pasted link opens the sheet with what the server says", await until(`!document.getElementById('sheet-file').hidden && document.getElementById('sheet-name').value === ${JSON.stringify(iso)} && /^48 MB/.test(document.getElementById('sheet-meta').textContent)`, 15000));
  check("the sheet says it can pause and resume, and where it goes", await js(`document.getElementById('sheet-meta').textContent.includes('pause and resume') && document.getElementById('sheet-folder').textContent.endsWith('Programs')`));
  await shot("02-sheet");
  await js(`document.getElementById('sheet-start').click()`);
  check("Download starts it and opens its details", await until(`!document.getElementById('deck').hidden && document.getElementById('deck-name').textContent === ${JSON.stringify(iso)}`));
  check("connections split the file and work at once", await until(`(async () => { const s = await window.__TAURI__.core.invoke('snapshot'); const v = s.views[0]; return v.connections >= 4 && v.lanes.length >= 5; })()`, 20000));
  check("the lane map says so in words too", await until(`/\\d+ connections working/.test(document.getElementById('lanes-caption').textContent)`));
  await sleep(1500);
  await shot("03-downloading");

  await js(`[...document.querySelectorAll('#deck-actions button')].find(b => b.textContent.includes('Pause')).click()`);
  check("Pause stops it where it is", await until(`${view(iso)}?.classList.contains('paused')`, 10000));
  const pausedAt = await state(`s.views[0].done`);
  await js(`[...document.querySelectorAll('#deck-actions button')].find(b => b.textContent.includes('Resume')).click()`);
  check("Resume continues from there", await until(`${view(iso)}?.classList.contains('downloading')`, 10000));
  const later = await state(`s.views[0].done`);
  check("nothing was lost in between", later >= pausedAt, `${later} < ${pausedAt}`);
  check("it finishes, with its SHA-256", await until(`(async () => (await window.__TAURI__.core.invoke('snapshot')).views[0].sha256 === ${JSON.stringify(sha(iso))})()`, 120000));
  await until(`!document.getElementById('verify-form').hidden`);
  await js(`(() => { document.getElementById('verify-input').value = ${JSON.stringify(sha(iso).toUpperCase())}; document.getElementById('verify-form').requestSubmit(); })()`);
  check("a published checksum can be compared", await until(`document.getElementById('verify-result').textContent.startsWith('It matches')`));
  check("the file is where the sheet said", readdirSync(join(downloads, "Programs")).includes(iso));
  await shot("04-done");

  // A numbered series.
  await js(`(() => { document.getElementById('add-url').value = ${JSON.stringify(`${base}/series/s[1-3].bin`)}; document.getElementById('add-form').requestSubmit(); })()`);
  check("a range in brackets becomes a series", await until(`document.getElementById('batch').open && document.getElementById('batch-preview').textContent.startsWith('3 files')`));
  await js(`document.getElementById('batch-start').click()`);
  check("all three download", await until(`(async () => (await window.__TAURI__.core.invoke('snapshot')).views.filter(v => v.name.startsWith('s') && v.status === 'done').length === 3)()`, 60000));

  // The browser hand-off, exactly as Chrome would do it.
  const ping = await host({ handoff: 0, kind: "ping" });
  check("the native host answers a ping", ping.ok === true && typeof ping.version === "string", JSON.stringify(ping));
  const bad = await host({ handoff: 0, kind: "links", items: [{ url: "file:///etc/passwd" }] });
  check("the native host refuses a file: address", bad.ok === false && bad.error.code === "invalid", JSON.stringify(bad));
  const handed = await host({ handoff: 0, kind: "download", items: [{ url: `${base}/dl/handoff.zip`, referrer: `${base}/page`, file_name: "handoff.zip" }] });
  check("a browser download is handed over", handed.ok === true, JSON.stringify(handed));
  check("and the running window asks about it", await until(`document.getElementById('sheet').open && document.getElementById('sheet-name').value === 'handoff.zip'`, 20000));
  await js(`document.getElementById('sheet-later').click()`);
  check("Download later keeps it, stopped", await until(`${view("handoff.zip")}?.classList.contains('paused')`, 10000));

  // A second start with --add goes to the window that is open.
  const second = spawn(appExe, ["--add", `${base}/docs/added.pdf`], { env, stdio: "ignore" });
  const exited = await new Promise((r) => { const timer = setTimeout(() => r(false), 15000); second.on("exit", () => { clearTimeout(timer); r(true); }); });
  check("a second start hands its link to the open window and exits", exited && await until(`document.getElementById('sheet').open && document.getElementById('sheet-name').value === 'added.pdf'`, 15000));
  await js(`document.querySelector('#sheet-file .sheet-cancel').click()`);

  // Keyboard.
  await js(`document.querySelector('#list .row').focus()`);
  await key("ArrowDown");
  check("arrows move through the list", await until(`document.activeElement?.classList.contains('row') && document.activeElement.getAttribute('aria-selected') === 'true'`));
  await shortcut("n");
  check("Ctrl+N goes to the link field", await until(`document.activeElement.id === 'add-url'`));

  // Settings: Turkish, light.
  await js(`document.getElementById('settings-open').click()`);
  check("settings open", await until(`document.getElementById('settings').open`));
  await shot("05-settings");
  await js(`(() => { document.getElementById('s-language').value = 'tr'; document.getElementById('s-theme').value = 'light'; document.getElementById('settings-form').requestSubmit(); })()`);
  check("Turkish, everywhere", await until(`document.getElementById('list-title').textContent === 'Tüm indirmeler' && document.querySelector('[data-filter="done"] span').textContent === 'Bitenler'`));
  check("the light theme", await until(`document.documentElement.dataset.theme === 'light'`));
  await js(`document.querySelector('#list .row').click()`);
  await sleep(800);
  await shot("06-light-tr");

  const unnamed = await js(`[...document.querySelectorAll('button, input, select, textarea')].filter(e => {
    if (e.closest('[hidden], dialog:not([open])')) return false;
    const label = e.getAttribute('aria-label') || e.textContent.trim() || (e.id && document.querySelector('label[for="' + e.id + '"]')?.textContent.trim()) || e.closest('label')?.textContent.trim();
    return !label;
  }).map(e => e.outerHTML.slice(0, 90))`);
  check("every visible control has a name", unnamed.length === 0, JSON.stringify(unnamed));
} catch (error) {
  failures++;
  console.error(`  FAIL  ${error.stack || error}`);
} finally {
  cdp?.close();
  try { app?.kill(); } catch { /* gone */ }
  server.close();
  await sleep(1500);
  try { rmSync(root, { recursive: true, force: true }); } catch { /* in use */ }
}
console.log(failures ? `\n${failures} check(s) failed.` : "\nall checks passed");
process.exit(failures ? 1 : 0);
