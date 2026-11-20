// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// The EON Download Manager extension, Chromium and Firefox from one source.
// It does three things and nothing else:
//   1. takes over downloads of the kinds that benefit (by type or size), and
//      hands them to the application with the cookies of that address only;
//   2. adds "Download with EON" to links and selections;
//   3. notices the video a page itself loads (HLS playlists, media files) and
//      offers a button over the player.
// It never downloads by itself, never sends a page's content anywhere, and
// talks only to the application, over native messaging
// (eon-download-manager-spec, docs/handoff.md).

"use strict";

const api = globalThis.browser ?? globalThis.chrome;
const HOST = "org.eon.download_manager";
const VERSION = api.runtime.getManifest().version;
const BROWSER = navigator.userAgent.includes("Firefox/") ? "firefox" : navigator.userAgent.includes("Edg/") ? "edge" : "chrome";

const DEFAULTS = {
  enabled: true,
  // Taken over by type…
  types: "zip rar 7z gz xz bz2 tar iso img dmg exe msi msix deb rpm appimage apk mp4 mkv webm avi mov mp3 flac wav m4a pdf epub",
  // …or by size, whatever the type.
  minSize: 20 * 1024 * 1024,
};

async function settings() {
  const got = await api.storage.local.get(DEFAULTS);
  return { ...DEFAULTS, ...got };
}

// ---- talking to the application --------------------------------------------------

async function send(kind, items, start = "ask") {
  const message = { handoff: 0, kind, source: { browser: BROWSER, extension: VERSION } };
  if (items) message.items = items;
  if (kind !== "ping") message.start = start;
  return api.runtime.sendNativeMessage(HOST, message);
}

async function cookiesFor(url) {
  try {
    const list = await api.cookies.getAll({ url });
    return list.map((c) => `${c.name}=${c.value}`).join("; ") || undefined;
  } catch {
    return undefined;
  }
}

function nameFrom(item) {
  const path = (item.filename || "").split(/[\\/]/).pop();
  if (path) return path;
  try {
    const last = new URL(item.finalUrl || item.url).pathname.split("/").filter(Boolean).pop();
    return last ? decodeURIComponent(last) : undefined;
  } catch {
    return undefined;
  }
}

const isWeb = (url) => /^https?:\/\//i.test(url || "");

// ---- 1. taking over downloads ------------------------------------------------------

// Downloads the extension itself restarts in the browser (the application did
// not answer) and downloads clicked with Alt are left alone.
const leaveAlone = new Set();
const altClicks = new Map(); // tabId -> time of an Alt+click

async function wanted(item) {
  const s = await settings();
  if (!s.enabled || !isWeb(item.finalUrl || item.url) || leaveAlone.has(item.url)) return false;
  const name = (nameFrom(item) || "").toLowerCase();
  const ext = name.includes(".") ? name.split(".").pop() : "";
  if (s.types.split(/\s+/).includes(ext)) return true;
  return item.totalBytes >= s.minSize && !/^text\//.test(item.mime || "");
}

api.downloads.onCreated.addListener(async (item) => {
  const alt = [...altClicks.values()].some((at) => Date.now() - at < 2500);
  if (alt) { altClicks.clear(); return; }
  if (!(await wanted(item))) return;
  try {
    await api.downloads.cancel(item.id);
    await api.downloads.erase({ id: item.id });
  } catch {
    return; // Already finished or not ours to cancel.
  }
  const url = item.finalUrl || item.url;
  const entry = {
    url: item.url,
    referrer: item.referrer || undefined,
    file_name: nameFrom(item),
    mime: item.mime || undefined,
    size: item.totalBytes > 0 ? item.totalBytes : undefined,
    cookies: await cookiesFor(url),
    user_agent: navigator.userAgent,
  };
  if (item.finalUrl && item.finalUrl !== item.url) entry.final_url = item.finalUrl;
  try {
    const reply = await send("download", [entry]);
    if (!reply?.ok) throw new Error(reply?.error?.message || "refused");
  } catch {
    // The application is not there: give the download back to the browser.
    leaveAlone.add(item.url);
    api.downloads.download({ url: item.url });
    setTimeout(() => leaveAlone.delete(item.url), 10000);
    badge("!");
  }
});

