// DOM helpers. Everything the workspace shows from a repository goes in as text: there is no
// HTML string anywhere in this code, and `cargo xtask vendor-check` refuses one.

import { displayPath } from "./bytes.js";

/**
 * Builds an element. `attrs` keys: `class`, `text`, `title`, `on<event>` handlers, `aria-*`,
 * `data-*`, and plain attributes. Children are elements or strings (added as text nodes).
 */
export function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") el.className = v;
    else if (k === "text") el.textContent = v;
    else if (k.startsWith("on") && typeof v === "function") el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(typeof c === "string" ? document.createTextNode(c) : c);
  }
  return el;
}

/** A Lucide icon from the page's own sprite is not available here, so icons are small inline SVGs built node by node. */
export function icon(name) {
  const NS = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("class", "ws-icon");
  svg.setAttribute("aria-hidden", "true");
  for (const d of ICONS[name] || []) {
    const p = document.createElementNS(NS, "path");
    p.setAttribute("d", d);
    svg.append(p);
  }
  return svg;
}

// Lucide icon paths (ISC licence, https://lucide.dev), the same set Codehaven's pages use.
const ICONS = {
  files: ["M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z", "M14 2v4a2 2 0 0 0 2 2h4"],
  search: ["m21 21-4.34-4.34", "M11 3a8 8 0 1 0 0 16 8 8 0 0 0 0-16Z"],
  changes: ["M6 3v12", "M18 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z", "M6 21a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z", "M15 6a9 9 0 0 0-9 9"],
  runs: ["M6 3 20 12 6 21Z"],
  branch: ["M6 3v12", "M18 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z", "M6 21a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z", "M15 6a9 9 0 0 0-9 9"],
  x: ["M18 6 6 18", "m6 6 12 12"],
  folder: ["M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"],
  file: ["M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"],
  plus: ["M5 12h14", "M12 5v14"],
  check: ["M20 6 9 17l-5-5"],
  alert: ["M12 9v4", "M12 17h.01", "M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0Z"],
  back: ["m12 19-7-7 7-7", "M19 12H5"],
  refresh: ["M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8", "M21 3v5h-5", "M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16", "M8 16H3v5"],
  stop: ["M6 6h12v12H6Z"],
  terminal: ["m4 17 6-6-6-6", "M12 19h8"],
};

/** A path for display: escapes for control and bidi characters, in an isolated span. */
export function pathText(p, cls) {
  return h("bdi", { class: cls || "ws-path", dir: "ltr", title: displayPath(p) }, displayPath(p));
}

/** Removes every child. */
export function clear(el) {
  while (el.firstChild) el.firstChild.remove();
  return el;
}

/** A small live region for results a screen reader should hear. */
let liveEl = null;
export function announce(text) {
  if (!liveEl) {
    liveEl = h("div", { class: "ws-sr", "aria-live": "polite", role: "status" });
    document.body.append(liveEl);
  }
  liveEl.textContent = "";
  setTimeout(() => { liveEl.textContent = text; }, 30);
}

export function button(label, onclick, opts) {
  const o = opts || {};
  return h("button", { type: "button", class: o.class || "ws-btn", title: o.title, "aria-label": o.aria || null, disabled: o.disabled || null, onclick },
    o.icon ? icon(o.icon) : null, label ? h("span", { text: label }) : null);
}

/** A modal dialog with a title, a body and buttons. Resolves with the chosen value. */
export function dialog(title, body, buttons) {
  return new Promise((resolve) => {
    const d = h("dialog", { class: "ws-dialog", "aria-label": title });
    const done = (v) => { d.close(); d.remove(); resolve(v); };
    d.append(
      h("h2", { class: "ws-dialog-title", text: title }),
      body,
      h("div", { class: "ws-dialog-actions" }, buttons.map((b) => button(b.label, () => done(b.value), { class: b.primary ? "ws-btn ws-btn-primary" : b.danger ? "ws-btn ws-btn-danger" : "ws-btn" }))),
    );
    d.addEventListener("cancel", (e) => { e.preventDefault(); done(null); });
    document.body.append(d);
    d.showModal();
  });
}
