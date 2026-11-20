// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// EON Download Manager's interface. It has no network of its own: everything
// goes through the host. Text that comes from servers and pages (file names,
// addresses) enters the page only through textContent.

"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();
const $ = (id) => document.getElementById(id);

const LANE_COLORS = ["--current", "--pulse", "--rose", "--amber"];
const KINDS = ["video", "music", "documents", "compressed", "programs", "images", "other"];
const KIND_ICON = { video: "film", music: "music", documents: "file-text", compressed: "archive", programs: "package", images: "image", other: "file" };

let strings = {};
let lang = "en";
let settings = null;
let downloadsDir = "";
let views = new Map();
let order = [];
let filter = { view: "all", kind: null };
let selected = null;
let totalHistory = [];
let lastTotalSample = 0;
let scheduledRun = false;
let pendingSelect = null; // a download just added, selected once it appears

// ---- text and numbers -----------------------------------------------------------

function t(key, vars = {}) {
  let text = strings[key] ?? key;
  for (const [n, v] of Object.entries(vars)) text = text.replaceAll(`{${n}}`, String(v));
  return text;
}

function num(value, digits, min = digits) {
  return new Intl.NumberFormat(lang, { maximumFractionDigits: digits, minimumFractionDigits: min }).format(value);
}

/// Sizes drop a trailing ".0" (48 MB); speeds keep one fixed decimal so the
/// number does not change width as it moves.
function percent(p) {
  return new Intl.NumberFormat(lang, { style: "percent", maximumFractionDigits: p > 0 && p < 10 ? 1 : 0 }).format(p / 100);
}

function sizeParts(bytes, fixed = false) {
  if (bytes == null) return ["", ""];
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = bytes;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  const digits = i === 0 ? 0 : v >= 100 ? 0 : 1;
  return [num(v, digits, fixed ? digits : 0), units[i]];
}
const size = (b) => sizeParts(b).join(" ");
const speedText = (b) => `${sizeParts(b || 0, true).join(" ")}/s`;

function duration(seconds) {
  if (seconds == null || !isFinite(seconds)) return "";
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return t("time.s", { n: s });
  if (s < 3600) return t("time.ms", { m: Math.floor(s / 60), s: s % 60 });
  return t("time.hm", { h: Math.floor(s / 3600), m: Math.floor((s % 3600) / 60) });
}

function when(ms) {
  if (!ms) return "";
  return new Intl.DateTimeFormat(lang, { dateStyle: "medium", timeStyle: "short" }).format(new Date(ms));
}

const reason = (e) => (e && (e.message || e.code)) || String(e);
const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

function announce(text) {
  $("announcer").textContent = "";
  window.setTimeout(() => { $("announcer").textContent = text; }, 30);
}

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === null || v === undefined || v === false) continue;
    if (k === "class") node.className = v;
    else if (k === "text") node.textContent = v;
    else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else node.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children) if (c !== null && c !== undefined && c !== false) node.append(c);
  return node;
}

function icon(name) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `icons.svg#i-${name}`);
  svg.append(use);
  return svg;
}

function glyph(node, kind) {
  node.dataset.kind = kind;
  node.replaceChildren(icon(KIND_ICON[kind] || "file"));
}

// ---- strings, theme -----------------------------------------------------------------

async function loadStrings() {
  const pref = settings?.language || "system";
  lang = pref !== "system" ? pref : ((navigator.language || "en").toLowerCase().startsWith("tr") ? "tr" : "en");
  strings = await invoke("strings", { language: lang });
  document.documentElement.lang = lang;
  for (const n of document.querySelectorAll("[data-i18n]")) n.textContent = t(n.dataset.i18n);
  for (const n of document.querySelectorAll("[data-i18n-label]")) n.setAttribute("aria-label", t(n.dataset.i18nLabel));
  for (const n of document.querySelectorAll("[data-i18n-placeholder]")) n.setAttribute("placeholder", t(n.dataset.i18nPlaceholder));
}

function applyTheme() {
  const pref = settings?.theme || "system";
  const dark = pref === "dark" || (pref === "system" && !window.matchMedia("(prefers-color-scheme: light)").matches);
  document.documentElement.dataset.theme = dark ? "dark" : "light";
  drawEmpty();
}
window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", applyTheme);

// ---- the list ----------------------------------------------------------------------------

function matches(v) {
  if (filter.kind && v.category !== filter.kind) return false;
  switch (filter.view) {
    case "active": return v.status === "downloading";
    case "queued": return v.status === "queued" || v.status === "paused";
    case "done": return v.status === "done";
    case "failed": return v.status === "failed";
    default: return true;
  }
}

function percentOf(v) {
  if (v.status === "done") return 100;
  if (v.pieces) return v.pieces[1] ? (v.pieces[0] / v.pieces[1]) * 100 : 0;
  if (!v.length) return null;
  return Math.min(100, (v.done / v.length) * 100);
}

function metaFor(v) {
  const parts = [];
  const p = percentOf(v);
  const ofTotal = v.length ? t("meta.of", { done: size(v.done), total: size(v.length) }) : size(v.done);
  switch (v.status) {
    case "downloading":
      if (v.pieces) parts.push([t("meta.pieces", { done: v.pieces[0], total: v.pieces[1] }), ""]);
      else parts.push([ofTotal, ""]);
      parts.push([speedText(v.speed), "speed"]);
      if (v.eta != null) parts.push([t("meta.left", { time: duration(v.eta) }), ""]);
      break;
    case "queued":
      parts.push([t("status.queued"), ""]);
      if (v.length) parts.push([size(v.length), ""]);
      break;
    case "paused":
      parts.push([p != null ? t("status.paused.at", { percent: percent(p) }) : t("status.paused"), ""]);
      if (v.length) parts.push([ofTotal, ""]);
      break;
    case "done":
      parts.push([size(v.length ?? v.done), ""]);
      parts.push([t("status.done"), "good"]);
      break;
    case "failed":
      parts.push([errorText(v.error), "bad"]);
      break;
  }
  return parts;
}

function errorText(error) {
  if (!error) return t("status.failed");
  const key = `error.${error.code}`;
  return strings[key] ? t(key) : error.detail;
}

function rowFor(v) {
  const li = el("li", { class: "row", role: "option", tabindex: "-1", "data-id": v.id, "aria-selected": "false" });
  const g = el("span", { class: "glyph", "aria-hidden": "true" });
  const name = el("div", { class: "name" });
  const meta = el("div", { class: "meta" });
  const bar = el("canvas", { class: "bar", "aria-hidden": "true", height: "10" });
  const act = el("div", { class: "act" });
  li.append(g, el("div", { class: "body" }, name, meta, bar), act);
  li.addEventListener("click", () => select(v.id));
  li.addEventListener("dblclick", () => { const cur = views.get(v.id); if (cur?.status === "done") invoke("open_file", { id: v.id }).catch(fail); });
  li._parts = { g, name, meta, bar, act };
  return li;
}

