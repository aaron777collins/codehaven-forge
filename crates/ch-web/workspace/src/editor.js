// The editors (spec section 10): Monaco on a desktop, a plain text area on a phone. Tabs,
// banners, drafts and diffs behave the same either way. Model URIs are counters, never file
// paths, and everything the repository sent back reaches the screen as a text node.

import { h, clear, announce, button, pathText } from "./ui.js";
import { chooseEnding, longestLine, displayPath } from "./bytes.js";
import { LONG_LINE } from "./session.js";

const BY_EXT = new Map([
  ["rs", "rust"], ["ts", "typescript"], ["tsx", "typescript"], ["js", "javascript"], ["mjs", "javascript"],
  ["json", "json"], ["css", "css"], ["scss", "scss"], ["html", "html"], ["md", "markdown"], ["py", "python"],
  ["go", "go"], ["java", "java"], ["kt", "kotlin"], ["swift", "swift"], ["c", "c"], ["h", "c"], ["cpp", "cpp"],
  ["hpp", "cpp"], ["cs", "csharp"], ["rb", "ruby"], ["php", "php"], ["sh", "shell"], ["bash", "shell"],
  ["yml", "yaml"], ["yaml", "yaml"], ["toml", "ini"], ["ini", "ini"], ["sql", "sql"], ["xml", "xml"], ["svg", "xml"],
]);
const BY_NAME = new Map([["dockerfile", "dockerfile"], ["makefile", "plaintext"]]);

function langOf(path) {
  const name = path.split("/").pop().toLowerCase();
  if (BY_NAME.has(name)) return BY_NAME.get(name);
  const dot = name.lastIndexOf(".");
  return BY_EXT.get(dot === -1 ? "" : name.slice(dot + 1)) || "plaintext";
}

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

