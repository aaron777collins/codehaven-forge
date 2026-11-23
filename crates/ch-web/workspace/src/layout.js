// The frame around the editor: top bar, rail, side views, tabs, banner, runs panel, and the
// three smaller layouts (spec section 10). No file text is rendered here; only names through
// `pathText`, and nothing is ever a property key.

import { h, icon, pathText, clear, button, dialog } from "./ui.js";
import { openCommitDialog } from "./changes.js";

const VIEWS = [
  ["files", "Files", "files"],
  ["search", "Search", "search"],
  ["changes", "Changes", "changes"],
];

export function buildLayout(root, session, opts) {
  const phone = !!(opts && opts.phone);
  let editor = null;
  let currentView = "files";
  let panelOpen = false;
  let pane = "files"; // phones: which of files, edit, changes, runs fills the screen

  // ---------------------------------------------------------------- top bar

  const repo = h("a", { class: "ws-repo", href: session.meta.base.replace(/\/workspace$/, ""), title: "Back to the repository" }, pathText(session.meta.repo || ""));
  const branchName = h("span", { class: "ws-branch-name" });
  const branchBtn = h("button",
    { type: "button", class: "ws-btn ws-branch", title: "Switch branch", "aria-label": "Switch branch", onclick: openBranches },
    icon("branch"), branchName);
  const chip = h("span", { class: "ws-chip", hidden: true }, icon("alert"), h("span", { class: "ws-chip-text" }));
  const count = h("span", { class: "ws-count" });
  const status = h("span", { class: "ws-status", role: "status" });
  // The terminal's button appears once `boot` says the terminal is on.
  let onTerminal = null;
  const termBtn = h("button",
    { type: "button", class: "ws-btn ws-term-toggle", title: "Terminal (Ctrl+`)", "aria-label": "Terminal", "aria-expanded": "false", hidden: true, onclick: () => { if (onTerminal) onTerminal(); } },
    icon("terminal"));
  const runsToggle = h("button",
    { type: "button", class: "ws-btn ws-runs-toggle", title: "Runs", "aria-label": "Runs", "aria-expanded": "false", onclick: toggleRuns },
    icon("runs"));
  // On a phone the label is "Commit", so the repository name keeps its room in the top bar.
  const commitBtn = h("button", { type: "button", class: "ws-btn ws-btn-primary", "aria-label": "Commit and run", onclick: onCommit },
    h("span", { text: "Commit" }), h("span", { class: "ws-wide", text: "and run" }));
  const top = h("header", { class: "ws-top" },
    repo, branchBtn, chip,
    h("span", { class: "ws-top-spacer" }),
    count, status, termBtn, runsToggle, commitBtn);

  // ---------------------------------------------------------------- rail, views, side

  const railBtns = new Map();
  const rail = h("nav", { class: "ws-rail", "aria-label": "Views" });
  for (const [name, label, ic] of VIEWS) {
    const b = h("button",
      { type: "button", class: "ws-rail-btn", title: label, "aria-label": label, "aria-pressed": "false", onclick: () => show(name) },
      icon(ic));
    railBtns.set(name, b);
    rail.append(b);
  }

  const viewEls = {
    files: h("section", { class: "ws-view", id: "ws-view-files", "aria-label": "Files" }),
    search: h("section", { class: "ws-view", id: "ws-view-search", "aria-label": "Search" }),
    changes: h("section", { class: "ws-view", id: "ws-view-changes", "aria-label": "Changes" }),
  };
  const drawerClose = h("button",
    { type: "button", class: "ws-btn ws-icon-only ws-drawer-close", title: "Close", "aria-label": "Close", onclick: () => setDrawer(false) },
    icon("x"));
  const side = h("aside", { class: "ws-side", "aria-label": "Repository views" },
    h("div", { class: "ws-side-head" }, h("span", { class: "ws-side-title", text: "Views" }), drawerClose),
    viewEls.files, viewEls.search, viewEls.changes,
    h("div", { class: "ws-side-grip", title: "Drag to resize", onpointerdown: (e) => startColGrip(e, side, 200, 600, false) }));

  // ---------------------------------------------------------------- centre

  const tabs = h("div", { class: "ws-tabs", role: "tablist", "aria-label": "Open files" });
  const banner = h("div", { class: "ws-banner", role: "status" });
  const editorEl = h("div", { class: "ws-editor" });
  const termEl = h("section", { class: "ws-term", "aria-label": "Terminal", hidden: true });
  const center = h("main", { class: "ws-center", id: "ws-main" }, tabs, banner, editorEl, termEl);

  // ---------------------------------------------------------------- runs panel and sheet

  const runsEl = h("div", { id: "ws-runs", class: "ws-runs" });
  const panel = h("aside", { class: "ws-panel", "aria-label": "Runs" },
    h("div", { class: "ws-panel-head" }, h("h2", { class: "ws-panel-title", text: "Runs" }),
      h("div", { class: "ws-side-grip ws-panel-grip", title: "Drag to resize", onpointerdown: (e) => startColGrip(e, panel, 280, 700, true) })),
    runsEl);
  const sheet = h("div", { class: "ws-sheet", "aria-label": "Runs" },
    h("div", { class: "ws-sheet-handle", title: "Drag to resize", onpointerdown: startSheetGrip }));
  const sheetBody = h("div", { class: "ws-sheet-body" });

  // ---------------------------------------------------------------- phone tabs

  const phoneBtns = new Map();
  const phoneTabs = h("nav", { class: "ws-phone-tabs", "aria-label": "Views" });
  for (const [name, label, ic] of [["files", "Files", "files"], ["edit", "Edit", "file"], ["changes", "Changes", "changes"], ["runs", "Runs", "runs"], ["terminal", "Terminal", "terminal"]]) {
    const b = h("button",
      { type: "button", class: "ws-phone-tab", "aria-pressed": "false", hidden: name === "terminal" || null,
        onclick: () => (name === "terminal" && onTerminal ? onTerminal() : setPane(name)) },
      icon(ic), h("span", { text: label }));
    phoneBtns.set(name, b);
    phoneTabs.append(b);
  }

  const app = h("div", { class: "ws-app", "data-pane": "files" });
  const main = h("div", { class: "ws-main" }, rail, side, center, panel);
  app.append(top, main, sheet, phoneTabs);
  clear(root).append(app);

  // ---------------------------------------------------------------- width ranges

  const midRange = matchMedia("(min-width: 640px) and (max-width: 1023.98px)");
  const wideRange = matchMedia("(min-width: 1024px)");
  const fullRange = matchMedia("(min-width: 1280px)");
  const onChangeRange = () => { placePanel(); apply(); };
  for (const m of [midRange, wideRange, fullRange]) {
    if (m.addEventListener) m.addEventListener("change", onChangeRange);
  }
  panelOpen = fullRange.matches;

  function placePanel() {
    const want = midRange.matches ? sheetBody : main;
    if (panel.parentElement !== want) want.append(panel);
    sheet.append(sheetBody);
  }

  function apply() {
    app.classList.toggle("ws-panel-open", panelOpen);
    runsToggle.setAttribute("aria-expanded", String(panelOpen));
    app.classList.toggle("ws-drawer-open", drawerWanted && midRange.matches);
  }

  let drawerWanted = false;
  function setDrawer(on) {
    drawerWanted = on;
    if (!on) drawerClose.blur();
    apply();
  }

  // ---------------------------------------------------------------- views and panes

  function syncViews() {
    for (const [n] of VIEWS) {
      viewEls[n].hidden = n !== currentView;
      railBtns.get(n).setAttribute("aria-pressed", String(n === currentView));
    }
  }

  function syncPane() {
    app.dataset.pane = pane;
    for (const [n, b] of phoneBtns) b.setAttribute("aria-pressed", String(n === pane));
  }

  function show(name) {
    currentView = name;
    syncViews();
    if (midRange.matches) setDrawer(true);
    if (!wideRange.matches) { pane = name === "search" ? "files" : name; syncPane(); }
    const first = viewEls[name].querySelector("button, input, [tabindex]");
    if (first && !phone) first.focus();
  }

  function setPane(name) {
    pane = name;
    syncPane();
    if (name === "files" || name === "changes") { currentView = name; syncViews(); }
  }

  function toggleRuns() {
    panelOpen = !panelOpen;
    apply();
  }

  // ---------------------------------------------------------------- resizing

  function startColGrip(e, el, min, max, fromLeft) {
    e.preventDefault();
    const startX = e.clientX, startW = el.getBoundingClientRect().width;
    const move = (ev) => {
      const d = fromLeft ? startX - ev.clientX : ev.clientX - startX;
      el.style.width = Math.min(max, Math.max(min, Math.round(startW + d))) + "px";
    };
    const up = () => { removeEventListener("pointermove", move); removeEventListener("pointerup", up); };
    addEventListener("pointermove", move); addEventListener("pointerup", up);
  }

  function startSheetGrip(e) {
    e.preventDefault();
    const startH = sheet.getBoundingClientRect().height, startY = e.clientY;
    const move = (ev) => {
      const dvh = ((startH + (startY - ev.clientY)) / innerHeight) * 100;
      sheet.style.height = Math.min(85, Math.max(20, Math.round(dvh))) + "dvh";
    };
    const up = () => { removeEventListener("pointermove", move); removeEventListener("pointerup", up); };
    addEventListener("pointermove", move); addEventListener("pointerup", up);
  }

  addEventListener("keydown", (e) => {
    if (e.key === "Escape" && midRange.matches && drawerWanted) setDrawer(false);
  });

  // ---------------------------------------------------------------- the keyboard on phones

  if (phone && window.visualViewport) {
    const vv = visualViewport;
    const check = () => document.body.classList.toggle("ws-kb", vv.height < innerHeight * 0.75);
    vv.addEventListener("resize", check);
    check();
  }

  // ---------------------------------------------------------------- top bar behaviour

  function setStatus(text, title) {
    status.textContent = text || "";
    status.title = title || text || "";
  }

  function setCount(n) {
    count.textContent = n === 0 ? "No changes" : n === 1 ? "1 change" : n + " changes";
  }

  function updateCount() {
    if (!session.boot) { setCount(0); return; }
    setCount(session.changes().length);
  }

  function updateCommit() {
    const n = session.boot ? session.changes().length : 0;
    const no = !session.boot || !session.canCommit;
    commitBtn.disabled = no || n === 0 || null;
    commitBtn.title = n === 0 ? "There is nothing to commit." : no ? "This tab cannot commit." : "Commit every change and start the checks.";
  }

  function onCommit() {
    if (commitBtn.disabled || !editor) return;
    openCommitDialog(session, editor);
  }

  function refreshTop() {
    const boot = session.boot;
    if (!boot) return;
    clear(branchName); branchName.append(pathText(session.branch || ""));
    clear(repo); repo.append(pathText(boot.repo.full_name || session.meta.repo || ""));
    if (boot.ci && boot.ci.paused) {
      chip.hidden = false;
      chip.lastChild.textContent = boot.ci.reason || "Checks are paused.";
      chip.title = boot.ci.reason || "Checks are paused.";
    } else chip.hidden = true;
    updateCount();
    updateCommit();
    if (!session.store || session.store.persistent) setStatus("");
    else setStatus(session.store.reason || "Drafts stay in this tab only.");
  }

  session.on("boot", refreshTop);
  session.on("change", () => { updateCount(); updateCommit(); });

  // ---------------------------------------------------------------- branch picker

  function closeVia(el, value) {
    const d = el.closest(".ws-dialog");
    const btns = d ? d.querySelectorAll(".ws-dialog-actions .ws-btn") : [];
    if (btns.length) btns[btns.length - 1].click();
    return value;
  }

  async function openBranches() {
    const boot = session.boot;
    if (!boot) return;
    let picked = null;
    let wantNew = false;
    const input = h("input", { type: "text", class: "ws-input", placeholder: "Filter branches", "aria-label": "Filter branches", autocomplete: "off", spellcheck: "false" });
    const list = h("div", { class: "ws-list", role: "listbox", "aria-label": "Branches" });
    const render = () => {
      clear(list);
      const f = input.value.trim().toLowerCase();
      let shown = 0;
      for (const b of boot.branches || []) {
        if (shown >= 200) break;
        if (f && !b.toLowerCase().includes(f)) continue;
        shown++;
        const on = b === session.branch;
        list.append(h("button",
          { type: "button", class: "ws-list-item" + (on ? " ws-on" : ""), role: "option", "aria-selected": String(on), onclick: (e) => { picked = b; closeVia(e.currentTarget); } },
          pathText(b)));
      }
      if (!list.firstChild) list.append(h("p", { class: "ws-hint", text: "No branch matches that." }));
    };
    input.addEventListener("input", render);
    render();
    const body = h("div", { class: "ws-dialog-body" }, input,
      button("New branch", (e) => { wantNew = true; closeVia(e.currentTarget); }, { icon: "plus" }),
      list);
    await dialog("Branches", body, [{ label: "Close", value: null }]);
    if (wantNew) await newBranch();
    else if (picked && picked !== session.branch) {
      try { await session.switchBranch(picked); } catch (err) { setStatus(err.message); }
    }
  }

  async function newBranch() {
    const boot = session.boot;
    if (!boot) return;
    const pre = boot.suggested_branch || ((boot.login || "you") + "/patch");
    const input = h("input", { type: "text", class: "ws-input", value: pre, "aria-label": "Branch name", autocomplete: "off", spellcheck: "false" });
    const v = await dialog("New branch", h("div", { class: "ws-dialog-body" }, h("p", { class: "ws-hint", text: "Made from the branch you are on." }), input),
      [{ label: "Create", value: "ok", primary: true }, { label: "Cancel", value: null }]);
    const name = v === "ok" ? input.value.trim() : "";
    if (!name) return;
    try {
      await session.newBranch(name);
      await session.switchBranch(name);
    } catch (err) { setStatus(err.message); }
  }

  // ---------------------------------------------------------------- finish

  show("files");
  placePanel();
  apply();
  setCount(0);
  updateCommit();

  return {
    top: { branchBtn, count, status, commitBtn, repo, termBtn },
    views: viewEls,
    show,
    setPane,
    tabs,
    banner,
    editorEl,
    runsEl,
    setStatus,
    setCount,
    setEditor(e) { editor = e; },
    termEl,
    /** Shows the terminal's button and phone tab; `fn` opens or focuses it. */
    enableTerminal(fn) {
      onTerminal = fn;
      termBtn.hidden = false;
      phoneBtns.get("terminal").hidden = false;
    },
    /** The terminal panel opened or closed. On a phone it is a pane of its own. */
    termShown(on) {
      app.classList.toggle("ws-term-open", on);
      termBtn.setAttribute("aria-expanded", String(on));
      if (on && matchMedia("(max-width: 639.98px)").matches) setPane("terminal");
      else if (!on && pane === "terminal") setPane("edit");
    },
  };
}