function primaryAction(v) {
  switch (v.status) {
    case "downloading": return ["pause", t("action.pause"), () => invoke("pause", { id: v.id })];
    case "paused": case "failed": case "queued": return ["play", t("action.resume"), () => invoke("start_now", { id: v.id })];
    case "done": return ["folder-open", t("action.folder"), () => invoke("show_in_folder", { id: v.id }).catch(fail)];
    default: return null;
  }
}

function updateRow(li, v) {
  const { g, name, meta, bar, act } = li._parts;
  if (g.dataset.kind !== v.category) glyph(g, v.category);
  if (name.textContent !== v.name) name.textContent = v.name;
  li.className = `row ${v.status}`;
  const parts = metaFor(v);
  const sig = parts.map((p) => p.join("|")).join("·");
  if (meta.dataset.sig !== sig) {
    meta.dataset.sig = sig;
    meta.replaceChildren(...parts.map(([text, cls]) => el("span", { class: cls || null, text })));
  }
  const showBar = v.status !== "done" && v.status !== "queued" && (v.lanes.length > 0 || v.length);
  bar.hidden = !showBar;
  if (showBar) drawBar(bar, v);
  const action = primaryAction(v);
  const sigA = `${v.status}`;
  if (act.dataset.sig !== sigA && action) {
    act.dataset.sig = sigA;
    const [ic, label, run] = action;
    act.replaceChildren(el("button", { type: "button", class: "ghost", "aria-label": `${label}: ${v.name}`, title: label, onclick: (e) => { e.stopPropagation(); run(); } }, icon(ic)));
  }
  li.setAttribute("aria-label", `${v.name}, ${parts.map((p) => p[0]).join(", ")}`);
}

function render() {
  const list = $("list");
  const shown = order.map((id) => views.get(id)).filter((v) => v && matches(v));
  const existing = new Map([...list.children].map((n) => [n.dataset.id, n]));
  const nodes = shown.map((v) => {
    const node = existing.get(v.id) || rowFor(v);
    updateRow(node, v);
    node.setAttribute("aria-selected", String(v.id === selected));
    node.tabIndex = v.id === (selected || shown[0]?.id) ? 0 : -1;
    return node;
  });
  // Only move nodes that are out of place, so focus and hover survive.
  nodes.forEach((n, i) => { if (list.children[i] !== n) list.insertBefore(n, list.children[i] || null); });
  while (list.children.length > nodes.length) list.lastElementChild.remove();
  $("empty").hidden = views.size > 0;
  list.hidden = views.size === 0;
  renderCounts();
  const titleKey = filter.kind ? `kind.${filter.kind}` : `view.${filter.view}`;
  $("list-title").textContent = t(titleKey);
  if (selected && !views.has(selected)) closeDeck();
  if (selected) renderDeck();
}

function renderCounts() {
  const all = [...views.values()];
  const counts = {
    all: all.length,
    active: all.filter((v) => v.status === "downloading").length,
    queued: all.filter((v) => v.status === "queued" || v.status === "paused").length,
    done: all.filter((v) => v.status === "done").length,
    failed: all.filter((v) => v.status === "failed").length,
  };
  for (const b of document.querySelectorAll("#views button")) {
    const n = counts[b.dataset.filter];
    b.querySelector(".count").textContent = n ? String(n) : "";
    b.setAttribute("aria-current", !filter.kind && filter.view === b.dataset.filter ? "page" : "false");
  }
  const kinds = KINDS.filter((k) => all.some((v) => v.category === k));
  const list = $("kinds");
  if (list.dataset.sig !== kinds.join() + lang + (filter.kind || "")) {
    list.dataset.sig = kinds.join() + lang + (filter.kind || "");
    list.replaceChildren(...kinds.map((k) => el("li", {}, el("button", {
      type: "button", "data-kind": k, "aria-current": filter.kind === k ? "page" : "false",
      onclick: () => { filter = filter.kind === k ? { view: "all", kind: null } : { view: "all", kind: k }; render(); },
    }, el("span", { class: "swatch", "aria-hidden": "true" }), el("span", { text: t(`kind.${k}`) }), el("span", { class: "count" })))));
  }
  for (const b of list.querySelectorAll("button")) {
    b.querySelector(".count").textContent = String(all.filter((v) => v.category === b.dataset.kind).length);
  }
  $("rail-kinds-heading")?.toggleAttribute("hidden", kinds.length === 0);
}

for (const b of document.querySelectorAll("#views button")) {
  b.addEventListener("click", () => { filter = { view: b.dataset.filter, kind: null }; render(); });
}

function apply(snapshot) {
  const next = new Map(snapshot.views.map((v) => [v.id, v]));
  for (const [id, v] of next) {
    const before = views.get(id);
    if (before && before.status !== "done" && v.status === "done") {
      const node = document.querySelector(`#list [data-id="${CSS.escape(id)}"]`);
      node?.classList.add("settle");
      announce(t("announce.done", { name: v.name }));
    }
  }
  views = next;
  order = [...next.values()].sort((a, b) => b.added - a.added).map((v) => v.id);
  if (pendingSelect && views.has(pendingSelect)) { const id = pendingSelect; pendingSelect = null; select(id); }
  const total = snapshot.total || 0;
  const tot = $("total-speed");
  tot.textContent = speedText(total);
  tot.parentElement.classList.toggle("moving", total > 0);
  const now = Date.now();
  if (now - lastTotalSample >= 1000) {
    lastTotalSample = now;
    totalHistory.push(total);
    if (totalHistory.length > 60) totalHistory.shift();
    drawGraph($("total-graph"), totalHistory, "--current");
  }
  render();
}

// ---- drawing ---------------------------------------------------------------------------------

function fit(canvas) {
  const r = canvas.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  const w = Math.max(1, Math.round(r.width * dpr));
  const h = Math.max(1, Math.round(r.height * dpr));
  if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h; }
  const ctx = canvas.getContext("2d");
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  return [ctx, r.width, r.height];
}

function laneColor(i) {
  return css(LANE_COLORS[i % LANE_COLORS.length]);
}

/// The file as one strip: each segment's arrived part in its lane's colour.
function drawStrip(ctx, x, y, w, h, v, lanes, radius) {
  ctx.fillStyle = css("--line");
  roundRect(ctx, x, y, w, h, radius);
  ctx.fill();
  const total = v.kind === "hls" ? (lanes.at(-1)?.end || 1) : (v.length || lanes.at(-1)?.end || 1);
  ctx.save();
  roundRect(ctx, x, y, w, h, radius);
  ctx.clip();
  lanes.forEach((l, i) => {
    const a = x + (l.start / total) * w;
    const b = x + (l.pos / total) * w;
    if (b > a) {
      ctx.fillStyle = v.status === "done" ? css("--mint") : laneColor(i);
      ctx.fillRect(a, y, Math.max(1, b - a), h);
    }
  });
  ctx.restore();
}

