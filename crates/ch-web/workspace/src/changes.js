// The Changes view and the commit dialog (spec section 10): every draft as a row, conflicts
// after the branch moved under the drafts, where a commit lands, and commit on save. Paths and
// branch names reach the page only as text through `pathText`, and nothing that came from the
// server is ever used as a key.

import { h, pathText, clear, announce, button, dialog } from "./ui.js";
import { inspect, longestLine } from "./bytes.js";
import { LONG_LINE, NOTHING_RUNS, NO_WORKFLOWS, workflowQuery, touchedPaths } from "./session.js";
import { lcsDiff } from "./editor.js";

const LETTER_WORDS = { A: "Added", M: "Edited", R: "Renamed", D: "Deleted" };

/** The one-letter mark of a change: A added, M edited, R renamed, D deleted. */
function letter(c) {
  return c.op === "delete" ? "D" : c.op === "rename" ? "R" : c.added ? "A" : "M";
}

/** Where drafts live, and what that is worth. */
function storeNote(session) {
  if (session.store && !session.store.persistent) {
    return h("p", { class: "ws-note", text: session.store.reason || "Drafts stay in this tab only." });
  }
  return h("p", { class: "ws-hint", text: "Drafts are saved in this browser, encrypted. They are not a backup." });
}

// ---------------------------------------------------------------- the view