export async function createEditor(session, layout, opts) {
  const phone = !!(opts && opts.phone);
  const assets = opts.assets;

  const tabs = []; // open order: index 0 is the oldest
  const byPath = new Map();
  let active = null;
  let monaco = null;
  let editor = null;
  let textArea = null;
  let diffEd = null;
  let diffModels = [];
  let counter = 0;
  let suppress = false;

  const editBox = h("div", { class: "ws-edit-box" });
  const diffTop = h("div", { class: "ws-diff-top" });
  const diffHold = h("div", { class: "ws-diff-hold" });
  const diffBox = h("div", { class: "ws-diff-box", hidden: true }, diffTop, diffHold);
  const emptyNote = h("p", { class: "ws-editor-empty", text: "Open a file from Files, or press Ctrl+P to find one." });
  clear(layout.editorEl).append(editBox, diffBox, emptyNote);

  // The note sits over the editor only while there is nothing to show: no tab open and no diff.
  function syncEmpty() {
    emptyNote.hidden = tabs.length > 0 || !diffBox.hidden;
  }

  layout.tabs.addEventListener("keydown", (e) => {
    if ((e.key !== "ArrowLeft" && e.key !== "ArrowRight") || tabs.length < 2) return;
    e.preventDefault();
    const d = e.key === "ArrowRight" ? 1 : -1;
    const i = Math.max(0, tabs.findIndex((t) => t.path === active));
    activate(tabs[(i + d + tabs.length) % tabs.length].path);
  });

  session.on("change", renderTabs);
  session.on("head", refresh);

  // ---------------------------------------------------------------- tabs

  function renderTabs() {
    clear(layout.tabs);
    for (const t of tabs) {
      const on = t.path === active;
      const name = t.path.split("/").pop() || t.path;
      const el = h("div",
        { class: "ws-tab" + (on ? " ws-on" : ""), role: "tab", "aria-selected": String(on), tabindex: on ? "0" : "-1", title: displayPath(t.path) },
        h("span", { class: "ws-tab-main", onclick: () => activate(t.path) },
          session.drafts.has(t.path) ? h("span", { class: "ws-dot", title: "Unsaved changes" }) : null,
          pathText(name, "ws-path")),
        h("button",
          { type: "button", class: "ws-btn ws-icon-only ws-tab-close", title: "Close", "aria-label": "Close this file", tabindex: "-1", onclick: (e) => { e.stopPropagation(); closePath(t.path); } },
          h("span", { class: "ws-x", text: "x" })));
      el.addEventListener("click", (e) => { if (!e.target.closest("button")) activate(t.path); });
      t.el = el;
      layout.tabs.append(el);
    }
    syncEmpty();
  }

  function trimTabs(incoming) {
    while (tabs.length >= 10) {
      let victim = tabs.find((t) => !t.typed && t.path !== incoming);
      if (!victim) victim = tabs.find((t) => t.path !== incoming);
      if (!victim) break;
      closePath(victim.path);
    }
  }

  // ---------------------------------------------------------------- opening

  async function openPath(path, line) {
    if (phone && layout.setPane) layout.setPane("edit");
    if (!byPath.has(path)) {
      let f;
      try {
        f = await session.open(path);
      } catch (e) {
        announce(e.message);
        return;
      }
      if (byPath.has(path)) { activate(path, line); return; }
      trimTabs(path);
      const tab = { path, f, pushed: f.text, typed: false, el: null, model: null, vs: null };
      if (!phone) makeModel(tab);
      tabs.push(tab);
      byPath.set(path, tab);
    }
    activate(path, line);
  }

  function closePath(path) {
    const tab = byPath.get(path);
    if (!tab) return;
    const i = tabs.indexOf(tab);
    tabs.splice(i, 1);
    byPath.delete(path);
    if (tab.model) tab.model.dispose();
    if (active === path) {
      active = null;
      const next = tabs[Math.min(i, tabs.length - 1)];
      if (next) activate(next.path);
      else {
        if (editor) editor.setModel(null);
        clear(layout.banner);
        renderTabs();
      }
    }
    renderTabs();
  }

  function activate(path, line) {
    const tab = byPath.get(path);
    if (!tab) return;
    const prev = active !== path ? byPath.get(active) : null;
    if (prev && editor && phone === false) prev.vs = editor.saveViewState();
    active = path;
    if (phone) {
      if (textArea.value !== tab.f.text) {
        suppress = true;
        textArea.value = tab.f.text;
        suppress = false;
      }
      textArea.readOnly = !!tab.f.readOnly;
    } else if (editor) {
      suppress = true;
      editor.setModel(tab.model);
      if (tab.vs) editor.restoreViewState(tab.vs);
      editor.updateOptions({ readOnly: !!tab.f.readOnly });
      suppress = false;
    }
    refreshBanner(tab);
    renderTabs();
    if (line) revealLine(path, line);
    else if (!phone && editor) editor.focus();
  }

  // ---------------------------------------------------------------- the banner

  function refreshBanner(tab) {
    const b = layout.banner;
    clear(b);
    const f = tab.f;
    if (f.fixable) {
      b.append(
        h("span", { class: "ws-banner-text", text: f.reason || "This file mixes line endings. Choose one to edit it." }),
        button("Use LF", () => pickEol(tab, "lf"), { class: "ws-btn ws-btn-primary" }),
        button("Use CRLF", () => pickEol(tab, "crlf")));
      return;
    }
    if (f.readOnly && f.reason) b.append(h("span", { class: "ws-banner-text", text: f.reason }));
    if (!f.readOnly && longestLine(currentText(tab)) > LONG_LINE) {
      b.append(h("span", { class: "ws-banner-text", text: "One line is very long, so part of it may be hidden." }));
    }
  }

  function pickEol(tab, eol) {
    const info = chooseEnding(tab.f.base.info, eol);
    tab.f = { ...tab.f, info, text: info.text, readOnly: false, reason: "", fixable: false };
    tab.pushed = info.text;
    suppress = true;
    if (phone) {
      textArea.value = info.text;
      textArea.readOnly = false;
    } else if (tab.model) {
      tab.model.setValue(info.text);
      editor.updateOptions({ readOnly: false });
    }
    suppress = false;
    refreshBanner(tab);
  }

  function currentText(tab) {
    return phone ? textArea.value : tab.model ? tab.model.getValue() : "";
  }

  function onType(tab) {
    if (suppress) return;
    const v = phone ? textArea.value : tab.model.getValue();
    if (v === tab.pushed) return;
    tab.pushed = v;
    tab.typed = true;
    session.edit(tab.path, v, tab.f.info);
  }

  // ---------------------------------------------------------------- saving and refreshing

  async function saveActive() {
    if (!active) {
      announce("No file is open.");
      return;
    }
    try {
      const res = await session.save(active);
      const tab = byPath.get(active);
      if (tab) tab.typed = false;
      announce(res ? "Committed" : "Draft saved");
    } catch (e) {
      announce(e.message);
    }
  }

  async function refresh() {
    const path = active;
    if (!path) return;
    let f;
    try {
      f = await session.open(path);
    } catch {
      return;
    }
    const tab = byPath.get(path);
    if (!tab || active !== path) return;
    tab.f = f;
    suppress = true;
    if (phone) {
      if (!tab.typed) textArea.value = f.text;
      textArea.readOnly = !!f.readOnly;
    } else if (editor) {
      if (!tab.typed && tab.model.getValue() !== f.text) tab.model.setValue(f.text);
      editor.updateOptions({ readOnly: !!f.readOnly });
    }
    suppress = false;
    tab.pushed = currentText(tab);
    refreshBanner(tab);
  }

  function revealLine(path, line) {
    if (phone) {
      textArea.focus();
      const parts = textArea.value.split("\n");
      const at = Math.min(Math.max(1, line), parts.length);
      let pos = 0;
      for (let i = 0; i < at - 1; i++) pos += parts[i].length + 1;
      textArea.setSelectionRange(pos, pos + parts[at - 1].length);
      textArea.scrollTop = Math.max(0, (at - 5) * 20);
    } else if (editor) {
      editor.revealLineInCenter(line);
      editor.setPosition({ lineNumber: line, column: 1 });
      editor.focus();
    }
  }

  // ---------------------------------------------------------------- Monaco

  function nextUri() {
    counter += 1;
    return monaco.Uri.from({ scheme: "inmemory", authority: "model", path: "/ws/" + counter });
  }

  function makeModel(tab) {
    tab.model = monaco.editor.createModel(tab.f.text, langOf(tab.path), nextUri());
    tab.model.onDidChangeContent(() => onType(tab));
  }

  function showDiff(original, modified, o) {
    hideDiff();
    const longLine = !!(o && o.longLine);
    if (longLine) {
      clear(diffTop).append(
        h("span", { class: "ws-banner-text", text: "Long line, the diff may hide part of it." }),
        button("Show wrapped text", () => { if (diffEd) diffEd.updateOptions({ diffWordWrap: "on" }); }));
    } else {
      clear(diffTop);
    }
    if (phone || !monaco) {
      phoneDiff(original, modified);
    } else {
      const lang = active ? langOf(active) : "plaintext";
      const a = monaco.editor.createModel(original, lang, nextUri());
      const b = monaco.editor.createModel(modified, lang, nextUri());
      diffModels = [a, b];
      diffEd = monaco.editor.createDiffEditor(diffHold, {
        readOnly: true,
        originalEditable: false,
        ignoreTrimWhitespace: false,
        hideUnchangedRegions: { enabled: false },
        renderControlCharacters: true,
        unicodeHighlight: { invisibleCharacters: true, ambiguousCharacters: true, includeComments: true, includeStrings: true },
        automaticLayout: true,
        diffWordWrap: o && o.wrap ? "on" : "off",
        renderSideBySide: matchMedia("(min-width: 1024px)").matches,
      });
      diffEd.setModel({ original: a, modified: b });
    }
    editBox.hidden = true;
    diffBox.hidden = false;
    syncEmpty();
  }

  function phoneDiff(original, modified) {
    const list = h("div", { class: "ws-udiff" });
    const a = original.split("\n");
    const b = modified.split("\n");
    if (a.length <= 2000 && b.length <= 2000) {
      for (const l of lcsDiff(a, b)) list.append(h("div", { class: "ws-" + l.k }, l.t));
    } else {
      list.append(h("h3", { class: "ws-udiff-head", text: "Before" }));
      for (const t of a) list.append(h("div", { class: "ws-same" }, t));
      list.append(h("h3", { class: "ws-udiff-head", text: "After" }));
      for (const t of b) list.append(h("div", { class: "ws-same" }, t));
    }
    clear(diffHold).append(list);
  }

  function hideDiff() {
    if (diffEd) {
      diffEd.dispose();
      diffEd = null;
    }
    for (const m of diffModels) m.dispose();
    diffModels = [];
    clear(diffTop);
    clear(diffHold);
    diffBox.hidden = true;
    editBox.hidden = false;
    syncEmpty();
  }

  function palette() {
    if (editor) {
      editor.focus();
      editor.trigger("keyboard", "editor.action.quickCommand", {});
    }
  }

  function dispose() {
    if (diffEd) {
      diffEd.dispose();
      diffEd = null;
    }
    for (const m of diffModels) m.dispose();
    diffModels = [];
    for (const t of tabs) if (t.model) t.model.dispose();
    tabs.length = 0;
    byPath.clear();
    if (editor) {
      editor.dispose();
      editor = null;
    }
    clear(editBox);
    clear(diffHold);
    clear(diffTop);
    active = null;
  }

  // ---------------------------------------------------------------- build one of the two editors

  if (!phone) {
    const vs = assets.replace(/\/+$/, "") + "/monaco/vs";
    await new Promise((resolve, reject) => {
      const s = document.createElement("script");
      s.src = vs + "/loader.js";
      s.onload = resolve;
      s.onerror = () => reject(new Error("The editor could not load."));
      document.head.append(s);
    });
    const LANGUAGE_WORKERS = new Set(["typescript", "javascript", "json", "css", "scss", "less", "html", "handlebars", "razor"]);
    self.MonacoEnvironment = {
      getWorker(_id, label) {
        // Only the editor's own worker. Language service workers are not shipped, and Monaco's
        // fallback (a blob: worker) is refused by the Trusted Types policy.
        if (LANGUAGE_WORKERS.has(String(label).toLowerCase())) throw new Error("No worker for " + label);
        return new Worker(vs + "/assets/editor.worker-_vAIFJDs.js", { name: String(label) });
      },
    };
    window.require.config({ paths: { vs } });
    monaco = await new Promise((resolve, reject) => window.require(["vs/editor/editor.main"], () => resolve(window.monaco), reject));

    const ns = (n) => monaco[n] || (monaco.languages && monaco.languages[n]);
    const off = (d) => { if (d && d.setModeConfiguration) d.setModeConfiguration({}); };
    off(ns("typescript")?.typescriptDefaults); off(ns("typescript")?.javascriptDefaults);
    off(ns("json")?.jsonDefaults);
    off(ns("css")?.cssDefaults); off(ns("css")?.scssDefaults); off(ns("css")?.lessDefaults);
    off(ns("html")?.htmlDefaults); off(ns("html")?.handlebarDefaults); off(ns("html")?.razorDefaults);
    monaco.editor.registerLinkOpener({ open: () => true });

    const applyTheme = () => {
      const t = document.documentElement.dataset.theme;
      const dark = t === "dark" || (t !== "light" && matchMedia("(prefers-color-scheme: dark)").matches);
      monaco.editor.defineTheme("codehaven", {
        base: dark ? "vs-dark" : "vs",
        inherit: true,
        rules: [],
        colors: {
          "editor.background": cssVar("--surface"),
          "editor.foreground": cssVar("--text"),
          "editorLineNumber.foreground": cssVar("--muted"),
          "editor.selectionBackground": cssVar("--accent-soft"),
          "editorCursor.foreground": cssVar("--accent"),
          "editorWidget.border": cssVar("--border"),
        },
      });
      monaco.editor.setTheme("codehaven");
    };
    applyTheme();
    matchMedia("(prefers-color-scheme: dark)").addEventListener("change", applyTheme);

    editor = monaco.editor.create(editBox, {
      automaticLayout: true,
      links: false,
      renderControlCharacters: true,
      unicodeHighlight: { invisibleCharacters: true, ambiguousCharacters: true, includeComments: true, includeStrings: true },
      minimap: { enabled: false },
      fontFamily: cssVar("--mono"),
      fontSize: 13,
      scrollBeyondLastLine: false,
      theme: "codehaven",
      ariaLabel: "Editor",
    });
  } else {
    textArea = h("textarea",
      { class: "ws-textarea", spellcheck: "false", autocapitalize: "off", autocomplete: "off", "aria-label": "Editor" });
    editBox.append(textArea);
    textArea.addEventListener("input", () => {
      const t = byPath.get(active);
      if (t) onType(t);
    });
  }

  return {
    openPath,
    closePath,
    get active() { return active; },
    saveActive,
    palette,
    showDiff,
    hideDiff,
    refresh,
    dispose,
  };
}

// ---------------------------------------------------------------- the phone diff

/** A longest-common-subsequence line diff, used only when there is no Monaco to do it. */
export function lcsDiff(a, b) {
  const n = a.length;
  const m = b.length;
  const w = m + 1;
  const dp = new Uint32Array((n + 1) * w);
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i * w + j] = a[i] === b[j] ? dp[(i + 1) * w + j + 1] + 1 : Math.max(dp[(i + 1) * w + j], dp[i * w + j + 1]);
    }
  }
  const out = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push({ k: "same", t: a[i] });
      i++;
      j++;
    } else if (dp[(i + 1) * w + j] >= dp[i * w + j + 1]) {
      out.push({ k: "del", t: a[i] });
      i++;
    } else {
      out.push({ k: "add", t: b[j] });
      j++;
    }
  }
  while (i < n) {
    out.push({ k: "del", t: a[i] });
    i++;
  }
  while (j < m) {
    out.push({ k: "add", t: b[j] });
    j++;
  }
  return out;
}