function roundRect(ctx, x, y, w, h, r) {
  ctx.beginPath();
  ctx.roundRect(x, y, w, h, Math.min(r, h / 2, w / 2));
}

function drawBar(canvas, v) {
  const [ctx, w, h] = fit(canvas);
  ctx.clearRect(0, 0, w, h);
  const lanes = v.lanes.length ? v.lanes : [{ start: 0, pos: v.done, end: v.length || 1 }];
  if (v.status !== "downloading") ctx.globalAlpha = 0.55;
  drawStrip(ctx, 0, 0, w, h, v, lanes, 3);
  ctx.globalAlpha = 1;
}

function drawGraph(canvas, values, colorVar) {
  const [ctx, w, h] = fit(canvas);
  ctx.clearRect(0, 0, w, h);
  // A faint baseline over the whole minute, so a young graph reads as "recent".
  ctx.fillStyle = css("--line");
  ctx.fillRect(0, h - 1, w, 1);
  if (values.length < 3) return;
  const max = Math.max(...values, 1);
  // Under a minute of history fills the width; after that, the last minute.
  const step = w / (Math.min(60, Math.max(values.length, 12)) - 1);
  const x0 = w - (values.length - 1) * step;
  const color = css(colorVar);
  ctx.beginPath();
  values.forEach((v, i) => {
    const x = x0 + i * step;
    const y = h - 2 - (v / max) * (h - 6);
    if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
  });
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.5;
  ctx.lineJoin = "round";
  ctx.stroke();
  ctx.lineTo(w, h);
  ctx.lineTo(x0, h);
  ctx.closePath();
  const grad = ctx.createLinearGradient(0, 0, 0, h);
  grad.addColorStop(0, color + "55");
  grad.addColorStop(1, color + "00");
  ctx.fillStyle = grad;
  ctx.fill();
}

// The lane map: one row per segment, positioned where it sits in the file.
// Positions glide between snapshots; a newly split lane peels off its parent.
const laneState = new Map(); // key -> { disp, born }
let laneFrame = 0;
const reduceMotion = () => window.matchMedia("(prefers-reduced-motion: reduce)").matches;

function drawLanes() {
  laneFrame = 0;
  const v = selected && views.get(selected);
  const canvas = $("lanes");
  if (!v || $("deck").hidden) return;
  const [ctx, w, h] = fit(canvas);
  ctx.clearRect(0, 0, w, h);
  const pad = 14;
  const lanes = v.lanes.length ? v.lanes : [{ start: 0, pos: v.status === "done" ? (v.length || 1) : v.done, end: v.length || Math.max(v.done, 1), active: false }];
  const now = performance.now();
  let moving = false;
  const shown = lanes.map((l, i) => {
    const key = `${v.id}:${l.start}`;
    let s = laneState.get(key);
    if (!s) { s = { disp: l.pos, born: laneState.size && !reduceMotion() ? now : 0 }; laneState.set(key, s); }
    const target = l.pos;
    if (reduceMotion()) s.disp = target;
    else s.disp += (target - s.disp) * 0.16;
    if (Math.abs(target - s.disp) > 1) moving = true;
    const grow = s.born ? Math.min(1, (now - s.born) / 260) : 1;
    if (grow < 1) moving = true;
    return { ...l, pos: s.disp, grow, i };
  });

  // The whole file, on top.
  drawStrip(ctx, pad, pad, w - pad * 2, 8, v, shown, 4);

  if (v.kind === "hls") {
    // Pieces as cells: each fills by its share done.
    const cols = 16;
    const rows = Math.ceil(shown.length / cols);
    const top = pad + 22;
    const cw = (w - pad * 2) / cols;
    const ch = Math.min(18, (h - top - pad) / Math.max(rows, 1));
    shown.forEach((l, i) => {
      const x = pad + (i % cols) * cw;
      const y = top + Math.floor(i / cols) * ch;
      const f = (l.pos - l.start) / Math.max(1, l.end - l.start);
      ctx.fillStyle = css("--line");
      roundRect(ctx, x + 1.5, y + 1.5, cw - 3, ch - 3, 3);
      ctx.fill();
      if (f > 0) {
        ctx.fillStyle = v.status === "done" ? css("--mint") : laneColor(i);
        roundRect(ctx, x + 1.5, y + 1.5, (cw - 3) * f, ch - 3, 3);
        ctx.fill();
      }
    });
  } else if (v.status === "done") {
    // Finished: the file as a mosaic of the parts that built it, each in the
    // colour of the lane that fetched it.
    const total = v.length || shown.at(-1).end || 1;
    const span = w - pad * 2;
    const top = pad + 30;
    const hh = h - top - pad;
    ctx.save();
    roundRect(ctx, pad, top, span, hh, 10);
    ctx.clip();
    shown.forEach((l, i) => {
      const a = pad + (l.start / total) * span;
      const b = pad + (l.end / total) * span;
      const grad = ctx.createLinearGradient(0, top, 0, top + hh);
      grad.addColorStop(0, laneColor(i));
      grad.addColorStop(1, laneColor(i) + "66");
      ctx.fillStyle = grad;
      ctx.fillRect(a, top, Math.max(1, b - a - 1.5), hh);
    });
    ctx.restore();
  } else {
    const total = v.length || shown.at(-1).end || 1;
    const top = pad + 26;
    const avail = h - top - pad;
    const rowH = Math.max(4, Math.min(22, avail / Math.max(shown.length, 1)));
    const barH = Math.max(2, Math.min(8, rowH * 0.42));
    const span = w - pad * 2;
    let y = top;
    for (const l of shown) {
      const rh = rowH * l.grow;
      const cy = y + rh / 2;
      const a = pad + (l.start / total) * span;
      const b = pad + (l.end / total) * span;
      const p = pad + (l.pos / total) * span;
      ctx.globalAlpha = l.grow;
      // The lane, across the whole file, faint; its own part of the file, firmer.
      ctx.fillStyle = css("--line");
      ctx.globalAlpha = l.grow * 0.45;
      ctx.fillRect(pad, cy - 0.5, span, 1);
      ctx.globalAlpha = l.grow;
      roundRect(ctx, a, cy - barH / 2, Math.max(2, b - a), barH, barH / 2);
      ctx.fill();
      const color = v.status === "done" ? css("--mint") : laneColor(l.i);
      if (p > a) {
        const grad = ctx.createLinearGradient(a, 0, p, 0);
        grad.addColorStop(0, color + "40");
        grad.addColorStop(1, color);
        ctx.fillStyle = grad;
        roundRect(ctx, a, cy - barH / 2, Math.max(2, p - a), barH, barH / 2);
        ctx.fill();
      }
      if (l.active && v.status === "downloading") {
        ctx.save();
        ctx.shadowColor = color;
        ctx.shadowBlur = 10;
        ctx.fillStyle = color;
        ctx.beginPath();
        ctx.arc(p, cy, barH * 0.85 + 0.6, 0, Math.PI * 2);
        ctx.fill();
        ctx.restore();
      }
      ctx.globalAlpha = 1;
      y += rh;
    }
  }
  if (moving) laneFrame = requestAnimationFrame(drawLanes);
}

