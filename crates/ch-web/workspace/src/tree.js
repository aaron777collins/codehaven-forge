// The files tree, the search view and quick open (spec section 10). Paths live in Maps and Sets
// and reach the page only through `pathText`.

import { h, icon, pathText, clear, announce, dialog, button } from "./ui.js";
import { displayPath } from "./bytes.js";

/** Closes a dialog from a button inside its body, so the outer await finishes. */
function closeVia(el) {
  const d = el.closest(".ws-dialog");
  const btns = d ? d.querySelectorAll(".ws-dialog-actions .ws-btn") : [];
  if (btns.length) btns[btns.length - 1].click();
}

// ---------------------------------------------------------------- files

export function mountFiles(session, el, editor) {
  const openDirs = new Set(); // folder paths a person opened
  const rowNode = new Map(); // row element -> { kind, path, name, depth }
  let focusPath = null;

  const cutNote = h("p", { class: "ws-note", text: "This repository has too many files to list them all. It is read only here." });
  const newBtn = button("New file", () => newFile("New file"), { icon: "plus" });
  const firstBtn = button("Create the first file", () => newFile("Create the first file"), { icon: "plus", class: "ws-btn ws-btn-primary" });
  const head = h("div", { class: "ws-tree-head" }, newBtn, firstBtn);
  const treeBox = h("div", { class: "ws-tree", role: "tree", "aria-label": "Files" });
  clear(el).append(cutNote, head, treeBox);
  el.classList.add("ws-files");
  treeBox.addEventListener("keydown", onTreeKey);
  treeBox.addEventListener("focusin", (e) => {
    const row = e.target.closest(".ws-row");
    const n = row && rowNode.get(row);
    if (n && n.kind === "file") focusPath = n.path;
  });

  session.on("boot", render);
  session.on("change", render);
  render();

  function render() {
    clear(treeBox);
    rowNode.clear();
    const boot = session.boot;
    cutNote.hidden = !session.cut;
    const canCommit = !!boot && session.canCommit;
    const emptyRepo = !!boot && !!boot.repo.empty && session.drafts.size === 0;
    newBtn.hidden = !canCommit || null;
    firstBtn.hidden = !emptyRepo || null;
    head.hidden = (newBtn.hidden && firstBtn.hidden) || null;
    if (!boot) return;

    const changed = new Map();
    for (const c of session.changes()) changed.set(c.path, c);

    const root = { dirs: new Map(), files: [] };
    for (const p of session.paths()) {
      const parts = p.split("/");
      let node = root;
      for (let i = 0; i < parts.length - 1; i++) {
        if (!node.dirs.has(parts[i])) node.dirs.set(parts[i], { full: parts.slice(0, i + 1).join("/"), dirs: new Map(), files: [] });
        node = node.dirs.get(parts[i]);
      }
      node.files.push({ name: parts[parts.length - 1], path: p });
    }
    for (const d of root.dirs.keys()) openDirs.add(d); // the top level starts open

    const walk = (node, depth) => {
      for (const name of [...node.dirs.keys()].sort((a, b) => a.localeCompare(b))) {
        const dir = node.dirs.get(name);
        row("dir", dir.full, name, depth);
        if (openDirs.has(dir.full)) walk(dir, depth + 1);
      }
      node.files.sort((a, b) => a.name.localeCompare(b.name));
      for (const f of node.files) row("file", f.path, f.name, depth);
    };
    walk(root, 0);

    const rows = [...rowNode.keys()];
    const focusRow = rows.find((r) => { const n = rowNode.get(r); return n.kind === "file" && n.path === focusPath; });
    if (focusRow) focusRow.setAttribute("tabindex", "0");
    else if (rows.length) rows[0].setAttribute("tabindex", "0");
    if (!rows.length) treeBox.append(h("p", { class: "ws-hint", text: "No files here yet." }));

    function row(kind, path, name, depth) {
      const on = kind === "dir" && openDirs.has(path);
      const draft = kind === "file" && session.drafts.has(path);
      const added = kind === "file" && changed.has(path) && changed.get(path).added;
      const r = h("div",
        { class: "ws-row ws-" + kind + (draft ? " ws-changed" : ""), role: "treeitem", "aria-level": String(depth + 1), "aria-expanded": kind === "dir" ? String(on) : null, tabindex: "-1", style: "padding-left:" + (6 + depth * 14) + "px", title: displayPath(path) },
        h("span", { class: "ws-twist" + (on ? " ws-twist-open" : ""), text: kind === "dir" ? ">" : "", "aria-hidden": "true" }),
        icon(kind === "dir" ? "folder" : "file"),
        h("button", { type: "button", class: "ws-row-main" },
          h("span", { class: "ws-row-name" }, pathText(name, "ws-path")),
          added ? h("span", { class: "ws-badge", text: "A" }) : draft ? h("span", { class: "ws-dot", title: "Unsaved changes" }) : null));
      r.querySelector(".ws-row-main").addEventListener("click", () => {
        if (kind === "dir") {
          focusPath = null;
          if (openDirs.has(path)) openDirs.delete(path); else openDirs.add(path);
          render();
        } else {
          focusPath = path;
          editor.openPath(path);
        }
      });
      if (kind === "file") {
        r.append(h("button",
          { type: "button", class: "ws-btn ws-icon-only ws-row-more", title: "File actions", "aria-label": "File actions for this file", tabindex: "-1", onclick: (e) => { e.stopPropagation(); fileMenu(path); } },
          h("span", { text: "..." })));
        r.addEventListener("contextmenu", (e) => { e.preventDefault(); e.stopPropagation(); fileMenu(path); });
      }
      rowNode.set(r, { kind, path, name, depth });
      treeBox.append(r);
      return r;
    }
  }

  function onTreeKey(e) {
    const row = e.target.closest(".ws-row");
    const n = row && rowNode.get(row);
    if (!n) return;
    const keys = [...rowNode.keys()];
    const i = keys.indexOf(row);
    const move = (d) => { const r = keys[i + d]; if (r) { e.preventDefault(); r.focus(); } };
    if (e.key === "ArrowDown") return move(1);
    if (e.key === "ArrowUp") return move(-1);
    if (e.key === "ArrowRight") {
      e.preventDefault();
      if (n.kind === "dir" && !openDirs.has(n.path)) { openDirs.add(n.path); render(); }
      else if (keys[i + 1]) keys[i + 1].focus();
      return;
    }
    if (e.key === "ArrowLeft") {
      e.preventDefault();
      if (n.kind === "dir" && openDirs.has(n.path)) { openDirs.delete(n.path); render(); return; }
      const up = n.path.includes("/") ? n.path.slice(0, n.path.lastIndexOf("/")) : null;
      const parent = up === null ? null : keys.find((r) => { const m = rowNode.get(r); return m.kind === "dir" && m.path === up; });
      if (parent) parent.focus();
      return;
    }
    if ((e.key === "Enter" || e.key === " ") && n.kind === "file") {
      e.preventDefault();
      focusPath = n.path;
      editor.openPath(n.path);
    }
  }

  async function newFile(title) {
    let err = "";
    for (;;) {
      const input = h("input", { type: "text", class: "ws-input", placeholder: "path/to/file.rs", "aria-label": "File path", autocomplete: "off", spellcheck: "false", autofocus: true });
      const body = h("div", { class: "ws-dialog-body" }, err ? h("p", { class: "ws-error", text: err }) : null, input);
      const v = await dialog(title, body, [{ label: "Create", value: "ok", primary: true }, { label: "Cancel", value: null }]);
      if (v !== "ok") return;
      const p = input.value.trim().replace(/^\/+/, "");
      if (!p) { err = "Type a file path."; continue; }
      try { session.create(p); } catch (e2) { err = e2.message; continue; }
      editor.openPath(p);
      return;
    }
  }

  async function fileMenu(path) {
    let picked = "";
    const act = (name) => (e) => { picked = name; closeVia(e.currentTarget); };
    await dialog("File actions", h("div", { class: "ws-dialog-body" },
      h("p", { class: "ws-dialog-path" }, pathText(path)),
      button("Rename", act("rename")),
      button("Delete", act("delete"), { class: "ws-btn ws-btn-danger" })),
      [{ label: "Close", value: null }]);
    if (picked === "rename") await renameFile(path);
    else if (picked === "delete") await deleteFile(path);
  }

  async function renameFile(from) {
    let err = "";
    for (;;) {
      const input = h("input", { type: "text", class: "ws-input", value: from, "aria-label": "New file path", autocomplete: "off", spellcheck: "false", autofocus: true });
      const v = await dialog("Rename file", h("div", { class: "ws-dialog-body" }, err ? h("p", { class: "ws-error", text: err }) : null, input),
        [{ label: "Rename", value: "ok", primary: true }, { label: "Cancel", value: null }]);
      if (v !== "ok") return;
      const to = input.value.trim().replace(/^\/+/, "");
      if (!to || to === from) return;
      try { await session.rename(from, to); } catch (e) { err = e.message; continue; }
      editor.closePath(from);
      editor.openPath(to);
      return;
    }
  }

  async function deleteFile(path) {
    const v = await dialog("Delete file", h("div", { class: "ws-dialog-body" },
      h("p", { class: "ws-dialog-path" }, pathText(path)),
      h("p", { class: "ws-hint", text: "This is a draft until you commit it." })),
      [{ label: "Delete", value: "del", danger: true }, { label: "Cancel", value: null }]);
    if (v !== "del") return;
    try { await session.remove(path); } catch (e) { announce(e.message); }
    editor.closePath(path);
  }
}

