// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// In the page: notices Alt+click (leave this download to the browser), lists
// links when the menu asks, and shows the "Download video" button over the
// player when the page has loaded a video. The button lives in a closed shadow
// root so the page's styles and scripts cannot reach it.

"use strict";

(() => {
  const api = globalThis.browser ?? globalThis.chrome;

  document.addEventListener("click", (e) => {
    if (e.altKey) api.runtime.sendMessage({ type: "alt-click" }).catch(() => {});
  }, true);

  function links(scope) {
    if (scope === "selection") {
      const sel = window.getSelection();
      if (!sel || sel.isCollapsed) return [];
      const out = [];
      for (let i = 0; i < sel.rangeCount; i++) {
        const frag = sel.getRangeAt(i).cloneContents();
        for (const a of frag.querySelectorAll("a[href]")) out.push(new URL(a.getAttribute("href"), location.href).href);
        const anchor = sel.getRangeAt(i).commonAncestorContainer.parentElement?.closest("a[href]");
        if (anchor) out.push(anchor.href);
      }
      return out;
    }
    return [...document.querySelectorAll("a[href]")].map((a) => a.href);
  }

  // ---- the video button ----------------------------------------------------------
  let host = null;
  let button = null;
  let count = 0;

  function ensure() {
    if (host) return;
    host = document.createElement("eon-download-manager");
    const root = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = `
      button {
        all: initial; position: fixed; z-index: 2147483647; display: flex; align-items: center; gap: 8px;
        font: 600 13px/1 system-ui, sans-serif; color: #15132b; background: #a08bff;
        padding: 9px 13px 9px 10px; border-radius: 999px; cursor: pointer;
        box-shadow: 0 8px 24px rgba(10, 8, 30, .45), 0 0 0 1px rgba(255, 255, 255, .25) inset;
        opacity: 0; transform: translateY(-4px); transition: opacity .16s, transform .16s;
      }
      button.on { opacity: 1; transform: none; }
      button:hover { filter: brightness(1.08); }
      button:focus-visible { outline: 2px solid #4fd8e8; outline-offset: 2px; }
      svg { width: 16px; height: 16px; }
      @media (prefers-reduced-motion: reduce) { button { transition: none; } }
    `;
    button = document.createElement("button");
    button.type = "button";
    const ns = "http://www.w3.org/2000/svg";
    const svg = document.createElementNS(ns, "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("aria-hidden", "true");
    const path = document.createElementNS(ns, "path");
    path.setAttribute("d", "M12 4v12m0 0-5-5m5 5 5-5M5 20h14");
    path.setAttribute("fill", "none");
    path.setAttribute("stroke", "currentColor");
    path.setAttribute("stroke-width", "2.2");
    path.setAttribute("stroke-linecap", "round");
    path.setAttribute("stroke-linejoin", "round");
    svg.append(path);
    const label = document.createElement("span");
    button.append(svg, label);
    button.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      api.runtime.sendMessage({ type: "download-media", index: -1 }).catch(() => {});
      button.classList.remove("on");
    });
    root.append(style, button);
    document.documentElement.append(host);
  }

  function biggestVideo() {
    let best = null;
    let area = 0;
    for (const v of document.querySelectorAll("video")) {
      const r = v.getBoundingClientRect();
      const a = r.width * r.height;
      if (a > area && r.width > 200 && r.height > 120 && r.bottom > 0 && r.top < innerHeight) { best = v; area = a; }
    }
    return best;
  }

  function place() {
    if (!count) return;
    const video = biggestVideo();
    if (!video) { button?.classList.remove("on"); return; }
    ensure();
    const r = video.getBoundingClientRect();
    button.querySelector("span").textContent = count > 1 ? api.i18n.getMessage("pill_many", [String(count)]) : api.i18n.getMessage("pill");
    button.style.top = `${Math.max(8, r.top + 12)}px`;
    button.style.left = `${Math.max(8, r.right - button.offsetWidth - 12)}px`;
    button.classList.add("on");
  }

  addEventListener("scroll", place, { passive: true });
  addEventListener("resize", place);
  document.addEventListener("play", place, true);

  api.runtime.onMessage.addListener((message, _sender, reply) => {
    if (message.type === "links") { reply(links(message.scope)); return false; }
    if (message.type === "media") { count = message.count; place(); }
    return false;
  });
})();