function scheduleLanes() {
  if (!laneFrame) laneFrame = requestAnimationFrame(drawLanes);
}

function drawEmpty() {
  const canvas = $("empty-art");
  if (!canvas || $("empty").hidden) return;
  const [ctx, w, h] = fit(canvas);
  ctx.clearRect(0, 0, w, h);
  const rows = [0.18, 0.62, 0.38, 0.8];
  rows.forEach((f, i) => {
    const y = 26 + i * 28;
    const color = laneColor(i);
    ctx.fillStyle = css("--line");
    roundRect(ctx, 20, y - 2, w - 40, 4, 2);
    ctx.fill();
    const x = 20 + (w - 40) * f;
    const grad = ctx.createLinearGradient(20, 0, x, 0);
    grad.addColorStop(0, color + "00");
    grad.addColorStop(1, color);
    ctx.fillStyle = grad;
    roundRect(ctx, 20, y - 2, x - 20, 4, 2);
    ctx.fill();
    ctx.save();
    ctx.shadowColor = color;
    ctx.shadowBlur = 12;
    ctx.fillStyle = color;
    ctx.beginPath();
    ctx.arc(x, y, 4.5, 0, Math.PI * 2);
    ctx.fill();
    ctx.restore();
  });
}

// ---- the deck ----------------------------------------------------------------------------

function select(id, focus = false) {
  selected = id;
  $("deck").hidden = false;
  $("layout").classList.add("with-deck");
  $("verify-input").value = "";
  $("verify-result").textContent = "";
  render();
  const node = document.querySelector(`#list [data-id="${CSS.escape(id)}"]`);
  if (focus) node?.focus();
}

function closeDeck() {
  const was = selected;
  selected = null;
  $("deck").hidden = true;
  $("layout").classList.remove("with-deck");
  render();
  if (was) document.querySelector(`#list [data-id="${CSS.escape(was)}"]`)?.focus();
}
$("deck-close").addEventListener("click", closeDeck);

const LIMITS = [0, 256 * 1024, 512 * 1024, 1 << 20, 2 << 20, 5 << 20, 10 << 20, 20 << 20];

function deckActions(v) {
  const b = (ic, key, run, cls = "secondary") => el("button", { type: "button", class: cls, onclick: run }, icon(ic), el("span", { text: t(key) }));
  const id = v.id;
  const out = [];
  if (v.status === "downloading") out.push(b("pause", "action.pause", () => invoke("pause", { id }), "primary"));
  if (["paused", "failed", "queued"].includes(v.status)) out.push(b("play", v.status === "failed" ? "action.retry" : "action.resume", () => invoke("start_now", { id }), "primary"));
  if (v.status === "done") out.push(b("external-link", "action.open", () => invoke("open_file", { id }).catch(fail), "primary"));
  out.push(b("folder-open", "action.folder", () => invoke("show_in_folder", { id }).catch(fail)));
  if (v.status !== "done" && v.kind === "file") out.push(b("refresh-cw", "action.refresh", () => openRefresh(id)));
  out.push(b("trash-2", "action.remove", () => confirmRemove(id), "ghost"));
  return out;
}

function renderDeck() {
  const v = views.get(selected);
  if (!v) return;
  glyph($("deck-glyph"), v.category);
  if ($("deck-name").textContent !== v.name) $("deck-name").textContent = v.name;
  $("deck-folder").textContent = v.folder;
  const live = v.status === "downloading";
  const [n, unit] = live ? sizeParts(v.speed, true) : sizeParts(v.status === "done" ? (v.length ?? v.done) : v.done);
  $("deck-speed").textContent = n || "0";
  $("deck-speed-unit").textContent = live ? `${unit || "B"}/s` : (unit || "B");
  const p = percentOf(v);
  $("deck-percent").textContent = p == null ? size(v.done) : percent(p);
  $("deck-eta").textContent = v.status === "downloading" ? (v.eta != null ? t("meta.left", { time: duration(v.eta) }) : "") : t(`status.${v.status}`);
  const caption = v.kind === "hls"
    ? t("deck.pieces", { done: v.pieces?.[0] ?? 0, total: v.pieces?.[1] ?? 0 })
    : v.status === "downloading"
      ? t("deck.lanes.live", { n: v.connections, segments: v.lanes.length })
      : v.status === "done"
        ? t("deck.mosaic", { n: v.lanes.length || 1 })
        : t("deck.lanes.still", { segments: v.lanes.length || 1 });
  $("lanes-caption").textContent = caption;
  $("lanes").setAttribute("aria-label", caption);
  drawGraph($("graph"), v.history, "--current");
  scheduleLanes();

  const err = $("deck-error");
  err.hidden = v.status !== "failed";
  if (v.status === "failed") {
    err.textContent = `${errorText(v.error)}${v.error?.code === "refused" || v.error?.code === "gone" ? " " + t("error.hint.refresh") : ""}`;
  }

  const actions = $("deck-actions");
  const sig = `${v.id}:${v.status}:${lang}`;
  if (actions.dataset.sig !== sig) {
    actions.dataset.sig = sig;
    actions.replaceChildren(...deckActions(v));
  }

  const facts = $("facts");
  const sigF = `${v.id}:${v.status}:${v.length}:${v.sha256}:${v.resumable}:${lang}:${v.speed_limit}:${v.own_connections}:${settings.engine.connections}`;
  if (facts.dataset.sig !== sigF) {
    facts.dataset.sig = sigF;
    const rows = [];
    const fact = (key, value) => rows.push(el("dt", { text: t(key) }), el("dd", {}, ...(Array.isArray(value) ? value : [value])));
    fact("fact.size", v.length != null ? size(v.length) : t("fact.unknown"));
    fact("fact.resume", v.resumable ? t("fact.yes") : t("fact.no"));
    if (v.status !== "done" && v.kind === "file") {
      const sel = el("select", { "aria-label": t("fact.limit"), onchange: (e) => invoke("set_speed_limit", { id: v.id, bytesPerSecond: Number(e.target.value) }) },
        ...LIMITS.map((l) => el("option", { value: String(l), text: l ? speedText(l) : t("limit.none"), selected: l === v.speed_limit })));
      fact("fact.limit", sel);
      const conn = el("select", { "aria-label": t("fact.connections"), onchange: (e) => invoke("set_connections", { id: v.id, connections: e.target.value === "" ? null : Number(e.target.value) }) },
        el("option", { value: "", text: t("connections.auto", { n: settings.engine.connections }), selected: v.own_connections == null }),
        ...[1, 2, 4, 6, 8, 12, 16, 24, 32].map((n) => el("option", { value: String(n), text: String(n), selected: v.own_connections === n })));
      fact("fact.connections", conn);
    }
    const addr = el("span", { class: "url", text: v.url });
    const copy = el("button", { type: "button", class: "ghost icon-only", "aria-label": t("fact.copy"), title: t("fact.copy"), onclick: () => navigator.clipboard.writeText(v.url).then(() => announce(t("fact.copied"))) }, icon("copy"));
    fact("fact.address", [addr, copy]);
    fact("fact.added", when(v.added));
    if (v.finished) fact("fact.finished", when(v.finished));
    if (v.sha256) fact("fact.sha", el("span", { class: "hash", text: v.sha256 }));
    facts.replaceChildren(...rows);
  }
  $("verify-form").hidden = !v.sha256;
}

