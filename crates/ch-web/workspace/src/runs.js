// The Runs panel: the runs of this branch and this commit, one run's jobs, and one job's log
// streamed step by step. Log text reaches the page only as text nodes, a class from the server
// only when it is on this file's own allowlist, and a path mentioned in a log line becomes a
// jump button only when that exact path is in the tree.

import { h, icon, pathText, clear, announce, button, dialog } from "./ui.js";
import { displayPath } from "./bytes.js";
import { NOTHING_RUNS, NO_WORKFLOWS, workflowQuery } from "./session.js";

// The only class names a log line's spans may carry.
const ANSI_CLASSES = new Set(["ansi-bold", "ansi-dim", "ansi-italic", "ansi-underline"]);
for (const colour of ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"]) {
  ANSI_CLASSES.add("ansi-" + colour);
  ANSI_CLASSES.add("ansi-bright-" + colour);
  ANSI_CLASSES.add("ansi-bg-" + colour);
}

// A `path:line` mention in a log line.
const JUMP = /([\w./-]+):(\d+)/g;

const enc = (v) => encodeURIComponent(String(v));
const short = (sha) => (sha ? String(sha).slice(0, 7) : "");

export function mountRuns(session, el, editor) {
  const known = new Map(); // run id -> the status last seen, so a finish can be said out loud
  let runs = [];
  let canWrite = false;
  let paused = null; // what /runs last said about CI being paused
  let selectedRun = null; // run id
  let selectedJob = null; // job id
  let detail = null; // the /runs/<id> answer
  let waitSha = null; // a commit waiting for its workflows to start
  let waitPush = null; // that commit's push: { target, isNew, paths }
  let noteSha = null; // the commit the waiting note or its verdict is about
  let waitText = "";
  let waitBad = []; // workflows with an error: [{ path, error }]
  let waitTimer = 0;
  let pollTimer = 0;
  let stopped = false;
  let jobCtrl = null; // the AbortController of the open log stream
  let logGen = 0; // bumped whenever the open log stream is closed for good
  let stepEls = new Map(); // step number -> its section
  let logBox = null; // where the open job's steps and lines are shown
  let detailState = null;

  // ---------------------------------------------------------------- the panel's parts

  const pausedNote = h("div", { class: "ws-note", role: "status", hidden: true });
  const waitNote = h("div", { class: "ws-note", role: "status", hidden: true });
  const runBtn = button("Run a workflow", openDispatch, { icon: "runs" });
  const tools = h("div", { class: "ws-runs-tools" }, runBtn);
  const listBox = h("div", { class: "ws-runs-list" });
  const detailBox = h("div", { class: "ws-run-detail", hidden: true });
  clear(el).append(pausedNote, waitNote, tools, listBox, detailBox);

  // ---------------------------------------------------------------- notes

  function renderNotes() {
    const ci = (paused && paused.paused) || (session.boot && session.boot.ci && session.boot.ci.paused);
    const reason = (paused && paused.paused && paused.reason)
      || (session.boot && session.boot.ci && session.boot.ci.reason)
      || "Workflows are paused on this server.";
    pausedNote.hidden = !ci;
    if (ci) {
      clear(pausedNote);
      pausedNote.append(icon("alert"), h("span", { text: reason }));
    }

    clear(waitNote);
    if (waitSha) {
      waitNote.hidden = false;
      waitNote.append(h("span", { text: "Waiting for workflows for " + short(waitSha) }));
    } else if (waitText || waitBad.length) {
      // What was found out, then each workflow file that could not be read, with its error.
      waitNote.hidden = false;
      const said = h("div", {}, waitText ? h("div", { text: waitText }) : null);
      for (const w of waitBad) said.append(h("div", {}, pathText(w.path), h("span", { text: ": " + w.error })));
      waitNote.append(icon("alert"), said);
    } else waitNote.hidden = true;

    runBtn.hidden = !(canWrite && session.head);
  }

  // ---------------------------------------------------------------- the list of runs

  function renderList() {
    clear(listBox);
    if (!session.boot) return;
    const mine = session.head ? runs.filter((r) => r.head_sha === session.head) : [];
    const rest = runs.filter((r) => !session.head || r.head_sha !== session.head);
    if (mine.length) listBox.append(group("This commit", mine));
    if (rest.length) listBox.append(group("This branch", rest));
    if (!runs.length) listBox.append(h("p", { class: "ws-hint", text: "No runs on this branch yet." }));
  }

  function group(title, items) {
    const g = h("div", { class: "ws-runs-group" }, h("h3", { class: "ws-runs-group-title", text: title }));
    for (const r of items) g.append(runRow(r));
    return g;
  }

  function runRow(run) {
    const main = h("button",
      { type: "button", class: "ws-run-main", onclick: () => selectRun(run.id) },
      mark(run),
      h("span", { class: "ws-run-name", text: run.workflow || "workflow" }),
      h("span", { class: "ws-run-num", text: "#" + run.number }),
      h("span", { class: "ws-run-sha", text: short(run.head_sha) }));
    return h("div", { class: "ws-run-row" + (selectedRun === run.id ? " ws-on" : "") }, main,
      h("span", { class: "ws-run-actions" },
        canWrite && run.status === "completed" ? button("Re-run", () => rerun(run), { class: "ws-btn ws-run-btn" }) : null,
        canWrite && run.status === "completed" && run.conclusion === "failure" ? button("Re-run failed", () => rerun(run, true), { class: "ws-btn ws-run-btn" }) : null,
        canWrite && run.status !== "completed" ? button("Cancel", () => cancelRun(run), { class: "ws-btn ws-run-btn" }) : null));
  }

  /** The colour of a run's, job's or step's mark, and the word for a screen reader. */
  function stateOf(it) {
    if (it.status === "completed") {
      if (it.conclusion === "success") return { cls: "ok", label: "success" };
      if (it.conclusion === "failure") return { cls: "danger", label: "failure" };
      return { cls: "muted", label: it.conclusion || "done" };
    }
    if (it.status === "in_progress") return { cls: "warn", label: "running" };
    return { cls: "muted", label: it.status || "queued" };
  }

  /** A small circle, plus a label only a screen reader hears. */
  function mark(it) {
    const s = stateOf(it);
    return h("span", { class: "ws-mark" },
      h("span", { class: "ws-mark-dot ws-dot-" + s.cls, "aria-hidden": "true" }),
      h("span", { class: "ws-sr", text: s.label }));
  }

  // ---------------------------------------------------------------- polling

  async function poll() {
    if (stopped || document.hidden || !session.boot) return;
    let res;
    try {
      res = await session.api.json("/runs", session.head ? { branch: session.branch, sha: session.head } : { branch: session.branch });
    } catch (e) {
      if (e.code !== "locked") schedule(); // the lock handler stops the panel
      return;
    }
    runs = (res.runs || []).sort((a, b) => String(b.created_at || "").localeCompare(String(a.created_at || "")));
    canWrite = !!res.can_write;
    paused = res.paused || null;
    for (const r of runs) {
      const was = known.get(r.id);
      if (was && was !== "completed" && r.status === "completed") {
        announce((r.workflow || "Run") + " finished: " + (r.conclusion || "done"));
      }
      known.set(r.id, r.status);
    }
    if (waitSha && runs.some((r) => r.head_sha === waitSha)) stopWaiting();
    renderList();
    renderNotes();
    syncDetail();
    schedule();
  }

  /** Two seconds while anything listed is still running, five seconds otherwise. */
  function schedule() {
    clearTimeout(pollTimer);
    if (stopped) return;
    pollTimer = setTimeout(poll, runs.some((r) => r.status !== "completed") ? 2000 : 5000);
  }

  /** No polling while hidden; the moment the tab is back, one poll at once. */
  function onVisibility() {
    if (stopped) return;
    if (document.hidden) clearTimeout(pollTimer);
    else poll();
  }

  /** Another branch (or a first boot) means another set of runs. */
  function reset() {
    if (stopped) return;
    // A commit to a new branch boots onto the committed sha: its wait, or what was found out
    // about it, still stands.
    if (!noteSha || session.head !== noteSha) stopWaiting();
    runs = [];
    known.clear();
    canWrite = !!(session.boot && session.boot.can_write);
    paused = null;
    detail = null;
    selectedRun = null;
    selectedJob = null;
    closeLog();
    renderList();
    renderDetail();
    renderNotes();
    poll();
  }

  /** The selected run's own line in the list is fresher than its detail page: copy its state. */
  function syncDetail() {
    if (!detail || selectedRun === null || selectedRun === undefined) return;
    const live = runs.find((r) => r.id === selectedRun);
    if (live) {
      detail.run = live;
      if (detailState) detailState.textContent = stateOf(live).label;
    }
  }

  // ---------------------------------------------------------------- a fresh commit waiting for CI

  function onCommitted(data) {
    if (stopped) return;
    waitSha = data && data.sha ? data.sha : null;
    waitPush = data && data.pushed ? data.pushed : null;
    noteSha = waitSha;
    waitText = "";
    waitBad = [];
    clearTimeout(waitTimer);
    if (!waitSha) return;
    waitTimer = setTimeout(checkWaiting, 10000);
    renderNotes();
    listens(waitSha);
    // A commit to a new branch polls again once that branch has booted.
    if (!data || data.branch === session.branch) poll();
  }

  /** The workflows of the commit `sha`, with the filters of its push applied. */
  function workflowsOf(sha) {
    const p = waitPush;
    return session.api.json("/workflows", p ? workflowQuery(p.target, sha, p.target, p.isNew, p.paths) : { branch: session.branch, sha });
  }

  /** At once: when no workflow listens to this push, say so instead of waiting for nothing. */
  async function listens(sha) {
    let res;
    try {
      res = await workflowsOf(sha);
    } catch {
      return; // the ten second check asks again
    }
    if (stopped || waitSha !== sha) return;
    const list = (res && res.workflows) || [];
    if (list.some((w) => w.on_push)) return;
    waitSha = null;
    clearTimeout(waitTimer);
    waitText = list.length ? NOTHING_RUNS : NO_WORKFLOWS;
    waitBad = list.filter((w) => w.error).map((w) => ({ path: w.path, error: w.error }));
    renderNotes();
  }

  /** Ten seconds and nothing ran: say why, and offer nothing else. */
  async function checkWaiting() {
    const sha = waitSha;
    if (!sha || stopped) return;
    let res;
    try {
      res = await workflowsOf(sha);
    } catch {
      return; // the next poll still looks for the run
    }
    if (stopped || waitSha !== sha) return;
    waitSha = null; // the wait is over: the verdict stands until the next commit
    const list = (res && res.workflows) || [];
    const ciPaused = (paused && paused.paused) || (session.boot && session.boot.ci && session.boot.ci.paused);
    waitBad = list.filter((w) => w.error).map((w) => ({ path: w.path, error: w.error }));
    if (!list.some((w) => w.on_push)) waitText = list.length ? NOTHING_RUNS : NO_WORKFLOWS;
    else if (ciPaused) waitText = "Workflows are paused on this server.";
    else if (!waitBad.length) waitText = "Still queued. A runner will pick it up.";
    renderNotes();
  }

  function stopWaiting() {
    waitSha = null;
    waitPush = null;
    noteSha = null;
    waitText = "";
    waitBad = [];
    clearTimeout(waitTimer);
    renderNotes();
  }

  // ---------------------------------------------------------------- one run and its jobs

  async function selectRun(id) {
    if (stopped) return;
    selectedRun = id;
    selectedJob = null;
    detail = null;
    closeLog();
    renderList();
    renderDetail();
    try {
      const res = await session.api.json("/runs/" + enc(id));
      if (stopped || selectedRun !== id) return;
      detail = res;
      renderDetail();
    } catch (e) {
      if (e.code === "locked" || stopped || selectedRun !== id) return;
      clear(detailBox).append(h("p", { class: "ws-error", text: e.message }));
      detailBox.hidden = false;
    }
  }

  function renderDetail() {
    clear(detailBox);
    detailState = null;
    logBox = h("div", { class: "ws-log", hidden: true });
    stepEls = new Map();
    if (!detail) {
      detailBox.hidden = true;
      return;
    }
    detailBox.hidden = false;
    const run = detail.run || {};
    detailState = h("span", { class: "ws-detail-state" });
    detailBox.append(
      h("div", { class: "ws-detail-head" },
        h("strong", { text: run.workflow || "Run" }),
        h("span", { class: "ws-run-num", text: "#" + run.number }),
        h("span", { class: "ws-run-sha", text: short(run.head_sha) }),
        detailState),
      h("h3", { class: "ws-runs-group-title", text: "Jobs" }));
    detailState.textContent = stateOf(run).label;
    const jobs = detail.jobs || [];
    for (const j of jobs) detailBox.append(jobRow(j));
    if (!jobs.length) detailBox.append(h("p", { class: "ws-hint", text: "No jobs yet." }));
    detailBox.append(logBox);
  }

  function jobRow(job) {
    const state = h("span", { class: "ws-job-state" });
    state.textContent = stateOf(job).label;
    return h("button",
      { type: "button", class: "ws-job-row" + (selectedJob === job.id ? " ws-on" : ""), onclick: () => selectJob(job) },
      mark(job), h("span", { class: "ws-job-name", text: job.name || "job" }), state);
  }

  function selectJob(job) {
    selectedJob = job.id;
    closeLog();
    renderDetail();
    startLog(job);
  }

  // ---------------------------------------------------------------- one job's log, streamed by step

  /** Only one stream is ever open. Closing it aborts the fetch and stops the reconnects. */
  function closeLog() {
    logGen++;
    if (jobCtrl) {
      jobCtrl.abort();
      jobCtrl = null;
    }
  }

  function startLog(job) {
    const runId = selectedRun;
    const gen = ++logGen;
    const ctrl = new AbortController();
    jobCtrl = ctrl;
    logBox.hidden = false;
    for (const s of job.steps || []) makeStep(s);

    let offset = 0; // the byte offset reached
    let nextLine = 1; // the next line number to expect
    let finished = false;
    let conn = null; // the current connection, so a retry can end it alone

    const onEvent = (ev) => {
      if (gen !== logGen) return;
      if (ev.event === "log") {
        let d;
        try { d = JSON.parse(ev.data); } catch { return; }
        if (!d || !Array.isArray(d.lines) || d.step === null || d.step === undefined) return;
        if (ev.id) {
          const at = Number(ev.id);
          if (Number.isFinite(at)) offset = at;
        }
        const start = Number.isFinite(d.line) ? d.line : nextLine;
        const sec = makeStep({ number: d.step });
        const stick = nearBottom();
        // A reconnect can repeat lines: skip the ones already on screen.
        for (let i = Math.max(0, nextLine - start); i < d.lines.length; i++) addLine(sec, start + i, d.lines[i]);
        nextLine = Math.max(nextLine, start + d.lines.length);
        if (stick) el.scrollTop = el.scrollHeight;
      } else if (ev.event === "steps") {
        let arr;
        try { arr = JSON.parse(ev.data); } catch { return; }
        if (Array.isArray(arr)) for (const s of arr) makeStep(s);
      } else if (ev.event === "done") {
        finished = true;
      } else if (ev.event === "retry") {
        if (conn) conn.abort(); // the settle below reconnects after two seconds
      }
    };

    const attempt = () => {
      if (gen !== logGen || finished || stopped) return;
      conn = new AbortController();
      const onAbort = () => conn.abort();
      ctrl.signal.addEventListener("abort", onAbort);
      session.api.stream("/runs/" + enc(runId) + "/jobs/" + enc(job.id) + "/log/stream", { offset, line: nextLine }, onEvent, conn.signal)
        .catch(() => {})
        .then(() => {
          ctrl.signal.removeEventListener("abort", onAbort);
          // A drop: pick the stream back up at the offset, from the next line.
          if (gen === logGen && !finished && !stopped) setTimeout(attempt, 2000);
        });
    };
    attempt();
  }

  /** One collapsible step. A failed step opens itself; so does one that is running. */
  function makeStep(info) {
    let sec = stepEls.get(info.number);
    if (!sec) {
      const dot = h("span", { class: "ws-mark-dot", "aria-hidden": "true" });
      const name = h("span", { class: "ws-log-step-name", text: info.name || "Step " + info.number });
      const state = h("span", { class: "ws-log-step-state" });
      const lines = h("div", { class: "ws-log-lines" });
      const det = h("details", { class: "ws-log-step" }, h("summary", { class: "ws-log-step-head" }, dot, name, state), lines);
      sec = { det, dot, name, state, lines, opened: false };
      stepEls.set(info.number, sec);
      logBox.append(det);
    }
    if (info.name) sec.name.textContent = info.name;
    if (info.status || info.conclusion) {
      const s = stateOf(info);
      sec.state.textContent = s.label;
      sec.dot.className = "ws-mark-dot ws-dot-" + s.cls;
      if (info.status === "in_progress") sec.det.open = true;
      if (info.conclusion === "failure" && !sec.opened) {
        sec.opened = true;
        sec.det.open = true;
      }
    }
    return sec;
  }

  function addLine(sec, num, spans) {
    const text = h("span", { class: "ws-log-text" });
    for (const sp of Array.isArray(spans) ? spans : []) {
      if (!sp || typeof sp.t !== "string") continue;
      const s = h("span", {});
      for (const c of Array.isArray(sp.c) ? sp.c : []) if (ANSI_CLASSES.has(c)) s.classList.add(c);
      fillJump(s, sp.t);
      text.append(s);
    }
    sec.lines.append(h("div", { class: "ws-log-line" }, h("span", { class: "ws-log-num", text: String(num) }), text));
  }

  /** A `path:line` mention becomes a jump button only when that path is in the tree. */
  function fillJump(span, text) {
    const found = [...text.matchAll(JUMP)].filter((m) => session.tree && session.tree.has(m[1]));
    if (!found.length) {
      span.textContent = text;
      return;
    }
    let at = 0;
    for (const m of found) {
      if (m.index > at) span.append(text.slice(at, m.index));
      span.append(h("button",
        { type: "button", class: "ws-log-jump", title: "Open " + displayPath(m[1]), onclick: () => editor.openPath(m[1], Number(m[2])) },
        m[0]));
      at = m.index + m[0].length;
    }
    if (at < text.length) span.append(text.slice(at));
  }

  function nearBottom() {
    return el.scrollHeight - el.scrollTop - el.clientHeight < 48;
  }

  // ---------------------------------------------------------------- re-run and cancel

  async function rerun(run, failedOnly) {
    const name = (run.workflow || "Run") + " #" + run.number + " from " + short(run.head_sha);
    const ok = await dialog(failedOnly ? "Re-run failed jobs" : "Re-run",
      h("div", { class: "ws-dialog-body" },
        h("p", { text: failedOnly ? "Run only the failed jobs of " + name + " again?" : "Run " + name + " again?" })),
      [{ label: "Re-run", value: true, primary: true }, { label: "Cancel", value: null }]);
    if (!ok) return;
    try {
      const res = await session.api.post("/runs/" + enc(run.id) + "/rerun", { failed_only: !!failedOnly });
      if (res && res.run) selectRun(res.run.id);
    } catch (e) {
      announce(e.message);
    }
  }

  async function cancelRun(run) {
    try {
      await session.api.post("/runs/" + enc(run.id) + "/cancel", {});
      poll();
    } catch (e) {
      announce(e.message);
    }
  }

  // ---------------------------------------------------------------- run a workflow by hand

  function openDispatch() {
    if (!session.head || !session.boot) return;
    const err = h("p", { class: "ws-error", role: "alert", hidden: true });
    const sel = h("select", { class: "ws-input", "aria-label": "Workflow" });
    const fields = h("div", { class: "ws-disp-fields" });
    const hint = h("p", { class: "ws-hint" });
    const cancelB = button("Cancel", done);
    const runB = button("Run", onRun, { class: "ws-btn ws-btn-primary", disabled: true });
    const d = h("dialog", { class: "ws-dialog", "aria-label": "Run a workflow" });
    let workflows = [];
    let controls = []; // the workflow's inputs and their elements, in order
    let current = null;

    d.append(
      h("h2", { class: "ws-dialog-title", text: "Run a workflow" }),
      h("div", { class: "ws-dialog-body" }, sel, fields, err, hint),
      h("div", { class: "ws-dialog-actions" }, cancelB, runB));
    d.addEventListener("cancel", (e) => { e.preventDefault(); done(); });
    document.body.append(d);
    d.showModal();
    load();

    function done() {
      d.close();
      d.remove();
    }

    async function load() {
      let res;
      try {
        res = await session.api.json("/workflows", { branch: session.branch, sha: session.head });
      } catch (e) {
        err.hidden = false;
        err.textContent = e.message;
        return;
      }
      if (!d.open) return;
      workflows = ((res && res.workflows) || []).filter((w) => w.dispatch);
      if (!workflows.length) {
        hint.textContent = "No workflow here can be run by hand.";
        return;
      }
      for (let i = 0; i < workflows.length; i++) {
        sel.append(h("option", { value: String(i), text: workflows[i].name || workflows[i].path || "workflow" }));
      }
      sel.addEventListener("change", pick);
      runB.disabled = false;
      pick();
    }

    function pick() {
      current = workflows[Number(sel.value)] || null;
      controls = [];
      clear(fields);
      err.hidden = true;
      for (const input of (current && current.inputs) || []) {
        const ctl = field(input);
        controls.push({ input, ctl });
        fields.append(ctl);
      }
    }

    /** One workflow input: text, a checkbox for a boolean, a select for a choice. */
    function field(input) {
      const label = (input.name || "input") + (input.required ? " (required)" : "");
      if (input.type === "boolean") {
        return h("label", { class: "ws-disp-check" },
          h("input", { type: "checkbox", checked: !!input.default }),
          h("span", { text: label }));
      }
      if (input.type === "choice") {
        const s = h("select", { class: "ws-input" });
        for (const o of input.options || []) s.append(h("option", { value: String(o), text: String(o), selected: o === input.default ? true : null }));
        return h("label", { class: "ws-disp-field" }, h("span", { class: "ws-disp-label", text: label }), s,
          input.description ? h("span", { class: "ws-hint", text: input.description }) : null);
      }
      return h("label", { class: "ws-disp-field" },
        h("span", { class: "ws-disp-label", text: label }),
        h("input", { type: "text", class: "ws-input", value: input.default === undefined || input.default === null ? "" : String(input.default), placeholder: input.description || null }),
        input.description ? h("span", { class: "ws-hint", text: input.description }) : null);
    }

    async function onRun() {
      if (!current || runB.disabled) return;
      const inputs = controls.map((c) => [c.input.name, c.ctl.type === "checkbox" ? c.ctl.checked : c.ctl.value]);
      runB.disabled = true;
      cancelB.disabled = true;
      try {
        await session.api.post("/dispatch", { workflow: current.path, branch: session.branch, expected_head: session.head, inputs: Object.fromEntries(inputs) });
        done();
        announce("Workflow started.");
        poll();
      } catch (e) {
        runB.disabled = false;
        cancelB.disabled = false;
        err.hidden = false;
        err.textContent = e.code === "head_moved" ? "The branch moved. Reload it to run the newest commit." : e.message;
      }
    }
  }

  // ---------------------------------------------------------------- the end: access lost

  function stop() {
    stopped = true;
    clearTimeout(pollTimer);
    clearTimeout(waitTimer);
    closeLog();
    document.removeEventListener("visibilitychange", onVisibility);
    clear(el);
  }

  // ---------------------------------------------------------------- start

  session.on("boot", reset);
  session.on("head", () => { if (!stopped) poll(); });
  session.on("committed", onCommitted);
  session.on("lock", stop);
  document.addEventListener("visibilitychange", onVisibility);
  renderList();
  poll();

  return { dispose: stop };
}
