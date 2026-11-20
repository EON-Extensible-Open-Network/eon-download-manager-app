// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors

"use strict";

const api = globalThis.browser ?? globalThis.chrome;
const msg = (key, sub) => api.i18n.getMessage(key, sub);
for (const n of document.querySelectorAll("[data-m]")) n.textContent = msg(n.dataset.m);
document.documentElement.lang = api.i18n.getUILanguage().startsWith("tr") ? "tr" : "en";

api.runtime.sendMessage({ type: "popup" }).then((state) => {
  const status = document.getElementById("status");
  status.textContent = state.version ? msg("popup_connected", [state.version]) : msg("popup_missing");
  status.classList.toggle("bad", !state.version);
  const box = document.getElementById("enabled");
  box.checked = state.settings.enabled;
  box.addEventListener("change", () => api.runtime.sendMessage({ type: "set", values: { enabled: box.checked } }));
  const list = document.getElementById("media");
  if (!state.media.length) {
    const li = document.createElement("li");
    li.className = "none";
    li.textContent = msg("popup_none");
    list.append(li);
    return;
  }
  state.media.forEach((m, i) => {
    const li = document.createElement("li");
    const b = document.createElement("button");
    b.type = "button";
    const name = decodeURIComponent(new URL(m.url).pathname.split("/").pop() || m.url);
    b.textContent = `${m.type === "hls" ? "HLS" : "▶"}  ${name}`;
    b.title = m.url;
    b.addEventListener("click", () => { api.runtime.sendMessage({ type: "popup-media", tab: state.tab, index: i }); window.close(); });
    li.append(b);
    list.append(li);
  });
});