$("verify-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const ok = await invoke("verify", { id: selected, expected: $("verify-input").value });
  const r = $("verify-result");
  r.className = `verify-result ${ok ? "good" : "bad"}`;
  r.textContent = ok ? t("verify.match") : t("verify.mismatch");
});

// ---- adding: one link, a series, a hand-off --------------------------------------------------

let sheetSource = null;
let sheetProbe = null;
let sheetFolder = "";

// ---- what to save: quality for a page, a format when the person asked to choose --

const EXT = (name) => (/\.([a-z0-9]{1,5})$/i.exec(name || "")?.[1] || "").toLowerCase();
const VIDEO_EXT = ["mp4", "webm", "mkv", "mov", "avi", "m4v", "ts", "flv", "wmv"];
const AUDIO_EXT = ["mp3", "m4a", "aac", "wav", "flac", "ogg", "opus", "wma"];
const IMAGE_EXT = ["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff", "avif"];

// Each choice: what the add request carries, and the extension it gives the file.
function formatChoices(probe, name) {
  const ask = settings.ask_format;
  const media = probe.media;
  if (media) {
    const list = [];
    if (media.has_video) {
      list.push({ id: "best", label: t("format.best"), media: { pick: { kind: "best" }, container: "mp4" }, ext: "mp4" });
      for (const h of media.heights.slice(0, 6)) {
        list.push({ id: `h${h}`, label: t("format.height", { h }), media: { pick: { kind: "height", value: h }, container: "mp4" }, ext: "mp4" });
      }
    }
    if (ask || !media.has_video) {
      if (media.has_audio) {
        list.push({ id: "mp3", label: t("format.audio", { f: "MP3" }), media: { pick: { kind: "audio_only" }, container: "mp3" }, ext: "mp3" });
        list.push({ id: "m4a", label: t("format.audio", { f: "M4A" }), media: { pick: { kind: "audio_only" }, container: "m4a" }, ext: "m4a" });
      }
      if (ask && media.has_video) {
        list.push({ id: "mkv", label: t("format.as", { f: "MKV" }), media: { pick: { kind: "best" }, container: "mkv" }, ext: "mkv" });
        list.push({ id: "silent", label: t("format.silent"), media: { pick: { kind: "video_only" }, container: "mp4" }, ext: "mp4" });
      }
    }
    return list;
  }
  if (!ask || probe.hls) return [];
  const ext = EXT(name);
  const original = { id: "original", label: t("format.original", { f: ext.toUpperCase() || "?" }), convert: null, ext };
  const as = (f, label) => ({ id: f, label, convert: f, ext: f });
  if (ext === "gif") return [original, as("png", t("format.still", { f: "PNG" })), as("jpg", t("format.still", { f: "JPG" })), as("mp4", t("format.as", { f: "MP4" }))];
  if (VIDEO_EXT.includes(ext)) {
    return [original, ...["mp4", "mkv"].filter((f) => f !== ext).map((f) => as(f, t("format.as", { f: f.toUpperCase() }))),
      as("mp3", t("format.audio", { f: "MP3" })), as("m4a", t("format.audio", { f: "M4A" })),
      { id: "video_only", label: t("format.silent"), convert: "video_only", ext }];
  }
  if (AUDIO_EXT.includes(ext)) return [original, ...["mp3", "m4a"].filter((f) => f !== ext).map((f) => as(f, t("format.as", { f: f.toUpperCase() })))];
  if (IMAGE_EXT.includes(ext)) return [original, ...["png", "jpg"].filter((f) => f !== ext && !(f === "jpg" && ext === "jpeg")).map((f) => as(f, t("format.as", { f: f.toUpperCase() })))];
  return [];
}

let sheetChoices = [];
function renderFormats(probe, name) {
  sheetChoices = formatChoices(probe, name);
  $("sheet-format").hidden = sheetChoices.length === 0;
  $("sheet-format-select").replaceChildren(...sheetChoices.map((c, i) => el("option", { value: c.id, text: c.label, selected: i === 0 })));
}
function chosenFormat() {
  return sheetChoices.find((c) => c.id === $("sheet-format-select").value) || null;
}
$("sheet-format-select").addEventListener("change", () => {
  const c = chosenFormat();
  const input = $("sheet-name");
  if (c?.ext) input.value = input.value.replace(/(\.[a-z0-9]{1,5})?$/i, `.${c.ext}`);
});

function sheetShow(part) {
  for (const id of ["sheet-loading", "sheet-error", "sheet-file", "sheet-links"]) $(id).hidden = id !== part;
}

function openSheet() {
  const d = $("sheet");
  if (!d.open) d.showModal();
}

async function addLink(source) {
  sheetSource = source;
  sheetProbe = null;
  sheetShow("sheet-loading");
  openSheet();
  try {
    const probe = await invoke("probe", { source });
    sheetProbe = probe;
    sheetFolder = probe.folder;
    const name = source.file_name || probe.file_name;
    $("sheet-name").value = name;
    glyph($("sheet-glyph"), probe.category);
    const meta = [];
    if (probe.media) {
      if (probe.media.uploader) meta.push(el("span", { text: probe.media.uploader }));
      if (probe.media.duration) meta.push(el("span", { text: duration(probe.media.duration) }));
    } else if (probe.hls) {
      meta.push(el("span", { text: t("sheet.video", { time: duration(probe.hls.seconds) }) }));
    } else {
      meta.push(el("span", { text: probe.length != null ? size(probe.length) : t("sheet.size.unknown") }));
    }
    meta.push(probe.resumable
      ? el("span", { class: "badge good" }, icon("shield-check"), el("span", { text: t("sheet.resumable") }))
      : el("span", { class: "badge warn" }, icon("circle-alert"), el("span", { text: t("sheet.not.resumable") })));
    $("sheet-meta").replaceChildren(...meta);
    const variants = probe.hls?.variants || [];
    $("sheet-variants").hidden = variants.length === 0;
    $("sheet-variant-list").replaceChildren(...variants.map((v, i) => {
      const label = v.resolution ? `${v.resolution.split("x")[1]}p` : `${Math.round(v.bandwidth / 1000)} kbit/s`;
      return el("label", {}, el("input", { type: "radio", name: "variant", value: v.url, checked: i === 0 }), el("span", { text: label }));
    }));
    renderFormats(probe, name);
    $("sheet-folder").textContent = sheetFolder;
    sheetShow("sheet-file");
    $("sheet-start").focus();
  } catch (e) {
    $("sheet-error-text").textContent = errorText(e);
    sheetShow("sheet-error");
    $("sheet-retry").focus();
  }
}

