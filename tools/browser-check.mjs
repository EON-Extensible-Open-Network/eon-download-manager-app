// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Usage: node tools/browser-check.mjs <eon-download-manager.exe> <extension folder>
// Windows, Microsoft Edge, debug build. Writes one HKCU key for the run and
// deletes it at the end, whatever happens.
//
// End-to-end: a real Edge with the extension loaded hands a real download to
// the real application over native messaging. Registers the host for Edge
// under HKCU only for the test (pointing at a wrapper with a private data
// folder) and removes it afterwards.
import { spawn, execFileSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, readdirSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const APP = resolve(process.argv[2]);
const EXT = resolve(process.argv[3]);
const EDGE = "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe";
const KEY = "HKCU\\Software\\Microsoft\\Edge\\NativeMessagingHosts\\org.eon.download_manager";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const root = mkdtempSync(join(tmpdir(), "eon-dm-browser-"));
const data = join(root, "data"), downloads = join(root, "downloads"), edgeProfile = join(root, "edge"), edgeDownloads = join(root, "edge-downloads");
for (const d of [data, downloads, edgeProfile, edgeDownloads]) mkdirSync(d, { recursive: true });
let failures = 0;
const check = (name, ok, detail = "") => { console.log(`  ${ok ? "ok  " : "FAIL"}  ${name}${ok || !detail ? "" : `: ${detail}`}`); if (!ok) failures++; };

// A page with a link to a 6 MB zip, served with ranges.
const body = Buffer.alloc(6 << 20, 7);
const server = createServer((req, res) => {
  if (req.url === "/") { res.writeHead(200, { "Content-Type": "text/html" }).end(`<a id="dl" href="/files/release.zip">release.zip</a>`); return; }
  if (req.url.startsWith("/files/release.zip")) {
    const m = /bytes=(\d+)-(\d*)/.exec(req.headers.range || "");
    const a = m ? Number(m[1]) : 0, b = m && m[2] ? Number(m[2]) : body.length - 1;
    res.writeHead(m ? 206 : 200, { "Content-Type": "application/zip", "Content-Length": b - a + 1, "Accept-Ranges": "bytes", ...(m ? { "Content-Range": `bytes ${a}-${b}/${body.length}` } : {}) });
    res.end(body.subarray(a, b + 1));
    return;
  }
  res.writeHead(404).end();
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;

writeFileSync(join(data, "settings.json"), JSON.stringify({ language: "en", theme: "dark", folder: downloads, engine: { max_active: 3, connections: 8, speed_limit: 0, ignore_changes: false, queue_running: true }, watch_clipboard: false, close_to_tray: false, notify: false, tray_hint_shown: true }));
const wrapper = join(root, "host.bat");
writeFileSync(wrapper, `@echo off\r\nset EON_DM_DATA=${data}\r\n"${APP}" %*\r\n`);
const manifest = join(root, "host.json");
writeFileSync(manifest, JSON.stringify({ name: "org.eon.download_manager", description: "EON Download Manager (test)", path: wrapper, type: "stdio", allowed_origins: ["chrome-extension://piagpgnijlkmaacfoiknlbofkfnbjjgn/"] }));
execFileSync("reg", ["add", KEY, "/ve", "/d", manifest, "/f"]);

const appPort = 9900 + Math.floor(Math.random() * 50), edgePort = 9960 + Math.floor(Math.random() * 30);
const env = { ...process.env, EON_DM_DATA: data, EON_WEBVIEW_ARGS: `--remote-debugging-port=${appPort}` };
let app, edge;
async function cdp(port, match) {
  for (let i = 0; i < 200; i++) {
    try {
      const pages = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
      const page = pages.find(match);
      if (page) {
        const ws = new WebSocket(page.webSocketDebuggerUrl);
        await new Promise((ok, no) => { ws.onopen = ok; ws.onerror = no; });
        let next = 1; const waiting = new Map();
        ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && waiting.has(m.id)) { waiting.get(m.id)(m); waiting.delete(m.id); } };
        const send = (method, params = {}) => new Promise((ok) => { const id = next++; waiting.set(id, ok); ws.send(JSON.stringify({ id, method, params })); });
        const js = async (expression) => (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true })).result?.result?.value;
        return { send, js, close: () => ws.close(), pages };
      }
    } catch { /* not yet */ }
    await sleep(150);
  }
  throw new Error(`no DevTools target on ${port}`);
}
async function until(fn, ms = 20000) { const end = Date.now() + ms; while (Date.now() < end) { if (await fn().catch(() => false)) return true; await sleep(200); } return false; }

try {
  console.log("EON Download Manager in a real browser");
  app = spawn(APP, [], { env, stdio: "ignore" });
  const win = await cdp(appPort, (p) => p.type === "page");
  check("the application is running", await until(() => win.js(`document.body.dataset.ready === '1'`)));

  edge = spawn(EDGE, [`--user-data-dir=${edgeProfile}`, `--load-extension=${EXT}`, `--disable-extensions-except=${EXT}`, `--remote-debugging-port=${edgePort}`, "--no-first-run", "--no-default-browser-check", "--headless=new", `${base}/`], { stdio: "ignore" });
  const ext = await cdp(edgePort, (p) => p.type === "service_worker" && p.url.includes("piagpgnijlkmaacfoiknlbofkfnbjjgn"));
  check("Edge loaded the extension with the fixed id", true);
  const ping = await ext.js(`chrome.runtime.sendNativeMessage("org.eon.download_manager", { handoff: 0, kind: "ping" }).then(r => JSON.stringify(r), e => "ERR " + e.message)`);
  check("the extension reaches the application over native messaging", ping?.includes('"ok":true'), ping);

  const page = await cdp(edgePort, (p) => p.type === "page" && p.url.startsWith(base));
  await page.send("Browser.setDownloadBehavior", { behavior: "allow", downloadPath: edgeDownloads }).catch(() => {});
  await page.js(`document.getElementById('dl').click()`);
  check("a click on a zip lands in the application's add sheet", await until(() => win.js(`document.getElementById('sheet').open && document.getElementById('sheet-name').value === 'release.zip'`), 30000));
  check("with the size the server gave", await until(() => win.js(`/^6 MB/.test(document.getElementById('sheet-meta').textContent)`)));
  await sleep(2000);
  const browserGot = readdirSync(edgeDownloads).filter((f) => !f.endsWith(".tmp"));
  check("and the browser did not download it as well", browserGot.length === 0, JSON.stringify(browserGot));
  await win.js(`document.getElementById('sheet-start').click()`);
  check("it downloads in the application", await until(async () => existsSync(join(downloads, "Compressed", "release.zip")), 30000));
} catch (e) {
  failures++;
  console.error("  FAIL ", e.stack || e);
} finally {
  try { execFileSync("reg", ["delete", KEY, "/f"]); console.log("  (Edge host registration removed)"); } catch { console.log("  (could not remove the registration!)"); }
  try { edge?.kill(); } catch {}
  try { app?.kill(); } catch {}
  server.close();
  await sleep(1500);
  try { execFileSync("taskkill", ["/F", "/IM", "eon-download-manager.exe"], { stdio: "ignore" }); } catch {}
  try { rmSync(root, { recursive: true, force: true }); } catch {}
}
console.log(failures ? `\n${failures} check(s) failed.` : "\nall checks passed");
process.exit(failures ? 1 : 0);