function badge(text) {
  api.action.setBadgeText({ text });
  api.action.setBadgeBackgroundColor({ color: "#a08bff" });
  if (text) setTimeout(() => api.action.setBadgeText({ text: "" }), 6000);
}

// ---- 2. the context menu --------------------------------------------------------------

api.runtime.onInstalled.addListener(() => {
  api.contextMenus.removeAll(() => {
    api.contextMenus.create({ id: "eon-link", title: api.i18n.getMessage("menu_link"), contexts: ["link"] });
    api.contextMenus.create({ id: "eon-selection", title: api.i18n.getMessage("menu_selection"), contexts: ["selection"] });
    api.contextMenus.create({ id: "eon-all", title: api.i18n.getMessage("menu_all"), contexts: ["page"] });
  });
});

api.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "eon-link" && isWeb(info.linkUrl)) {
    send("links", [{ url: info.linkUrl, referrer: tab?.url, cookies: await cookiesFor(info.linkUrl) }]).catch(() => badge("!"));
    return;
  }
  // Ask the page which links there are (in the selection, or all of them).
  const scope = info.menuItemId === "eon-selection" ? "selection" : "page";
  const links = await api.tabs.sendMessage(tab.id, { type: "links", scope }).catch(() => []);
  const unique = [...new Set(links.filter(isWeb))].slice(0, 5000);
  if (!unique.length) return;
  const items = [];
  for (const url of unique) items.push({ url, referrer: tab.url });
  send("links", items).catch(() => badge("!"));
});

// ---- 3. video the page loads -------------------------------------------------------------

const media = new Map(); // tabId -> [{ url, type, label }]

function remember(tabId, entry) {
  if (tabId < 0) return;
  const list = media.get(tabId) || [];
  if (list.some((m) => m.url === entry.url)) return;
  list.push(entry);
  media.set(tabId, list.slice(-8));
  api.tabs.sendMessage(tabId, { type: "media", count: list.length }).catch(() => {});
}

api.webRequest.onHeadersReceived.addListener(
  (details) => {
    if (!isWeb(details.url)) return;
    const header = (name) => details.responseHeaders?.find((h) => h.name.toLowerCase() === name)?.value || "";
    const type = header("content-type").toLowerCase();
    const length = Number(header("content-length")) || 0;
    const path = details.url.split("?")[0].toLowerCase();
    if (type.includes("mpegurl") || path.endsWith(".m3u8")) {
      remember(details.tabId, { url: details.url, type: "hls" });
    } else if (/^(video|audio)\//.test(type) && !type.includes("mp2t") && length > 3 * 1024 * 1024) {
      remember(details.tabId, { url: details.url, type: "file" });
    }
  },
  { urls: ["http://*/*", "https://*/*"], types: ["xmlhttprequest", "media", "other"] },
  ["responseHeaders"],
);

api.tabs.onUpdated.addListener((tabId, change) => {
  if (change.url) media.delete(tabId);
});
api.tabs.onRemoved.addListener((tabId) => media.delete(tabId));

async function downloadMedia(tab, index) {
  const list = media.get(tab.id) || [];
  // A master playlist is usually the first one a player asks for.
  const m = list[index] || list.find((x) => x.type === "hls") || list[0];
  if (!m) return;
  send("media", [{ url: m.url, referrer: tab.url, page_title: tab.title, cookies: await cookiesFor(m.url), user_agent: navigator.userAgent, media: { type: m.type } }]).catch(() => badge("!"));
}

api.runtime.onMessage.addListener((message, sender, reply) => {
  const tab = sender.tab;
  if (message.type === "alt-click" && tab) altClicks.set(tab.id, Date.now());
  if (message.type === "download-media" && tab) downloadMedia(tab, message.index ?? -1);
  if (message.type === "media-count" && tab) reply({ count: (media.get(tab.id) || []).length });
  if (message.type === "popup") {
    (async () => {
      const [active] = await api.tabs.query({ active: true, currentWindow: true });
      let version = null;
      try { const r = await send("ping"); if (r?.ok) version = r.version; } catch { /* not connected */ }
      reply({ version, settings: await settings(), media: active ? (media.get(active.id) || []) : [], tab: active?.id });
    })();
    return true;
  }
  if (message.type === "popup-media") {
    api.tabs.get(message.tab).then((t) => downloadMedia(t, message.index));
  }
  if (message.type === "set") api.storage.local.set(message.values);
  return false;
});