async function startFromSheet(start) {
  const variant = document.querySelector('#sheet-variant-list input:checked')?.value || null;
  const name = $("sheet-name").value.trim();
  const format = $("sheet-format").hidden ? null : chosenFormat();
  try {
    const id = await invoke("add", { args: { source: sheetSource, probe: sheetProbe, file_name: name || null, folder: sheetFolder || null, start, variant, media: format?.media || null, convert: format?.convert || null } });
    $("sheet").close();
    announce(t(start === "now" ? "announce.started" : "announce.later", { name: name || sheetProbe?.file_name || "" }));
    if (start === "now") { if (views.has(id)) select(id); else pendingSelect = id; }
    nextHandoff();
  } catch (e) {
    fail(e);
  }
}

function laterMode() {
  return settings?.scheduler?.enabled ? "queue" : "later";
}

$("sheet-form").addEventListener("submit", (e) => { e.preventDefault(); startFromSheet("now"); });
$("sheet-later").addEventListener("click", () => startFromSheet(laterMode()));
$("sheet-retry").addEventListener("click", () => addLink(sheetSource));
for (const b of document.querySelectorAll(".sheet-cancel")) b.addEventListener("click", () => { $("sheet").close(); nextHandoff(); });
$("sheet-folder-change").addEventListener("click", async () => {
  const picked = await invoke("pick_folder", { current: sheetFolder || null });
  if (picked) { sheetFolder = picked; $("sheet-folder").textContent = picked; }
});

// Several links at once (from the browser's "Download all links").
let linkBatch = [];
function openLinks(items) {
  linkBatch = items;
  $("sheet-links-title").textContent = t("links.title", { n: items.length });
  $("sheet-link-list").replaceChildren(...items.map((it, i) => el("li", {},
    el("input", { type: "checkbox", id: `lk-${i}`, checked: true }),
    el("label", { for: `lk-${i}`, text: it.file_name || it.url, title: it.url }))));
  sheetShow("sheet-links");
  openSheet();
  $("links-start").focus();
}

async function startLinks(start) {
  const chosen = linkBatch.filter((_, i) => $(`lk-${i}`)?.checked);
  for (const it of chosen) {
    await invoke("add", { args: { source: { url: it.url, referrer: it.referrer || null, cookies: it.cookies || null, user_agent: it.user_agent || null }, file_name: it.file_name || null, start } }).catch(fail);
  }
  $("sheet").close();
  announce(t("announce.many", { n: chosen.length }));
  nextHandoff();
}
$("links-start").addEventListener("click", () => startLinks("now"));
$("links-later").addEventListener("click", () => startLinks(laterMode()));

function isUrl(text) {
  return /^https?:\/\/\S+$/i.test(text.trim());
}

$("add-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const text = $("add-url").value.trim();
  if (!text) return;
  if (/\[[^\]]+-[^\]]+\]/.test(text)) { openBatch(text); return; }
  if (!isUrl(text)) { toast({ title: t("add.invalid"), body: text, kind: "other" }); return; }
  $("add-url").value = "";
  addLink({ url: text });
});

// A numbered series.
let batchUrls = [];
function openBatch(text = "") {
  $("batch-pattern").value = text;
  previewBatch();
  $("batch").showModal();
  $("batch-pattern").focus();
}
async function previewBatch() {
  const p = $("batch-pattern").value.trim();
  const out = $("batch-preview");
  batchUrls = [];
  if (!p) { out.textContent = ""; out.className = "batch-preview"; return; }
  try {
    batchUrls = await invoke("expand_batch", { pattern: p });
    out.className = "batch-preview";
    out.textContent = batchUrls.length > 1
      ? t("batch.preview", { n: batchUrls.length, first: batchUrls[0].split("/").pop(), last: batchUrls.at(-1).split("/").pop() })
      : t("batch.single");
  } catch (e) {
    out.className = "batch-preview bad";
    out.textContent = reason(e);
  }
}
$("batch-pattern").addEventListener("input", previewBatch);
async function startBatch(start) {
  await previewBatch();
  if (!batchUrls.length) return;
  const n = await invoke("add_batch", { urls: batchUrls, start }).catch(fail);
  $("batch").close();
  $("add-url").value = "";
  if (n) announce(t("announce.many", { n }));
}
$("batch-form").addEventListener("submit", (e) => { e.preventDefault(); startBatch("now"); });
$("batch-later").addEventListener("click", () => startBatch(laterMode()));
$("batch-open").addEventListener("click", () => openBatch($("add-url").value.trim()));
for (const b of document.querySelectorAll(".batch-cancel")) b.addEventListener("click", () => $("batch").close());

// Hand-offs from the browser, one at a time.
let handoffQueue = [];
async function takeHandoffs() {
  const got = await invoke("take_handoffs");
  handoffQueue.push(...got);
  if (!$("sheet").open) nextHandoff();
}
function nextHandoff() {
  const h = handoffQueue.shift();
  if (!h) return;
  if (h.kind === "links" && h.items.length > 1) { openLinks(h.items); return; }
  const it = h.items[0];
  addLink({ url: it.url, referrer: it.referrer || null, cookies: it.cookies || null, user_agent: it.user_agent || null, file_name: it.file_name || null });
}
listen("handoff", takeHandoffs);
listen("refreshed", ({ payload }) => toast({ title: payload.ok ? t("refresh.done") : t("refresh.failed"), body: views.get(payload.id)?.name || "", kind: "other" }));

// ---- refresh, remove --------------------------------------------------------------------------

let refreshId = null;
function openRefresh(id) {
  refreshId = id;
  $("refresh-url").value = "";
  $("refresh-result").textContent = "";
  $("refresh").showModal();
  $("refresh-url").focus();
}
$("refresh-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const url = $("refresh-url").value.trim();
  if (!isUrl(url)) { $("refresh-result").textContent = t("add.invalid"); return; }
  $("refresh-result").textContent = t("sheet.asking");
  try {
    await invoke("refresh", { id: refreshId, source: { url } });
    $("refresh").close();
    announce(t("refresh.done"));
  } catch (err) {
    $("refresh-result").textContent = errorText(err) || reason(err);
  }
});
for (const b of document.querySelectorAll(".refresh-cancel")) b.addEventListener("click", () => $("refresh").close());

