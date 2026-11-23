// The terminal panel (spec sections 7 and 11): a shell that runs in this browser, in a Worker
// inside a sandboxed frame with no network and no session. This file is the page's side.
//
// - The frame is created only when the panel opens, and nothing for it loads before then.
// - It gets one MessagePort, after exactly one `ready` from exactly that frame with origin
//   "null". A second `ready`, a second load, a burst or a message that does not fit destroys it.
// - The shell's bytes are fetched here, checked against this build's hashes, and handed over as
//   buffers. No URL, cookie, key or name ever crosses the port.
// - Nothing from the frame can fetch, navigate, commit or run. Moving its files into drafts is a
//   review where each file is opened and ticked by hand, and committing them is a second click.

import { h, clear, button, icon, pathText, announce, dialog } from "./ui.js";
import { inspect, encode, toB64, longestLine, sameBytes } from "./bytes.js";
import { LONG_LINE } from "./session.js";

const WARNING = "Runs in this browser. No internet. Not your CI machine. Codehaven never asks for a password here.";
const MAX_FILES = 2000;
const MAX_BYTES = 25 * 1024 * 1024;
const MAX_RATE = 20;
const MAX_CHANGES = 500;
const STATES = new Set(["starting", "ready", "running", "stopped", "failed"]);
const MAGIC = "CHSNAP1\n";

/**
 * A path the terminal may hand back: relative, plain segments, no controls or bidi marks, no
 * backslash, no spelling of `.git`, and short enough for git.
 */
export function termPathProblem(p) {
  if (typeof p !== "string" || !p) return "This file has no name.";
  if (new TextEncoder().encode(p).length > 4096) return "This name is too long.";
  if (p.startsWith("/")) return "This name starts with a slash.";
  if (/[\u0000-\u001f\u007f-\u009f\\؜‎‏‪-‮⁦-⁩]/.test(p)) return "This name has control characters or a backslash in it.";
  for (const seg of p.split("/")) {
    if (seg === "" || seg === "." || seg === "..") return "This name has an empty, . or .. part.";
    if (new TextEncoder().encode(seg).length > 255) return "A part of this name is too long.";
    // What HFS and NTFS fold to .git: ignorable characters gone, case, trailing dots and spaces.
    const folded = seg.replace(/[​-‍⁪-⁯﻿]/g, "").toLowerCase().replace(/[. ]+$/, "");
    if (folded === ".git" || folded.startsWith(".git:") || folded === "git~1") return "Files inside .git cannot be changed.";
  }
  return "";
}

/** The workspace's colours for xterm, from the page tokens. */
function theme() {
  const css = getComputedStyle(document.documentElement);
  const v = (n, d) => {
    const s = css.getPropertyValue(n).trim();
    return /^#[0-9a-fA-F]{6}$/.test(s) ? s : d;
  };
  const bg = v("--surface", "#ffffff");
  const dark = parseInt(bg.slice(1, 3), 16) + parseInt(bg.slice(3, 5), 16) + parseInt(bg.slice(5, 7), 16) < 384;
  const ansi = dark
    ? { black: "#1a2428", red: "#ff8f80", green: "#4ade80", yellow: "#facc15", blue: "#7cb7ff", magenta: "#f0a6f0", cyan: "#2dd4bf", white: "#e6edef" }
    : { black: "#1b2326", red: "#b42318", green: "#15803d", yellow: "#a16207", blue: "#1d4ed8", magenta: "#a21caf", cyan: "#0f766e", white: "#5b676b" };
  return { background: bg, foreground: v("--text", "#1b2326"), cursor: v("--accent", "#0f766e"), selectionBackground: v("--accent-soft", "#e0f2ef"), ...ansi };
}