// ---------------------------------------------------------------- search

export function mountSearch(session, el, editor) {
  const input = h("input",
    { type: "search", class: "ws-input ws-search-input", placeholder: "Search this repository", "aria-label": "Search this repository", autocomplete: "off", spellcheck: "false" });
  const busy = h("p", { class: "ws-hint", text: "Searching...", hidden: true });
  const out = h("div", { class: "ws-search-out" });
  clear(el).append(
    h("div", { class: "ws-search-head" }, input),
    h("p", { class: "ws-hint", text: "Case and words are matched literally." }),
    busy, out);
  el.classList.add("ws-search");
  input.addEventListener("keydown", (e) => { if (e.key === "Enter") run(); });

  async function run() {
    const text = input.value;
    if (!text.trim()) { clear(out); return; }
    busy.hidden = false;
    let res;
    try {
      res = await session.search(text);
    } catch (e) {
      busy.hidden = true;
      clear(out).append(h("p", { class: "ws-error", text: e.message }));
      return;
    }
    busy.hidden = true;
    clear(out);
    if (res.cut) out.append(h("p", { class: "ws-hint", text: "Showing the first results only." }));
    if (res.timedOut) out.append(h("p", { class: "ws-hint", text: "The search took too long, so these results are not complete." }));
    if (!res.hits.length) {
      out.append(h("p", { class: "ws-hint", text: "Nothing found." }));
      return;
    }
    const groups = new Map();
    for (const hit of res.hits) {
      if (!groups.has(hit.path)) groups.set(hit.path, []);
      groups.get(hit.path).push(hit);
    }
    for (const [path, hits] of groups) {
      out.append(h("div", { class: "ws-search-group" },
        h("div", { class: "ws-search-file" }, pathText(path)),
        hits.map((hit) => h("button",
          { type: "button", class: "ws-hit", onclick: () => editor.openPath(hit.path, hit.line) },
          h("span", { class: "ws-hit-line", text: String(hit.line) }),
          h("span", { class: "ws-hit-text" }, hit.text || ""),
          hit.draft ? h("span", { class: "ws-badge", text: "unsaved" }) : null))));
    }
  }

  return input;
}