export function mountChanges(session, el, editor) {
  let editing = null; // the conflict whose result is being written by hand
  let lastCommit = null; // what the last commit landed on, for the pull request note

  const note = storeNote(session);
  const commitNote = h("div", { class: "ws-commit-note" });
  const conflictBox = h("div", { class: "ws-conflicts" });
  const list = h("div", { class: "ws-changes-list" });
  const empty = h("p", { class: "ws-hint", text: "No changes. Edit a file to start." });
  const actions = h("div", { class: "ws-changes-actions" },
    button("Commit and run", () => openCommitDialog(session, editor), { class: "ws-btn ws-btn-primary" }),
    button("Discard all", discardAll, { class: "ws-btn ws-btn-danger" }),
    button("Export drafts", exportDrafts));

  const autoCheck = h("input",
    { type: "checkbox", id: "ws-commit-on-save", onchange: () => { autoCheck.checked = session.setAutoCommit(autoCheck.checked); } });
  const autoReason = h("p", { class: "ws-hint" });
  const autoBox = h("div", { class: "ws-autocommit" },
    h("label", { class: "ws-check", for: "ws-commit-on-save" }, autoCheck, h("span", { text: "Commit on save" })),
    autoReason,
    h("p", { class: "ws-hint", text: "Saving with Cmd+S or Ctrl+S commits at most once every 20 seconds." }));

  clear(el).append(note, commitNote, conflictBox, list, empty, actions, autoBox);
  el.classList.add("ws-changes");

  session.on("committed", (d) => { lastCommit = d; editor.hideDiff(); render(); });
  session.on("boot", onBoot);
  session.on("change", render);
  session.on("head", render);
  session.on("autocommit-paused", (reason) => { render(); announce(reason); });
  render();

  function onBoot() {
    // A branch picked by hand ends the note about the last commit. The switch a commit makes
    // itself keeps it, because `committed` fires before the boot that follows it.
    if (lastCommit && session.branch !== lastCommit.branch) lastCommit = null;
    render();
  }

  function render() {
    const changes = session.changes();
    renderCommitNote();
    renderConflicts();
    renderList(changes);
    syncAuto();
  }

  function renderCommitNote() {
    clear(commitNote);
    const on = !!(lastCommit && lastCommit.prUrl);
    commitNote.hidden = !on;
    if (!on) return;
    commitNote.append(
      h("p", { class: "ws-commit-note-text" }, "Committed to ", pathText(lastCommit.branch), "."),
      h("a", { class: "ws-btn ws-btn-primary", href: lastCommit.prUrl }, "Open a pull request"),
      ...(lastCommit.recovered ? [h("p", { class: "ws-hint", text: "The answer was lost, but the commit landed." })] : []));
  }

  function syncAuto() {
    const block = session.autoCommitBlock();
    autoCheck.checked = !!session.autoCommit;
    autoCheck.disabled = !!block;
    autoReason.hidden = !block;
    autoReason.textContent = block;
  }

  // ---------------------------------------------------------------- the list of changes

  function renderList(changes) {
    clear(list);
    const plain = changes.filter((c) => !c.conflict); // conflicted files sit in their own box
    list.hidden = !plain.length;
    empty.hidden = !!changes.length;
    actions.hidden = !changes.length;
    for (const c of plain) list.append(changeRow(c));
  }

  function changeRow(c) {
    const mark = letter(c);
    const name = c.op === "rename" && c.from ? [pathText(c.from), " -> ", pathText(c.path)] : pathText(c.path);
    return h("div", { class: "ws-changes-row" },
      h("button", { type: "button", class: "ws-changes-main", title: "Show the diff", onclick: () => showDiffOf(c.path) },
        h("span", { class: "ws-changes-letter ws-letter-" + mark.toLowerCase(), text: mark, title: LETTER_WORDS[mark], "aria-hidden": "true" }),
        h("span", { class: "ws-changes-path" }, name),
        c.workflow ? h("span", { class: "ws-changes-chip", text: "Workflow" }) : null,
        c.longLine ? h("span", { class: "ws-changes-chip", text: "Long line" }) : null,
        c.fromTerminal ? h("span", { class: "ws-changes-chip", text: "From terminal" }) : null),
      button("Diff", () => showDiffOf(c.path)),
      button("Discard", () => discardOne(c.path)));
  }

  async function showDiffOf(path) {
    const d = await session.diff(path);
    if (d) editor.showDiff(d.original, d.modified, { longLine: d.longLine });
  }

  async function discardOne(path) {
    const v = await dialog("Discard changes", h("div", { class: "ws-dialog-body" },
      h("p", { class: "ws-dialog-path" }, pathText(path)),
      h("p", { class: "ws-hint", text: "Discard your changes to this file? This cannot be undone." })),
      [{ label: "Discard", value: "yes", danger: true }, { label: "Cancel", value: null }]);
    if (v !== "yes") return;
    try { await session.drop(path); } catch (e) { announce(e.message); }
  }

  async function discardAll() {
    const v = await dialog("Discard every change", h("div", { class: "ws-dialog-body" },
      h("p", { class: "ws-hint", text: "Every draft on this branch is thrown away. This cannot be undone." })),
      [{ label: "Discard all", value: "yes", danger: true }, { label: "Cancel", value: null }]);
    if (v !== "yes") return;
    try { await session.discardAll(); } catch (e) { announce(e.message); }
  }

  /** Every draft as one JSON file the person keeps. */
  function exportDrafts() {
    const rows = [...session.drafts.values()].map((d) => ({ branch: d.branch, path: d.path, op: d.op, from: d.from || null, text: d.text || "" }));
    if (!rows.length) { announce("There are no drafts to export."); return; }
    const url = URL.createObjectURL(new Blob([JSON.stringify(rows, null, 2)], { type: "application/json" }));
    const a = h("a", { href: url, download: "codehaven-drafts.json" });
    document.body.append(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
    announce(rows.length === 1 ? "1 draft exported." : rows.length + " drafts exported.");
  }

  // ---------------------------------------------------------------- conflicts

  function renderConflicts() {
    clear(conflictBox);
    const paths = [...session.conflicts.keys()].sort((a, b) => a.localeCompare(b));
    conflictBox.hidden = !paths.length;
    if (editing && !session.conflicts.has(editing)) editing = null;
    if (!paths.length) return;
    const n = paths.length;
    conflictBox.append(h("p", { class: "ws-note" },
      n === 1
        ? "The branch moved and 1 file changed on both sides. Choose what to keep for it."
        : "The branch moved and " + n + " files changed on both sides. Choose what to keep for each."));
    for (const p of paths) conflictBox.append(conflictRow(p));
    conflictBox.append(h("div", { class: "ws-conflicts-actions" },
      button("Commit to a new branch from where I started", () => {
        const first = session.conflicts.values().next().value;
        openCommitDialog(session, editor, { fromStart: first ? first.oldHead : null });
      })));
  }

  function conflictRow(path) {
    const on = editing === path;
    const editBtn = button("Edit result", () => toggleEdit(path));
    editBtn.setAttribute("aria-pressed", String(on));
    return h("div", { class: "ws-conflict" },
      h("div", { class: "ws-conflict-main" },
        pathText(path),
        button("Keep mine", () => settle(path, "mine")),
        button("Take theirs", () => settle(path, "theirs")),
        editBtn),
      on ? editArea(path) : null);
  }

  async function settle(path, choice) {
    try { await session.resolve(path, choice); } catch (e) { announce(e.message); }
  }

  /** The two sides of a conflict as text: theirs from the bytes the server sent. */
  function conflictTexts(path) {
    const d = session.drafts.get(path);
    const mine = d && d.op !== "delete" ? d.text || "" : "";
    const c = session.conflicts.get(path);
    const theirs = c && c.theirs ? inspect(c.theirs).text.replaceAll("\r\n", "\n") : "";
    return { mine, theirs };
  }

  function toggleEdit(path) {
    if (editing === path) {
      editing = null;
      editor.hideDiff();
    } else {
      editing = path;
      const t = conflictTexts(path);
      editor.showDiff(t.theirs, t.mine, { longLine: longestLine(t.theirs) > LONG_LINE || longestLine(t.mine) > LONG_LINE });
    }
    render();
  }

  function editArea(path) {
    const t = conflictTexts(path);
    const text = h("textarea", { class: "ws-conflict-text", rows: "10", "aria-label": "The text to keep", spellcheck: "false" });
    text.value = t.mine;
    return h("div", { class: "ws-conflict-edit" },
      h("p", { class: "ws-hint", text: "Their version is on the left, yours on the right." }),
      text,
      button("Use this text", async () => {
        try {
          await session.resolve(path, "edited", text.value);
        } catch (e) {
          announce(e.message);
          return;
        }
        editing = null;
        editor.hideDiff();
      }, { class: "ws-btn ws-btn-primary" }));
  }
}

// ---------------------------------------------------------------- the commit dialog

/** Why this tab cannot commit, in plain words. */
function commitBlockReason(session) {
  if (session.locked) return "This tab is out of date. Load the page again.";
  if (!session.boot) return "This tab cannot commit.";
  if (!session.boot.can_write) return "This repository is read only for you here.";
  if (!session.editable) return "This branch is open in another tab. Close that tab to commit here.";
  if (session.cut) return "This repository has too many files to commit from here.";
  return "This tab cannot commit.";
}

export async function openCommitDialog(session, editor, opts) {
  const o = opts || {};
  if (!session.canCommit) {
    await dialog("Cannot commit", h("div", { class: "ws-dialog-body" },
      h("p", { class: "ws-hint", text: commitBlockReason(session) })),
      [{ label: "Close", value: null }]);
    return;
  }
  const changes = session.changes();
  if (!changes.length) { announce("There is nothing to commit."); return; }

  const title = o.fromStart ? "Commit to a new branch" : "Commit";
  const shown = changes.slice(0, 20);
  const message = h("textarea", { class: "ws-commit-msg", rows: "3", "aria-label": "Commit message", spellcheck: "false" });
  message.value = changes.length === 1 ? "Update " + changes[0].path.split("/").pop() : "Update " + changes.length + " files";

  const directOk = !!session.boot.direct_commit && !o.fromStart;
  const direct = h("input", { type: "radio", name: "ws-where", id: "ws-where-direct", checked: directOk || null, disabled: !directOk || null });
  const directNote = h("span", { class: "ws-hint", text: !session.boot.direct_commit ? "This branch is protected." : "The branch moved after you started." });
  directNote.hidden = directOk;
  const newRadio = h("input", { type: "radio", name: "ws-where", id: "ws-where-new", checked: !directOk || null });
  const branchInput = h("input", { type: "text", class: "ws-input", "aria-label": "New branch name", autocomplete: "off", spellcheck: "false" });
  branchInput.value = session.boot.suggested_branch || ((session.boot.login || "you") + "/patch");
  const branchErr = h("p", { class: "ws-error" });
  branchErr.hidden = true;

  const wfCheck = h("input", { type: "checkbox", id: "ws-workflow-confirm" });
  const wfWarn = h("div", { class: "ws-commit-workflow" },
    h("p", { class: "ws-note", text: "This commit changes a workflow file. CI will run what you wrote, with this repository's and its owner's secrets." }),
    h("label", { class: "ws-check", for: "ws-workflow-confirm" }, wfCheck, h("span", { text: "I mean to change what CI runs" })));
  let wfShown = changes.some((c) => c.workflow);
  wfWarn.hidden = !wfShown;

  // Files that came from the terminal, whatever their path: their changes in full, and a tick.
  // Text typed at a fake prompt could be in them, and CI may run them with secrets.
  const fromTerm = changes.filter((c) => c.fromTerminal);
  const termCheck = h("input", { type: "checkbox", id: "ws-terminal-confirm" });
  const termWarn = fromTerm.length ? h("div", { class: "ws-commit-terminal" },
    h("p", { class: "ws-note", text: (fromTerm.length === 1 ? "1 file came" : fromTerm.length + " files came") +
      " from the terminal. Read every change before you commit: CI may run this code with secrets." }),
    fromTerm.map((c) => terminalDiff(session, c)),
    h("label", { class: "ws-check", for: "ws-terminal-confirm" }, termCheck, h("span", { text: "I read the changes from the terminal" }))) : null;

  // Which workflows this commit starts, with each one's branch and path filters applied by
  // the server. Asked again when the target branch changes. The commit can go on without it.
  const runNote = h("div", { class: "ws-commit-runs", role: "status" });
  const paths = touchedPaths(changes);
  const startSha = o.fromStart || session.head;
  let askedFor = null;
  let askTimer = 0;

  async function askRuns() {
    clearTimeout(askTimer);
    const isNew = newRadio.checked;
    const target = isNew ? branchInput.value.trim() : session.branch;
    const key = (isNew ? "new:" : "at:") + target;
    if (key === askedFor) return;
    askedFor = key;
    if (!startSha || !target) { clear(runNote); return; }
    let list = null;
    try {
      list = (await session.api.json("/workflows", workflowQuery(session.branch, startSha, target, isNew, paths))).workflows || [];
    } catch { list = null; }
    if (askedFor === key) showRuns(list);
  }

  function showRuns(list) {
    clear(runNote);
    if (!list) return;
    const pushing = list.filter((w) => w.on_push);
    if (!pushing.length) {
      runNote.append(h("p", { class: "ws-hint", text: list.length ? NOTHING_RUNS : NO_WORKFLOWS }));
    } else if (session.boot.ci && session.boot.ci.paused) {
      runNote.append(h("p", { class: "ws-note", text: session.boot.ci.reason || "Checks are paused on this server." }));
    } else {
      runNote.append(
        h("p", { class: "ws-hint", text: "These workflows will run: " + pushing.map((w) => w.name || w.path).join(", ") }),
        h("p", { class: "ws-hint", text: "They can use this repository's secrets and its owner's secrets." }));
    }
  }

  const status = h("p", { class: "ws-hint", text: "Committing..." });
  const err = h("p", { class: "ws-error" });
  status.hidden = err.hidden = true;
  let busy = false;
  const commitBtn = button("Commit", onCommit, { class: "ws-btn ws-btn-primary" });
  const cancelBtn = button("Cancel", close);

  const d = h("dialog", { class: "ws-dialog", "aria-label": title });
  d.append(
    h("h2", { class: "ws-dialog-title", text: title }),
    h("div", { class: "ws-dialog-body" },
      h("p", { class: "ws-hint", text: changes.length === 1 ? "1 file" : changes.length + " files" }),
      h("div", { class: "ws-commit-paths" },
        shown.map((c) => h("div", { class: "ws-commit-path" },
          c.op === "rename" && c.from ? [pathText(c.from), " -> ", pathText(c.path)] : pathText(c.path))),
        changes.length > shown.length ? h("div", { class: "ws-commit-path", text: "and " + (changes.length - shown.length) + " more" }) : null),
      message,
      h("div", { class: "ws-commit-where", role: "radiogroup", "aria-label": "Where to commit" },
        h("label", { class: "ws-radio", for: "ws-where-direct" }, direct,
          h("span", { class: "ws-radio-text" }, "Commit to ", pathText(session.branch)),
          directNote),
        h("label", { class: "ws-radio", for: "ws-where-new" }, newRadio, h("span", { class: "ws-radio-text", text: "Commit to a new branch" })),
        h("div", { class: "ws-commit-new" }, branchInput, branchErr)),
      wfWarn,
      termWarn,
      runNote,
      err,
      status),
    h("div", { class: "ws-dialog-actions" }, cancelBtn, commitBtn));

  function update() {
    // A typed branch name is asked about once typing pauses.
    clearTimeout(askTimer);
    askTimer = setTimeout(askRuns, 400);
    cancelBtn.disabled = busy;
    commitBtn.disabled = busy
      || !message.value.trim()
      || (wfShown && !wfCheck.checked)
      || (fromTerm.length > 0 && !termCheck.checked)
      || (newRadio.checked && !branchInput.value.trim());
  }

  function close() {
    if (busy) return;
    d.close();
  }

  async function onCommit() {
    if (busy || commitBtn.disabled) return;
    busy = true;
    update();
    err.hidden = true;
    branchErr.hidden = true;
    status.hidden = false;
    try {
      const res = await session.commit({
        message: message.value,
        newBranch: newRadio.checked ? branchInput.value.trim() : null,
        workflowConfirmed: wfShown && wfCheck.checked,
        fromStart: o.fromStart,
      });
      d.close();
      announce("Committed " + res.sha.slice(0, 7));
    } catch (e) {
      busy = false;
      status.hidden = true;
      if (e.code === "head_moved") {
        d.close();
        try { await session.checkHead(); } catch { /* the view shows what it has */ }
        announce("The branch moved. Your drafts are kept.");
        return;
      }
      if (e.code === "branch_exists") {
        branchErr.textContent = "A branch with that name already exists.";
        branchErr.hidden = false;
        newRadio.checked = true;
        branchInput.focus();
        update();
        return;
      }
      if (e.code === "protected") {
        direct.disabled = true;
        directNote.textContent = "This branch is protected.";
        directNote.hidden = false;
        newRadio.checked = true;
        update();
        return;
      }
      if (e.code === "workflow_confirm") {
        wfShown = true;
        wfWarn.hidden = false;
        update();
        return;
      }
      err.textContent = e.message;
      err.hidden = false;
      update();
    }
  }

  message.addEventListener("input", update);
  branchInput.addEventListener("input", () => { branchErr.hidden = true; update(); });
  wfCheck.addEventListener("change", update);
  termCheck.addEventListener("change", update);
  for (const r of [direct, newRadio]) r.addEventListener("change", update);
  d.addEventListener("cancel", (e) => { if (busy) e.preventDefault(); });
  d.addEventListener("close", () => d.remove());
  d.addEventListener("close", () => clearTimeout(askTimer));
  document.body.append(d);
  d.showModal();
  update();
  askRuns();
  message.focus();
}

/** One terminal file's change as a line list, shown in the commit dialog, as text only. */
function terminalDiff(session, c) {
  const body = h("div", { class: "ws-udiff ws-commit-diff" });
  const box = h("details", { class: "ws-commit-term-file", open: true },
    h("summary", { class: "ws-commit-path" }, pathText(c.path)), body);
  session.diff(c.path).then((d) => {
    if (!d) return;
    if (d.longLine) body.append(h("p", { class: "ws-hint", text: "Long line, shown wrapped." }));
    const a = d.original.split("\n");
    const b = d.modified.split("\n");
    const lines = a.length <= 2000 && b.length <= 2000 ? lcsDiff(a, b).filter((l) => l.k !== "same") : b.map((t) => ({ k: "add", t }));
    for (const l of lines.slice(0, 400)) body.append(h("div", { class: "ws-" + l.k }, (l.k === "add" ? "+ " : "- ") + l.t));
    if (lines.length > 400) body.append(h("p", { class: "ws-hint", text: "And " + (lines.length - 400) + " more lines. Open the file to read them all." }));
    if (!lines.length) body.append(h("p", { class: "ws-hint", text: c.op === "delete" ? "The file is deleted." : "No line changed." }));
  }).catch(() => body.append(h("p", { class: "ws-hint", text: "The change could not be shown. Open the file to read it." })));
  return box;
}