/** Reads a snapshot body: magic, count, then flags, path, size and bytes for each file. */
export function parseSnapshot(buf) {
  const bytes = new Uint8Array(buf);
  if (bytes.length < 12 || new TextDecoder().decode(bytes.subarray(0, 8)) !== MAGIC) throw new Error("The files did not arrive whole.");
  const view = new DataView(buf);
  const count = view.getUint32(8);
  const out = new Map();
  let at = 12;
  const dec = new TextDecoder("utf-8", { fatal: true });
  for (let i = 0; i < count; i++) {
    if (at + 5 > bytes.length) throw new Error("The files did not arrive whole.");
    const exec = bytes[at] === 1;
    const plen = view.getUint32(at + 1);
    at += 5;
    if (at + plen + 4 > bytes.length) throw new Error("The files did not arrive whole.");
    const path = dec.decode(bytes.subarray(at, at + plen));
    at += plen;
    const size = view.getUint32(at);
    at += 4;
    if (at + size > bytes.length) throw new Error("The files did not arrive whole.");
    out.set(path, { data: bytes.slice(at, at + size), exec });
    at += size;
  }
  if (at !== bytes.length) throw new Error("The files did not arrive whole.");
  return out;
}

export function mountTerminal(session, layout, editor) {
  const el = layout.termEl;
  let frame = null;
  let port = null;
  let readySeen = false;
  let loads = 0;
  let ackTimer = 0;
  let windowStart = 0;
  let windowCount = 0;
  let state = "stopped";
  let guarded = false;
  let leaving = false;
  let folder = ""; // the folder the terminal holds, "" for the whole repository
  // What the terminal was given, by path: the snapshot's base bytes, and what was pushed since.
  let base = new Map();
  let given = new Map();
  let pushTimer = 0;
  let review = null; // the open review: { items: Map(path -> item) }
  let scanAsked = false; // a review list is accepted only in answer to our own request
  let lastChanges = null;

  // ---------------------------------------------------------------- the panel

  const title = h("h2", { class: "ws-term-title", text: "Terminal" });
  const stateEl = h("span", { class: "ws-term-state", role: "status" });
  const reviewBtn = button("Review terminal changes", () => askReview(), { icon: "changes" });
  // Stop ends the running line; typing goes straight back to the terminal.
  const stopBtn = button("Stop", () => { if (port) { send({ type: "stop" }); if (frame) frame.focus(); } }, { icon: "stop", title: "Stop the running command" });
  const closeBtn = button("", () => close(), { class: "ws-btn ws-icon-only", icon: "x", aria: "Close the terminal", title: "Close the terminal" });
  const tray = h("section", { class: "ws-term-tray", "aria-label": "Terminal changes", hidden: true });
  const box = h("div", { class: "ws-term-box" });
  const warn = h("p", { class: "ws-term-warn" }, icon("alert"), h("span", { text: WARNING }));
  const grip = h("div", { class: "ws-term-grip", title: "Drag to resize", onpointerdown: startGrip });
  clear(el).append(grip,
    h("div", { class: "ws-term-head" }, title, stateEl, h("span", { class: "ws-top-spacer" }), reviewBtn, stopBtn, closeBtn),
    tray, box, warn);
  el.hidden = true;

  function startGrip(e) {
    e.preventDefault();
    const startY = e.clientY;
    const startH = el.getBoundingClientRect().height;
    const move = (ev) => {
      const px = Math.min(innerHeight * 0.75, Math.max(160, Math.round(startH + (startY - ev.clientY))));
      el.style.height = px + "px";
    };
    const up = () => { removeEventListener("pointermove", move); removeEventListener("pointerup", up); };
    addEventListener("pointermove", move);
    addEventListener("pointerup", up);
  }

  function setState(s, note) {
    state = s;
    const words = { starting: "Starting", ready: "Ready", running: "Running", stopped: "Stopped", failed: "Stopped" };
    stateEl.textContent = note || words[s] || "";
    stopBtn.disabled = s !== "running" || null;
    reviewBtn.disabled = !(s === "ready" || s === "running") || null;
  }
  setState("stopped", "");

  // ---------------------------------------------------------------- opening and closing

  function isOpen() {
    return !el.hidden;
  }

  async function open() {
    if (!session.boot || !session.boot.terminal || session.locked) return;
    el.hidden = false;
    layout.termShown(true);
    if (!frame) await start();
    else if (frame) frame.focus();
  }

  function close() {
    destroy("");
    el.hidden = true;
    layout.termShown(false);
    const toggle = layout.top.termBtn;
    if (toggle) toggle.focus();
  }

  function toggle() {
    if (!isOpen()) {
      open();
      return;
    }
    // On a phone the terminal is a pane: its tab shows it, and Close is the way to end it.
    if (matchMedia("(max-width: 639.98px)").matches) {
      layout.termShown(true);
      if (frame) frame.focus();
      return;
    }
    if (frame && document.activeElement !== frame) frame.focus();
    else close();
  }

  /** The files the terminal starts with: base bytes under the folder, with drafts on top. */
  async function gather() {
    const blobs = [];
    for (const [p, e] of session.tree) if (e.kind === "blob" && (e.mode === "100644" || e.mode === "100755")) blobs.push([p, e.size || 0]);
    const fits = (f) => {
      let n = 0, b = 0;
      for (const [p, s] of blobs) if (!f || p.startsWith(f + "/")) { n++; b += s; }
      return n <= MAX_FILES && b <= MAX_BYTES;
    };
    folder = "";
    if (!fits("")) {
      const picked = await pickFolder(blobs, fits);
      if (picked === null) return null;
      folder = picked;
    }
    let snap = new Map();
    if (session.head) {
      const params = { branch: session.branch, sha: session.head };
      if (folder) params.path_b64 = toB64(new TextEncoder().encode(folder));
      const r = await session.api.raw("GET", "/snapshot", params);
      snap = parseSnapshot(await r.arrayBuffer());
    }
    base = new Map(snap);
    given = new Map();
    const files = new Map(snap);
    for (const [p, want] of draftState()) {
      if (want === null) files.delete(p);
      else files.set(p, { data: want, exec: files.has(p) ? files.get(p).exec : false });
      given.set(p, want);
    }
    for (const p of [...files.keys()]) if (termPathProblem(p)) files.delete(p);
    return files;
  }

  /** Every drafted path under the folder and its bytes as the editor has them (null: deleted). */
  function draftState() {
    const out = new Map();
    const inside = (p) => !folder || p.startsWith(folder + "/");
    for (const d of session.drafts.values()) {
      if (d.op === "rename" && d.from && inside(d.from)) out.set(d.from, null);
      if (!inside(d.path)) continue;
      out.set(d.path, d.op === "delete" ? null : encode(d.text, d.info));
    }
    return out;
  }

  async function pickFolder(blobs, fits) {
    const folders = new Map();
    for (const [p] of blobs) {
      const parts = p.split("/");
      for (let i = 1; i < parts.length && i <= 3; i++) folders.set(parts.slice(0, i).join("/"), true);
    }
    const ok = [...folders.keys()].filter(fits).sort();
    let picked = null;
    const list = h("div", { class: "ws-list", role: "listbox", "aria-label": "Folders" });
    const input = h("input", { type: "text", class: "ws-input", placeholder: "Filter folders", "aria-label": "Filter folders", autocomplete: "off", spellcheck: "false" });
    const render = () => {
      clear(list);
      const f = input.value.trim().toLowerCase();
      let shown = 0;
      for (const name of ok) {
        if (f && !name.toLowerCase().includes(f)) continue;
        if (++shown > 100) break;
        list.append(h("button", { type: "button", class: "ws-list-item", role: "option", onclick: (e) => { picked = name; e.currentTarget.closest(".ws-dialog").querySelector(".ws-dialog-actions .ws-btn").click(); } },
          icon("folder"), pathText(name)));
      }
      if (!list.firstChild) list.append(h("p", { class: "ws-hint", text: "No folder that fits matches that." }));
    };
    input.addEventListener("input", render);
    render();
    await dialog("Pick a folder for the terminal",
      h("div", { class: "ws-dialog-body" },
        h("p", { class: "ws-hint", text: "This repository has more than 2,000 files or 25 MB. The terminal takes one folder of it." }),
        input, list),
      [{ label: "Cancel", value: null }]);
    return picked;
  }

  async function fetchRuntime() {
    const pieces = session.boot.terminal_runtime || [];
    if (pieces.length !== 2) throw new Error("This build has no shell.");
    const out = [];
    for (const p of pieces) {
      let r;
      try {
        r = await fetch(p.url, { credentials: "same-origin", redirect: "manual", integrity: p.integrity, referrerPolicy: "same-origin" });
      } catch {
        throw new Error("The shell could not be loaded, or it is not the one this server built.");
      }
      if (r.type === "opaqueredirect" || r.status === 401) {
        session.api.lock("signed_out");
        throw new Error("Sign in again.");
      }
      if (!r.ok) throw new Error("The shell could not be loaded.");
      out.push(await r.arrayBuffer());
    }
    return out;
  }

  async function start() {
    setState("starting", "Getting the files");
    let files;
    let runtime;
    try {
      files = await gather();
      if (!files) {
        close();
        return;
      }
      setState("starting", "Loading the shell");
      runtime = await fetchRuntime();
    } catch (e) {
      setState("failed", e.message || "The terminal could not start.");
      return;
    }
    if (!isOpen() || session.locked) return;
    pending = { files, runtime };
    readySeen = false;
    loads = 0;
    // The address is set before the frame is in the page, so it loads exactly once.
    frame = h("iframe", {
      class: "ws-term-frame",
      sandbox: "allow-scripts",
      allow: "clipboard-read 'none'; clipboard-write 'none'",
      referrerpolicy: "no-referrer",
      title: "Terminal",
      src: session.meta.base + "/terminal",
    });
    frame.addEventListener("load", () => {
      loads++;
      if (loads > 1) destroy("The terminal tried to load something else, so it was closed.");
    });
    addEventListener("message", onWindowMessage);
    guard();
    clear(box).append(frame);
    ackTimer = setTimeout(() => destroy("The terminal did not start in this browser."), 15000);
  }
  let pending = null;

  /** Ends the frame, its Worker and its port. The files it had are gone with it. */
  function destroy(reason) {
    clearTimeout(ackTimer);
    clearTimeout(pushTimer);
    removeEventListener("message", onWindowMessage);
    if (port) {
      port.onmessage = null;
      port.close();
    }
    port = null;
    pending = null;
    scanAsked = false;
    if (frame) frame.remove();
    frame = null;
    closeTray();
    unguard();
    clear(box);
    if (reason) {
      setState("failed", "Stopped");
      box.append(h("div", { class: "ws-term-fail", role: "alert" },
        h("p", { class: "ws-hint", text: reason }),
        button("Start again", () => { clear(box); start(); }, { icon: "refresh" })));
    } else setState("stopped", "");
  }

  // ---------------------------------------------------------------- history while the frame lives

  function guard() {
    if (guarded) return;
    guarded = true;
    history.pushState({ wsTerminal: true }, "", location.href);
    addEventListener("popstate", onPop);
  }

  function unguard() {
    if (!guarded) return;
    guarded = false;
    removeEventListener("popstate", onPop);
    if (history.state && history.state.wsTerminal) {
      leaving = true;
      history.back();
      setTimeout(() => { leaving = false; }, 500);
    }
  }

  async function onPop() {
    if (leaving || !guarded) return;
    // Back was pressed (or the frame found a way to move history): stay, and ask.
    history.pushState({ wsTerminal: true }, "", location.href);
    const v = await dialog("Leave the workspace?", h("div", { class: "ws-dialog-body" },
      h("p", { class: "ws-hint", text: "The terminal will stop and its files are lost. Your drafts are kept." })),
      [{ label: "Stay", value: null, primary: true }, { label: "Leave", value: "leave", danger: true }]);
    if (v !== "leave") return;
    removeEventListener("popstate", onPop);
    guarded = false;
    destroy("");
    leaving = true;
    history.go(-2);
  }

  // ---------------------------------------------------------------- the handshake

  function onWindowMessage(e) {
    if (!frame || e.source !== frame.contentWindow) return;
    if (e.origin !== "null" || readySeen || !e.data || e.data.type !== "ready" || Object.keys(e.data).length !== 1) {
      destroy("The terminal sent something unexpected, so it was closed.");
      return;
    }
    readySeen = true;
    const ch = new MessageChannel();
    port = ch.port1;
    port.onmessage = onPortMessage;
    port.onmessageerror = () => destroy("The terminal sent something that could not be read.");
    frame.contentWindow.postMessage("port", "*", [ch.port2]);
  }

  function send(msg, transfer) {
    if (port) port.postMessage(msg, transfer || []);
  }

  function isObj(v) {
    return v !== null && typeof v === "object" && !Array.isArray(v);
  }

  function onPortMessage(e) {
    const now = Date.now();
    if (now - windowStart >= 1000) { windowStart = now; windowCount = 0; }
    if (++windowCount > MAX_RATE) return destroy("The terminal sent too many messages, so it was closed.");
    const m = e.data;
    if (!isObj(m) || typeof m.type !== "string") return destroy("The terminal sent something unexpected, so it was closed.");
    if (m.type === "ack") {
      if (!pending || Object.keys(m).length !== 1) return destroy("The terminal sent something unexpected, so it was closed.");
      clearTimeout(ackTimer);
      const { files, runtime } = pending;
      pending = null;
      const list = [];
      const transfer = [...runtime];
      for (const [p, f] of files) {
        const copy = f.data.slice().buffer;
        list.push({ path: p, data: copy, exec: !!f.exec });
        transfer.push(copy);
      }
      send({ type: "init", runtime, files: list, theme: theme() }, transfer);
      return;
    }
    if (pending) return destroy("The terminal sent something before it started, so it was closed.");
    if (m.type === "state") {
      if (!STATES.has(m.state) || Object.keys(m).length !== 2) return destroy("The terminal sent something unexpected, so it was closed.");
      if (m.state === "failed") return destroy("The shell stopped. Start it again to carry on.");
      setState(m.state);
      return;
    }
    if (m.type === "blur") {
      if (Object.keys(m).length !== 1) return destroy("The terminal sent something unexpected, so it was closed.");
      closeBtn.focus();
      return;
    }
    if (m.type === "changes") {
      const c = readChanges(m);
      if (!c) return destroy("The terminal sent a list of changes that does not fit, so it was closed.");
      if (m.reason === "conflict") {
        if (c.files.length) return destroy("The terminal sent a list of changes that does not fit, so it was closed.");
        announce(c.conflicts.length + " files changed in the editor and the terminal. The terminal kept its copy.");
        lastChanges = c;
        return;
      }
      if (!scanAsked) return destroy("The terminal sent changes nobody asked for, so it was closed.");
      scanAsked = false;
      showTray(c);
      return;
    }
    destroy("The terminal sent something unexpected, so it was closed.");
  }

  /** The `changes` message, checked: types, counts, sizes and no repeated path. */
  function readChanges(m) {
    const keys = ["type", "reason", "files", "conflicts", "cut"];
    if (!Object.keys(m).every((k) => keys.includes(k))) return null;
    if (m.reason !== "scan" && m.reason !== "conflict") return null;
    if (!Array.isArray(m.files) || m.files.length > MAX_CHANGES || !Array.isArray(m.conflicts) || m.conflicts.length > MAX_CHANGES) return null;
    const seen = new Set();
    let total = 0;
    const files = [];
    for (const f of m.files) {
      if (!isObj(f) || !Object.keys(f).every((k) => ["path", "kind", "data", "exec", "added"].includes(k))) return null;
      if (typeof f.path !== "string" || f.path.length > 8192 || seen.has(f.path)) return null;
      if (!["file", "link", "other", "deleted"].includes(f.kind)) return null;
      if (f.kind === "deleted" ? f.data !== null : !(f.data instanceof ArrayBuffer)) return null;
      seen.add(f.path);
      total += f.data ? f.data.byteLength : 0;
      if (total > MAX_BYTES * 2) return null;
      files.push({ path: f.path, kind: f.kind, data: f.data ? new Uint8Array(f.data) : null, added: f.added === true });
    }
    const conflicts = [];
    for (const p of m.conflicts) {
      if (typeof p !== "string" || p.length > 8192) return null;
      conflicts.push(p);
    }
    return { files, conflicts, cut: m.cut === true };
  }

  // ---------------------------------------------------------------- pushing saves in

  session.on("change", () => {
    if (!port || pending) return;
    clearTimeout(pushTimer);
    pushTimer = setTimeout(push, 1000);
  });

  /** Sends the terminal every drafted file that changed since it last heard. */
  function push() {
    if (!port) return;
    const want = draftState();
    for (const p of given.keys()) if (!want.has(p)) want.set(p, base.has(p) ? base.get(p).data : null);
    const puts = [];
    const dels = [];
    const transfer = [];
    for (const [p, bytes] of want) {
      if (termPathProblem(p)) continue;
      const had = given.has(p) ? given.get(p) : base.has(p) ? base.get(p).data : null;
      if (bytes === null ? had === null : had !== null && sameBytes(had, bytes)) continue;
      given.set(p, bytes);
      if (bytes === null) dels.push(p);
      else {
        const copy = bytes.slice().buffer;
        puts.push({ path: p, data: copy, exec: base.has(p) ? base.get(p).exec : false });
        transfer.push(copy);
      }
    }
    if (puts.length) send({ type: "write", files: puts.slice(0, 200) }, transfer);
    if (dels.length) send({ type: "delete", paths: dels.slice(0, 200) });
  }

  // After a commit the drafts are the new base.
  session.on("committed", () => {
    for (const [p, bytes] of given) {
      if (bytes === null) base.delete(p);
      else base.set(p, { data: bytes, exec: base.has(p) ? base.get(p).exec : false });
    }
    given = new Map();
  });

  // A different branch is a different set of files: the terminal does not follow.
  session.on("boot", () => {
    if (frame && session.branch !== openBranch) destroy("You switched branch, so the terminal was closed. Start it again for this branch.");
    openBranch = session.branch;
  });
  let openBranch = session.branch;

  session.on("lock", () => destroy(""));

  // ---------------------------------------------------------------- the review tray

  function askReview() {
    if (!port || pending || scanAsked) return;
    scanAsked = true;
    send({ type: "scan" });
  }

  // The review needs room beside the shell: the panel grows while it is open, then goes back.
  let heightBefore = null;
  function closeTray() {
    review = null;
    clear(tray);
    tray.hidden = true;
    if (heightBefore !== null) {
      el.style.height = heightBefore;
      heightBefore = null;
    }
  }

  /** What the editor holds for a path now: the draft, else the base, as bytes. */
  async function editorBytes(path) {
    const d = session.drafts.get(path);
    if (d) return d.op === "delete" ? null : encode(d.text, d.info);
    const e = session.entry(path);
    if (!e) return null;
    const f = await session.base(path);
    return f ? f.bytes : null;
  }

  function problemOf(c) {
    const limits = session.boot.limits;
    const bad = termPathProblem(c.path);
    if (bad) return bad;
    if (c.kind === "link") return "This is a symbolic link. Links are not moved to drafts.";
    if (c.kind === "other") return "This is not a regular file.";
    const e = session.entry(c.path);
    if (e && (e.mode === "120000" || e.mode === "160000")) return "In the repository this is a link or a submodule.";
    if (folder && !c.path.startsWith(folder + "/")) return "This is outside the folder the terminal holds.";
    if (c.kind === "deleted") return "";
    if (c.data.length > limits.commit_file_bytes) return "Larger than the 2 MB a file may be in one commit.";
    const info = inspect(c.data);
    if (info.readOnly) return info.fixable ? "This file mixes line endings. Fix them in the terminal first." : info.reason;
    if (!sameBytes(encode(info.text, info), c.data)) return "This file cannot be kept byte for byte as a draft.";
    return "";
  }

  async function showTray(c) {
    review = { items: new Map() };
    clear(tray);
    tray.hidden = false;
    if (heightBefore === null && el.getBoundingClientRect().height < innerHeight * 0.6) {
      heightBefore = el.style.height;
      el.style.height = "60vh";
    }
    const ok = [];
    const refused = [];
    for (const f of c.files) {
      const problem = problemOf(f);
      if (problem) refused.push([f, problem]);
      else ok.push(f);
    }
    const head = h("div", { class: "ws-term-tray-head" },
      h("h3", { class: "ws-term-tray-title", text: "Terminal changes" }),
      h("span", { class: "ws-top-spacer" }),
      button("", closeTray, { class: "ws-btn ws-icon-only", icon: "x", aria: "Close the review", title: "Close the review" }));
    tray.append(head);
    if (!c.files.length) {
      tray.append(h("p", { class: "ws-hint", text: "The terminal has not changed any file." }));
      return;
    }
    tray.append(h("p", { class: "ws-note", text: "Open each file and read it before you tick it. Nothing is ticked for you. Files you tick become drafts, and committing them is a separate step. CI may run this code with secrets." }));
    if (c.cut) tray.append(h("p", { class: "ws-hint", text: "Only the first " + MAX_CHANGES + " changes are listed." }));
    const conflicts = new Set((lastChanges && lastChanges.conflicts) || []);
    const list = h("div", { class: "ws-term-list" });
    const moveBtn = button("Move ticked files to drafts", () => accept(), { class: "ws-btn ws-btn-primary" });
    moveBtn.disabled = true;
    const count = h("span", { class: "ws-hint", text: "" });
    const update = () => {
      let n = 0, bytes = 0;
      for (const it of review.items.values()) if (it.box.checked) { n++; bytes += it.change.data ? it.change.data.length : 0; }
      const over = n > session.boot.limits.commit_changes || bytes > session.boot.limits.commit_total_bytes;
      moveBtn.disabled = n === 0 || over || null;
      count.textContent = over ? "That is more than one commit can take." : n === 0 ? "No file ticked." : n === 1 ? "1 file ticked." : n + " files ticked.";
    };
    for (const f of ok) {
      const id = "ws-term-" + review.items.size;
      const box = h("input", { type: "checkbox", id, disabled: true, onchange: update });
      const label = f.kind === "deleted" ? "Deleted" : f.added ? "New" : "Changed";
      const long = f.data && longestLine(new TextDecoder().decode(f.data)) > LONG_LINE;
      const openBtn = button(long ? "Open wrapped" : "Open", async () => {
        await openOne(f, long);
        box.disabled = false;
        box.title = "";
        row.classList.add("ws-term-seen");
      }, { class: "ws-btn" });
      box.title = "Open the file first";
      const row = h("div", { class: "ws-term-row" },
        box,
        h("label", { class: "ws-term-row-main", for: id },
          h("span", { class: "ws-badge", text: label }), pathText(f.path),
          conflicts.has(f.path) ? h("span", { class: "ws-term-chip", text: "Changed in the editor too" }) : null,
          long ? h("span", { class: "ws-term-chip", text: "Long line" }) : null),
        openBtn);
      review.items.set(f.path, { change: f, box });
      list.append(row);
    }
    for (const [f, why] of refused) {
      list.append(h("div", { class: "ws-term-row ws-term-refused" },
        h("span", { class: "ws-term-row-main" }, h("span", { class: "ws-badge", text: "Not moved" }), pathText(f.path)),
        h("span", { class: "ws-hint", text: why })));
    }
    tray.append(list, h("div", { class: "ws-term-tray-actions" }, count, moveBtn));
    update();
  }

  async function openOne(f, long) {
    const before = await editorBytes(f.path).catch(() => null);
    const text = (b) => {
      if (!b) return "";
      const i = inspect(b);
      return i.readOnly && !i.fixable ? "" : i.text.replaceAll("\r\n", "\n");
    };
    editor.showDiff(text(before), f.data ? text(f.data) : "", { longLine: long, wrap: long });
    if (layout.setPane) layout.setPane("edit");
  }

  async function accept() {
    if (!review) return;
    const picked = [];
    for (const it of review.items.values()) if (it.box.checked && !it.box.disabled) picked.push(it.change);
    if (!picked.length) return;
    try {
      // An open tab would still show the old text: close it, and it opens with the draft.
      for (const c of picked) editor.closePath(c.path);
      const n = await session.takeFromTerminal(picked.map((c) => ({ path: c.path, bytes: c.data })));
      for (const c of picked) given.set(c.path, c.data);
      announce(n === 1 ? "1 file moved to drafts. Review it again in the commit dialog." : n + " files moved to drafts. Review them again in the commit dialog.");
      closeTray();
    } catch (e) {
      tray.append(h("p", { class: "ws-error", text: e.message || "The files could not be moved." }));
    }
  }

  return { open, close, toggle, isOpen, destroy };
}