// ---------------------------------------------------------------- quick open

export async function quickOpen(session, editor) {
  const paths = session.paths();
  if (!paths.length) {
    announce("There are no files to open.");
    return;
  }
  let sel = 0;
  let shown = [];
  let picked = null;
  const input = h("input",
    { type: "text", class: "ws-input", placeholder: "Type a file name", "aria-label": "Open a file", autocomplete: "off", spellcheck: "false", autofocus: true });
  const list = h("div", { class: "ws-list ws-quick-list", role: "listbox", "aria-label": "Files" });

  const renderList = () => {
    const f = input.value.trim().toLowerCase();
    shown = [];
    clear(list);
    for (const p of paths) {
      if (shown.length >= 50) break;
      if (f && !p.toLowerCase().includes(f)) continue;
      shown.push(p);
    }
    if (sel >= shown.length) sel = Math.max(0, shown.length - 1);
    shown.forEach((p, i) => {
      list.append(h("button",
        { type: "button", class: "ws-list-item" + (i === sel ? " ws-on" : ""), role: "option", "aria-selected": String(i === sel), onclick: (e) => { picked = p; closeVia(e.currentTarget); } },
        pathText(p)));
    });
    if (!shown.length) list.append(h("p", { class: "ws-hint", text: "No file matches that." }));
  };

  input.addEventListener("input", () => { sel = 0; renderList(); });
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      if (sel < shown.length - 1) { sel++; renderList(); }
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (sel > 0) { sel--; renderList(); }
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (shown[sel]) { picked = shown[sel]; closeVia(input); }
    }
  });
  renderList();
  await dialog("Open a file", h("div", { class: "ws-dialog-body" }, input, list), [{ label: "Close", value: null }]);
  if (picked) editor.openPath(picked);
}