let removeId = null;
function confirmRemove(id) {
  const v = views.get(id);
  if (!v) return;
  removeId = id;
  $("confirm-title").textContent = t("remove.title", { name: v.name });
  $("confirm-delete").checked = false;
  $("confirm-delete-row").hidden = v.status !== "done";
  $("confirm").showModal();
  $("confirm").querySelector(".confirm-cancel").focus();
}
$("confirm-form").addEventListener("submit", (e) => {
  e.preventDefault();
  invoke("remove", { id: removeId, deleteFile: $("confirm-delete").checked });
  $("confirm").close();
  if (selected === removeId) closeDeck();
  announce(t("announce.removed"));
});
for (const b of document.querySelectorAll(".confirm-cancel")) b.addEventListener("click", () => $("confirm").close());

$("pause-all").addEventListener("click", () => invoke("pause_all"));
$("resume-all").addEventListener("click", () => invoke("resume_all"));
$("clear-done").addEventListener("click", () => invoke("clear_finished"));

// ---- toasts -----------------------------------------------------------------------------------

function toast({ title, body, kind = "other", actions = [], timeout = 9000 }) {
  const g = el("span", { class: "glyph", "aria-hidden": "true" });
  glyph(g, kind);
  const box = el("div", { class: "toast", role: "status" }, g, el("div", {},
    el("h3", { text: title }), el("p", { text: body, title: body }),
    actions.length ? el("div", { class: "toast-actions" }, ...actions.map(([label, run, cls]) => el("button", { type: "button", class: cls || "secondary", onclick: () => { box.remove(); run?.(); } }, label))) : null));
  $("toasts").append(box);
  if (timeout) window.setTimeout(() => box.remove(), timeout);
  return box;
}

function fail(e) {
  toast({ title: t("error.title"), body: errorText(e) || reason(e), kind: "other" });
}

listen("clipboard", ({ payload }) => {
  const kind = kindOf(payload.name);
  toast({
    title: t("clipboard.title"),
    body: payload.name || payload.url,
    kind,
    timeout: 12000,
    actions: [[t("clipboard.add"), () => addLink({ url: payload.url }), "primary"], [t("clipboard.dismiss"), null, "ghost"]],
  });
});

function kindOf(name) {
  const ext = (name || "").split(".").pop().toLowerCase();
  const map = { video: "mp4 mkv webm avi mov wmv flv m4v ts m3u8", music: "mp3 flac wav aac m4a ogg opus", documents: "pdf doc docx xls xlsx ppt pptx odt txt epub", compressed: "zip rar 7z tar gz xz bz2 zst", programs: "exe msi iso dmg pkg deb rpm appimage apk", images: "jpg jpeg png gif webp avif svg" };
  return Object.entries(map).find(([, list]) => list.split(" ").includes(ext))?.[0] || "other";
}

// ---- settings -----------------------------------------------------------------------------------

function options(select, values, label, current) {
  select.replaceChildren(...values.map((v) => el("option", { value: String(v), text: label(v), selected: v === current })));
}

let settingsFolder = "";
async function openSettings() {
  const s = settings;
  options($("s-connections"), [1, 2, 4, 6, 8, 12, 16, 24, 32], String, s.engine.connections);
  options($("s-active"), [1, 2, 3, 4, 5, 6, 8, 10], String, s.engine.max_active);
  options($("s-limit"), [0, 512 * 1024, 1 << 20, 2 << 20, 5 << 20, 10 << 20, 20 << 20, 50 << 20], (l) => (l ? speedText(l) : t("limit.none")), s.engine.speed_limit);
  settingsFolder = s.folder;
  $("s-folder").textContent = s.folder || downloadsDir;
  $("s-sort").checked = s.sort_by_category;
  $("s-ignore").checked = s.engine.ignore_changes;
  $("s-format").value = s.ask_format ? "ask" : "original";
  $("s-clipboard").checked = s.watch_clipboard;
  $("s-language").value = s.language;
  $("s-theme").value = s.theme;
  $("s-tray").checked = s.close_to_tray;
  $("s-notify").checked = s.notify;
  renderBrowsers(await invoke("browsers"));
  $("settings").showModal();
}

const BROWSER_NAMES = { chrome: "Chrome", edge: "Edge", brave: "Brave", vivaldi: "Vivaldi", chromium: "Chromium", firefox: "Firefox", opera: "Opera" };

function renderBrowsers(status) {
  const list = status.browsers || [];
  const names = BROWSER_NAMES;
  $("s-browsers").replaceChildren(...list.map((b) => el("li", { class: b.connected ? "on" : "" },
    el("span", { class: "dot", "aria-hidden": "true" }),
    el("span", { text: names[b.browser] || b.browser }),
    el("span", { class: "sr-only", text: b.connected ? t("browser.connected") : t("browser.not") }))));
}

$("s-connect").addEventListener("click", async () => {
  try { renderBrowsers(await invoke("connect_browsers")); announce(t("browser.done")); } catch (e) { fail(e); }
});
$("s-disconnect").addEventListener("click", async () => renderBrowsers(await invoke("disconnect_browsers")));
$("s-setup").addEventListener("click", () => { $("settings").close(); openConnect(); });

// ---- connecting a browser --------------------------------------------------------

let connectTimer = 0;
async function refreshConnect() {
  const status = await invoke("browsers");
  const hosts = (status.browsers || []).filter((b) => b.connected).map((b) => BROWSER_NAMES[b.browser] || b.browser);
  const seen = Object.entries(status.seen || {}).sort((a, b) => b[1].at - a[1].at);
  $("step-host").className = hosts.length || seen.length ? "done" : "";
  $("step-host-text").textContent = hosts.length ? t("connect.host.yes", { list: hosts.join(", ") }) : seen.length ? t("connect.host.yes", { list: BROWSER_NAMES[seen[0][0]] || seen[0][0] }) : t("connect.host.no");
  $("step-ext").className = seen.length ? "done" : "";
  $("step-seen").className = seen.length ? "done" : "waiting";
  $("step-seen-text").textContent = seen.length
    ? t("connect.seen.yes", { browser: BROWSER_NAMES[seen[0][0]] || seen[0][0] })
    : t("connect.seen.wait");
}

async function openConnect() {
  if (!settings.browser_integration) await invoke("connect_browsers").catch(() => {});
  await refreshConnect();
  if (!$("connect").open) $("connect").showModal();
  window.clearInterval(connectTimer);
  connectTimer = window.setInterval(() => { if ($("connect").open) refreshConnect(); }, 1500);
}

for (const b of document.querySelectorAll("#connect [data-browser]")) {
  b.addEventListener("click", async () => {
    try {
      await invoke("add_extension", { browser: b.dataset.browser });
      announce(t("connect.opened", { browser: b.textContent }));
    } catch (e) {
      fail(e);
    }
  });
}
$("connect").addEventListener("close", async () => {
  window.clearInterval(connectTimer);
  if (!settings.onboarded) {
    settings = await invoke("save_settings", { settings: { ...settings, onboarded: true } });
  }
});
for (const b of document.querySelectorAll(".connect-close")) b.addEventListener("click", () => $("connect").close());
$("s-folder-change").addEventListener("click", async () => {
  const picked = await invoke("pick_folder", { current: settingsFolder || downloadsDir });
  if (picked) { settingsFolder = picked; $("s-folder").textContent = picked; }
});

$("settings-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const next = structuredClone(settings);
  next.engine.connections = Number($("s-connections").value);
  next.engine.max_active = Number($("s-active").value);
  next.engine.speed_limit = Number($("s-limit").value);
  next.engine.ignore_changes = $("s-ignore").checked;
  next.ask_format = $("s-format").value === "ask";
  next.folder = settingsFolder;
  next.sort_by_category = $("s-sort").checked;
  next.watch_clipboard = $("s-clipboard").checked;
  next.language = $("s-language").value;
  next.theme = $("s-theme").value;
  next.close_to_tray = $("s-tray").checked;
  next.notify = $("s-notify").checked;
  settings = await invoke("save_settings", { settings: next });
  $("settings").close();
  applyTheme();
  await loadStrings();
  render();
  announce(t("settings.saved"));
});
for (const b of document.querySelectorAll(".settings-cancel")) b.addEventListener("click", () => $("settings").close());
$("settings-open").addEventListener("click", openSettings);
$("empty-browsers").addEventListener("click", () => openConnect());

// ---- the scheduler ------------------------------------------------------------------------------

function openScheduler() {
  const q = settings.scheduler;
  $("q-enabled").checked = q.enabled;
  $("q-start").value = q.start;
  $("q-stop").value = q.stop;
  $("q-after").value = q.after;
  const names = [...Array(7).keys()].map((d) => new Intl.DateTimeFormat(lang, { weekday: "short" }).format(new Date(2026, 10, 16 + d)));
  $("q-days").replaceChildren(...names.map((n, d) => el("label", {}, el("input", { type: "checkbox", value: String(d), checked: q.days.includes(d) }), el("span", { text: n }))));
  $("scheduler").showModal();
}
$("scheduler-open").addEventListener("click", openScheduler);
for (const b of document.querySelectorAll(".scheduler-cancel")) b.addEventListener("click", () => $("scheduler").close());
$("scheduler-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const next = structuredClone(settings);
  next.scheduler = {
    enabled: $("q-enabled").checked,
    start: $("q-start").value || "02:00",
    stop: $("q-stop").value || "",
    days: [...document.querySelectorAll("#q-days input:checked")].map((i) => Number(i.value)),
    after: $("q-after").value,
  };
  settings = await invoke("save_settings", { settings: next });
  $("scheduler").close();
  tickScheduler();
  announce(t("settings.saved"));
});

/// Should the queue run now? Times are the person's own clock.
function inWindow(q, now = new Date()) {
  const day = (now.getDay() + 6) % 7;
  const hm = `${String(now.getHours()).padStart(2, "0")}:${String(now.getMinutes()).padStart(2, "0")}`;
  if (!q.stop) return q.days.includes(day) && hm >= q.start;
  if (q.start <= q.stop) return q.days.includes(day) && hm >= q.start && hm < q.stop;
  // Overnight: 23:00 to 06:00 belongs to the day it started.
  const yesterday = (day + 6) % 7;
  return (q.days.includes(day) && hm >= q.start) || (q.days.includes(yesterday) && hm < q.stop);
}

async function tickScheduler() {
  if (!settings) return;
  const q = settings.scheduler;
  const want = !q.enabled || inWindow(q);
  if (want !== settings.engine.queue_running) {
    settings.engine.queue_running = want;
    await invoke("set_queue_running", { running: want });
    if (q.enabled && want) scheduledRun = true;
  }
}
window.setInterval(tickScheduler, 20000);

listen("queue-finished", () => {
  const q = settings?.scheduler;
  if (!q?.enabled || !scheduledRun || q.after === "nothing") return;
  scheduledRun = false;
  let left = 60;
  $("countdown-title").textContent = t(q.after === "shutdown" ? "countdown.shutdown" : "countdown.sleep");
  const tick = () => { $("countdown-text").textContent = duration(left); };
  tick();
  $("countdown").showModal();
  const timer = window.setInterval(() => {
    left -= 1;
    tick();
    if (left <= 0) {
      window.clearInterval(timer);
      $("countdown").close();
      invoke("power", { action: q.after }).catch(fail);
    }
  }, 1000);
  $("countdown").addEventListener("close", () => window.clearInterval(timer), { once: true });
});

// ---- the window ---------------------------------------------------------------------------------

$("win-min").addEventListener("click", () => appWindow.minimize());
$("win-max").addEventListener("click", () => appWindow.toggleMaximize());
$("win-close").addEventListener("click", () => appWindow.close());

document.addEventListener("paste", (e) => {
  const target = e.target;
  if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) return;
  if (document.querySelector("dialog[open]")) return;
  const text = (e.clipboardData?.getData("text") || "").trim();
  if (isUrl(text)) { e.preventDefault(); addLink({ url: text }); }
});

document.addEventListener("keydown", (e) => {
  if (document.querySelector("dialog[open]")) return;
  const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement?.tagName || "");
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "n") { e.preventDefault(); $("add-url").focus(); return; }
  if ((e.ctrlKey || e.metaKey) && e.key === ",") { e.preventDefault(); openSettings(); return; }
  if (typing) return;
  const shown = [...$("list").children];
  const index = shown.findIndex((n) => n.dataset.id === selected);
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const next = shown[Math.max(0, Math.min(shown.length - 1, index + (e.key === "ArrowDown" ? 1 : -1)))];
    if (next) select(next.dataset.id, true);
  } else if (e.key === "Escape" && selected) {
    closeDeck();
  } else if (selected && e.key === " ") {
    e.preventDefault();
    const v = views.get(selected);
    if (v?.status === "downloading") invoke("pause", { id: selected });
    else if (v && v.status !== "done") invoke("start_now", { id: selected });
  } else if (selected && e.key === "Delete") {
    confirmRemove(selected);
  } else if (selected && e.key === "Enter") {
    const v = views.get(selected);
    if (v?.status === "done") invoke("open_file", { id: selected }).catch(fail);
  }
});

window.addEventListener("resize", () => { scheduleLanes(); drawEmpty(); for (const n of $("list").children) { const v = views.get(n.dataset.id); if (v) updateRow(n, v); } });

listen("snapshot", ({ payload }) => apply(payload));

async function start() {
  const b = await invoke("boot");
  settings = b.settings;
  downloadsDir = b.downloads;
  applyTheme();
  await loadStrings();
  $("about").textContent = t("about", { version: b.version });
  apply(b.snapshot);
  drawEmpty();
  tickScheduler();
  await takeHandoffs();
  // First start: show how to catch downloads from the browser, once.
  if (!settings.onboarded && !document.querySelector("dialog[open]")) await openConnect();
  if (!document.querySelector("dialog[open]")) $("add-url").focus();
  document.body.dataset.ready = "1";
}
start();
