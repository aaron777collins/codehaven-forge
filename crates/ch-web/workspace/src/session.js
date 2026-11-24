// The workspace's state: the pinned commit, the tree, the drafts, commits, a branch that moved,
// a lost response, and commit on save (spec sections 8 and 9). No DOM here: views call these
// methods and listen with `on`.

import { Api, ApiError } from "./api.js";
import { Drafts, editLock, releaseEditLocks } from "./drafts.js";
import { inspect, encode, sameBytes, toB64, fromB64, longestLine } from "./bytes.js";

const WORKFLOW = /^(\.codehaven|\.github)\/workflows\//i;
export const LONG_LINE = 5000;
const AUTO_COMMIT_GAP_MS = 20000;

export function isWorkflowPath(p) {
  return WORKFLOW.test(p);
}

/** What the commit dialog and the run panel say when no workflow listens to a push. */
export const NOTHING_RUNS = "Nothing will run. No workflow listens to a push to this branch with these files.";
export const NO_WORKFLOWS = "Nothing will run. This repository has no workflow files.";

/** What a list of files may take up in a URL before it is left out. */
const MAX_CHANGED_B64 = 6000;

/**
 * The /workflows query for a push of `paths` to `target`, starting from `sha` on `branch`.
 * The server applies each workflow's branch and path filters to it. A new branch's push names
 * no earlier commit, so its files are unknown, as they are when the list is too long for a URL:
 * path filters then let the push through, as the push trigger does.
 */
export function workflowQuery(branch, sha, target, isNew, paths) {
  const q = { branch, sha, target };
  if (isNew) q.new = "1";
  else {
    const b64 = toB64(new TextEncoder().encode(paths.join("\0")));
    if (b64.length <= MAX_CHANGED_B64) q.changed_b64 = b64;
  }
  return q;
}

/** Every path a commit body touches: a rename changes its old path and its new one. */
export function touchedPaths(changes) {
  return changes.flatMap((c) => (c.from ? [c.from, c.path] : [c.path]));
}

function metaContent(name) {
  const m = document.querySelector(`meta[name="${name}"]`);
  return m ? m.getAttribute("content") || "" : "";
}

export function readMeta() {
  return {
    key: metaContent("ws-key"),
    csrf: metaContent("ws-csrf"),
    base: metaContent("ws-base"),
    assets: metaContent("ws-assets"),
    repo: metaContent("ws-repo"),
    branch: metaContent("ws-branch"),
    path: metaContent("ws-path"),
  };
}

export class Session {
  constructor(meta) {
    this.meta = meta;
    this.listeners = new Map();
    this.api = new Api(meta, (reason) => this.lock(reason));
    this.files = new Map(); // path -> loaded base file
    this.drafts = new Map(); // path -> draft
    this.timers = new Map();
    this.locked = false;
    this.editable = true;
    this.autoCommit = false;
    this.lastAutoCommit = 0;
    this.committing = false;
    this.conflicts = new Map(); // path -> { theirs: Uint8Array|null }
  }

  on(name, fn) {
    if (!this.listeners.has(name)) this.listeners.set(name, []);
    this.listeners.get(name).push(fn);
  }

  emit(name, data) {
    for (const fn of this.listeners.get(name) || []) {
      try { fn(data); } catch (e) { console.error(e); }
    }
  }

  // ---------------------------------------------------------------- start and branch

  async start(branch) {
    const boot = await this.api.json("/boot", branch ? { branch } : null);
    this.boot = boot;
    this.branch = boot.branch;
    this.head = boot.head;
    this.store = this.store || (await Drafts.open(boot.draft_key, boot.user_id ?? boot.login, boot.repo.id));
    this.editable = await editLock(boot.repo.id, this.branch);
    await this.loadTree();
    await this.loadDrafts();
    this.emit("boot", boot);
    this.emit("change");
    return boot;
  }

  async switchBranch(name) {
    await this.flush();
    releaseEditLocks();
    this.files.clear();
    this.drafts.clear();
    this.conflicts.clear();
    return this.start(name);
  }

  async loadTree() {
    this.tree = new Map();
    this.cut = false;
    if (!this.head) return;
    const t = await this.api.json("/tree", { branch: this.branch, sha: this.head });
    this.cut = !!t.cut;
    // Paths never become object keys: a Map holds them.
    for (const e of t.entries) this.tree.set(e.path ?? null, e);
    this.tree.delete(null);
    this.nonUtf8 = t.entries.filter((e) => e.path === null);
  }

  async loadDrafts() {
    for (const d of await this.store.forBranch(this.branch)) this.drafts.set(d.path, d);
    if (this.drafts.size) await this.checkHead(true);
  }

  /** Can this tab commit right now. */
  get canCommit() {
    return !this.locked && this.editable && this.boot.can_write && !this.cut;
  }

  // ---------------------------------------------------------------- files

  entry(path) {
    return this.tree.get(path) || null;
  }

  /** Paths of every file in the tree plus new drafts, minus drafted deletes, sorted. */
  paths() {
    const out = new Set();
    for (const [p, e] of this.tree) if (e.kind === "blob" || e.kind === "commit") out.add(p);
    for (const [p, d] of this.drafts) {
      if (d.op === "delete") out.delete(p);
      else out.add(p);
      if (d.op === "rename" && d.from) out.delete(d.from);
    }
    return [...out].sort();
  }

  async base(path, sha) {
    const at = sha || this.head;
    const k = `${at}\u0000${path}`;
    if (this.files.has(k)) return this.files.get(k);
    const e = at === this.head ? this.entry(path) : null;
    if (at === this.head && !e) return null;
    const limit = this.phone ? this.boot.limits.file_bytes_phone : this.boot.limits.file_bytes;
    const f = await this.api.file({ branch: this.branch, sha: at, path_b64: toB64(new TextEncoder().encode(path)) });
    const info = f.tooLarge ? { readOnly: true, reason: "This file is too large to edit here." } : inspect(f.bytes);
    if (!info.readOnly && f.bytes.length > limit) Object.assign(info, { readOnly: true, reason: "This file is too large to edit on this device." });
    if (f.mode === "120000") Object.assign(info, { readOnly: true, reason: "This is a symbolic link." });
    if (f.mode === "160000") Object.assign(info, { readOnly: true, reason: "This is a submodule." });
    const file = { path, bytes: f.bytes, mode: f.mode, info, sha: e ? e.sha : null };
    this.files.set(k, file);
    return file;
  }

  /** What the editor shows for a path: the draft if there is one, else the base. */
  async open(path) {
    const d = this.drafts.get(path);
    const fromPath = d && d.op === "rename" ? d.from : path;
    const base = d && d.base && d.base.sha === null ? null : await this.base(fromPath, d ? d.base.head : undefined).catch((e) => {
      if (e instanceof ApiError && e.code === "head_moved") return null;
      throw e;
    });
    const info = d ? { bom: d.info.bom, eol: d.info.eol, readOnly: false, reason: "" } : base ? base.info : { bom: false, eol: "lf", readOnly: false, reason: "" };
    // A read-only text file still shows its text; binary and non-UTF-8 files show nothing.
    const text = d && d.op !== "delete" ? d.text : base ? base.info.text || "" : "";
    let readOnly = info.readOnly || !this.editable || this.locked || (d && d.op === "delete");
    let reason = info.reason || (!this.editable ? "This branch is open in another tab." : "");
    if (this.conflicts.has(path)) { readOnly = true; reason = "Choose how to settle this file first."; }
    return { path, text, info, readOnly, reason, base, draft: d || null, fixable: !!(base && base.info.fixable) };
  }

  // ---------------------------------------------------------------- drafts

  /** The editor's text changed. Saved one second after typing stops. */
  edit(path, text, info) {
    if (!this.editable || this.locked) return;
    const old = this.drafts.get(path);
    const base = old ? null : this.files.get(`${this.head}\u0000${path}`);
    const d = old || {
      v: 1,
      branch: this.branch,
      path,
      op: "put",
      info: { bom: info.bom, eol: info.eol === "none" || info.eol === "mixed" ? "lf" : info.eol },
      base: { head: this.head, sha: base ? base.sha : null, b64: base ? toB64(base.bytes) : null },
      fromTerminal: false,
    };
    if (old && info && info.eol && info.eol !== "mixed") d.info.eol = info.eol === "none" ? d.info.eol : info.eol;
    d.text = text;
    d.updated = Date.now();
    // Changed means different bytes. Back to the base bytes, the draft goes away.
    const baseBytes = d.base.b64 ? fromB64(d.base.b64) : null;
    if (d.op === "put" && baseBytes && sameBytes(encode(text, d.info), baseBytes)) {
      this.drop(path);
      return;
    }
    this.drafts.set(path, d);
    this.schedule(path);
    this.emit("change");
  }

  /** A new, empty file. */
  create(path) {
    if (this.drafts.has(path) || this.tree.has(path)) throw new Error("A file with that name exists.");
    const d = { v: 1, branch: this.branch, path, op: "put", text: "", info: { bom: false, eol: "lf" }, base: { head: this.head, sha: null, b64: null }, fromTerminal: false, updated: Date.now() };
    this.drafts.set(path, d);
    this.schedule(path);
    this.emit("change");
  }

  async remove(path) {
    const d = this.drafts.get(path);
    if (d && d.base.sha === null && d.op !== "rename") {
      this.drop(path);
      return;
    }
    const base = await this.base(d && d.op === "rename" ? d.from : path);
    const nd = { v: 1, branch: this.branch, path, op: "delete", info: base ? { bom: base.info.bom, eol: base.info.eol } : { bom: false, eol: "lf" }, base: { head: this.head, sha: base ? base.sha : null, b64: base ? toB64(base.bytes) : null }, fromTerminal: false, updated: Date.now() };
    if (d) nd.id = d.id;
    this.drafts.set(path, nd);
    this.schedule(path);
    this.emit("change");
  }

  async rename(from, to) {
    if (this.tree.has(to) || this.drafts.has(to)) throw new Error("A file with that name exists.");
    const cur = await this.open(from);
    if (cur.info.readOnly && !cur.draft) throw new Error(cur.info.reason || "This file cannot be renamed here.");
    const old = this.drafts.get(from);
    const baseFrom = old && old.op === "rename" ? old.from : from;
    const base = await this.base(baseFrom).catch(() => null);
    const d = {
      v: 1, branch: this.branch, path: to, op: base ? "rename" : "put", from: base ? baseFrom : undefined, text: cur.text,
      info: { bom: cur.info.bom, eol: cur.info.eol === "none" ? "lf" : cur.info.eol },
      base: { head: this.head, sha: base ? base.sha : null, b64: base ? toB64(base.bytes) : null }, fromTerminal: !!(old && old.fromTerminal), updated: Date.now(),
    };
    if (old) { await this.store.remove(old.id); this.drafts.delete(from); }
    this.drafts.set(to, d);
    this.schedule(to);
    this.emit("change");
  }

  /**
   * Files the person opened and ticked in the terminal's review become drafts marked as coming
   * from the terminal. `bytes` null deletes the file. Each must be editable text that encodes
   * back to exactly these bytes. Returns how many paths changed.
   */
  async takeFromTerminal(list) {
    if (!this.editable || this.locked) throw new ApiError(0, "read_only", "This tab cannot change files.");
    let n = 0;
    for (const { path, bytes } of list) {
      const e = this.entry(path);
      if (bytes === null) {
        if (!e && !this.drafts.has(path)) continue;
        await this.remove(path);
        const d = this.drafts.get(path);
        if (d) {
          d.fromTerminal = true;
          this.schedule(path);
        }
        n++;
        continue;
      }
      const info = inspect(bytes);
      if (info.readOnly || !sameBytes(encode(info.text, info), bytes)) {
        throw new ApiError(0, "invalid", "A file from the terminal is not text the editor can keep byte for byte.");
      }
      const base = e ? await this.base(path) : null;
      const old = this.drafts.get(path);
      const renamed = old && old.op === "rename";
      const d = {
        v: 1,
        branch: this.branch,
        path,
        op: renamed ? "rename" : "put",
        from: renamed ? old.from : undefined,
        text: info.text,
        info: { bom: info.bom, eol: info.eol === "none" ? "lf" : info.eol },
        base: old && old.op !== "delete" ? old.base : { head: this.head, sha: base ? base.sha : null, b64: base ? toB64(base.bytes) : null },
        fromTerminal: true,
        updated: Date.now(),
      };
      if (old && old.id) d.id = old.id;
      const baseBytes = d.base.b64 ? fromB64(d.base.b64) : null;
      n++;
      if (d.op === "put" && baseBytes && sameBytes(encode(d.text, d.info), baseBytes)) {
        await this.drop(path);
        continue;
      }
      this.drafts.set(path, d);
      this.schedule(path);
    }
    this.emit("change");
    return n;
  }

  /** Throws away the draft of one path. */
  async drop(path) {
    const d = this.drafts.get(path);
    this.drafts.delete(path);
    this.conflicts.delete(path);
    clearTimeout(this.timers.get(path));
    if (d && d.id) await this.store.remove(d.id);
    this.emit("change");
  }

  async discardAll() {
    for (const p of [...this.drafts.keys()]) await this.drop(p);
  }

  schedule(path) {
    clearTimeout(this.timers.get(path));
    this.timers.set(path, setTimeout(() => this.persist(path), 1000));
  }

  async persist(path) {
    this.timers.delete(path);
    const d = this.drafts.get(path);
    if (d && this.store && !this.locked) await this.store.save(d);
  }

  /** Writes every pending draft now (before leaving, locking or switching branch). */
  async flush() {
    for (const [p, t] of this.timers) {
      clearTimeout(t);
      await this.persist(p);
    }
  }

  get dirty() {
    return this.timers.size > 0;
  }

  /** What the Changes list shows. */
  changes() {
    return [...this.drafts.values()].sort((a, b) => a.path.localeCompare(b.path)).map((d) => ({
      path: d.path,
      op: d.op,
      from: d.from || null,
      added: d.op === "put" && d.base.sha === null,
      conflict: this.conflicts.has(d.path),
      fromTerminal: !!d.fromTerminal,
      workflow: isWorkflowPath(d.path) || (d.from ? isWorkflowPath(d.from) : false),
      longLine: d.op !== "delete" && longestLine(d.text || "") > LONG_LINE,
    }));
  }

  /** The two texts a diff shows for a change. */
  async diff(path) {
    const d = this.drafts.get(path);
    if (!d) return null;
    let original = "";
    if (d.base.b64) {
      const info = inspect(fromB64(d.base.b64));
      original = info.readOnly && !info.fixable ? "" : info.text.replaceAll("\r\n", "\n");
    }
    const modified = d.op === "delete" ? "" : d.text;
    return { original, modified, longLine: longestLine(original) > LONG_LINE || longestLine(modified) > LONG_LINE };
  }

  // ---------------------------------------------------------------- search

  /** Server hits at the pinned commit, with drafted files searched here instead. */
  async search(text) {
    const res = this.head ? await this.api.post("/search", { branch: this.branch, sha: this.head, text }) : { hits: [], cut: false, timed_out: false };
    const needle = text.toLowerCase();
    const hits = res.hits.filter((h) => h.path !== null && !this.drafts.has(h.path));
    for (const d of this.drafts.values()) {
      if (d.op === "delete") continue;
      (d.text || "").split("\n").forEach((line, i) => {
        if (hits.length < 200 && line.toLowerCase().includes(needle)) hits.push({ path: d.path, line: i + 1, text: line.slice(0, 300), draft: true });
      });
    }
    return { hits, cut: res.cut, timedOut: res.timed_out };
  }

  // ---------------------------------------------------------------- a branch that moved

  /**
   * Fetches the branch head. When it moved: untouched files take the new base, files only this
   * person touched keep the edit, identical results are clean, and a file both sides changed
   * (or a delete against an edit) becomes a conflict for the person to settle. Nothing merges
   * silently and no draft is lost.
   */
  async checkHead(quiet) {
    const boot = await this.api.json("/boot", { branch: this.branch });
    this.boot = boot;
    if (boot.head === this.head) return [];
    const oldHead = this.head;
    this.head = boot.head;
    this.files.clear();
    await this.loadTree();
    const found = [];
    for (const d of this.drafts.values()) {
      const theirsEntry = this.tree.get(d.op === "rename" ? d.from : d.path) || null;
      const theirsSha = theirsEntry ? theirsEntry.sha : null;
      if (theirsSha === d.base.sha) {
        d.base.head = this.head; // nobody else touched it
        continue;
      }
      const theirs = theirsEntry ? await this.base(d.op === "rename" ? d.from : d.path) : null;
      const mine = d.op === "delete" ? null : encode(d.text, d.info);
      if (theirs && mine && sameBytes(theirs.bytes, mine)) {
        await this.drop(d.path); // the same change landed
        continue;
      }
      this.conflicts.set(d.path, { theirs: theirs ? theirs.bytes : null, oldHead });
      found.push(d.path);
    }
    for (const d of this.drafts.values()) if (!this.conflicts.has(d.path)) await this.store.save(d);
    this.emit("head", { oldHead, head: this.head, conflicts: found, quiet: !!quiet });
    this.emit("change");
    return found;
  }

  /** Settles a conflict: keep mine, take theirs, or an edited result. */
  async resolve(path, choice, text) {
    const c = this.conflicts.get(path);
    const d = this.drafts.get(path);
    if (!c || !d) return;
    const theirsEntry = this.tree.get(d.op === "rename" ? d.from : d.path) || null;
    if (choice === "theirs") {
      await this.drop(path);
      return;
    }
    // Mine or edited: the draft now starts from their version.
    d.base = { head: this.head, sha: theirsEntry ? theirsEntry.sha : null, b64: c.theirs ? toB64(c.theirs) : null };
    if (choice === "edited") d.text = text;
    if (d.op === "rename" && !theirsEntry) { d.op = "put"; delete d.from; }
    this.conflicts.delete(path);
    await this.store.save(d);
    this.emit("change");
  }

  // ---------------------------------------------------------------- commit

  body(opts) {
    const changes = [];
    for (const d of [...this.drafts.values()].sort((a, b) => a.path.localeCompare(b.path))) {
      if (d.op === "delete") changes.push({ op: "delete", path: d.path });
      else if (d.op === "rename") changes.push({ op: "rename", from: d.from, path: d.path, content_b64: toB64(encode(d.text, d.info)) });
      else changes.push({ op: "put", path: d.path, content_b64: toB64(encode(d.text, d.info)) });
    }
    return {
      branch: this.branch,
      expected_head: opts.fromStart ? opts.fromStart : this.head,
      new_branch: opts.newBranch || null,
      message: opts.message,
      workflow_confirmed: !!opts.workflowConfirmed,
      changes,
    };
  }

  /**
   * Commits every draft as one commit. `fromStart` commits to a new branch from the head the
   * drafts started at, so a moved branch needs no settling. After a lost response the server
   * is asked what happened, so nothing is committed twice.
   */
  async commit(opts) {
    if (!this.canCommit) throw new ApiError(0, "read_only", "This tab cannot commit.");
    if (this.conflicts.size && !opts.fromStart) throw new ApiError(0, "conflict", "Settle the files that changed on both sides first.");
    if (this.committing) throw new ApiError(0, "busy", "A commit is already on its way.");
    this.committing = true;
    const body = this.body(opts);
    try {
      let res;
      try {
        res = await this.api.post("/commit", body);
      } catch (e) {
        if (e.code !== "network") throw e;
        res = await this.afterLostResponse(body);
        if (!res) throw e;
      }
      await this.committed(res, body);
      return res;
    } finally {
      this.committing = false;
    }
  }

  /** The commit request got no answer. Did it land? Only if the branch head is exactly it. */
  async afterLostResponse(body) {
    for (let i = 0; i < 3; i++) {
      try {
        const target = body.new_branch || body.branch;
        const boot = await this.api.json("/boot", { branch: target });
        if (!boot.head || boot.head === body.expected_head) return null;
        if (!boot.head_parents || boot.head_parents.length !== 1 || boot.head_parents[0] !== (body.expected_head || undefined)) return null;
        if (boot.head_author_email !== boot.me_email) return null;
        for (const c of body.changes) {
          const at = { branch: target, sha: boot.head, path_b64: toB64(new TextEncoder().encode(c.path)) };
          if (c.op === "delete") {
            const gone = await this.api.file(at).then(() => false, (e) => e.status === 404);
            if (!gone) return null;
          } else {
            const f = await this.api.file(at);
            if (toB64(f.bytes) !== c.content_b64) return null;
          }
        }
        return { sha: boot.head, branch: target, recovered: true };
      } catch (e) {
        if (e.code !== "network") return null;
        await new Promise((r) => setTimeout(r, 1500));
      }
    }
    return null;
  }

  async committed(res, body) {
    // What the push was, so the run panel can say at once whether any workflow listens to it.
    const pushed = { target: res.branch, isNew: !!body.new_branch, paths: touchedPaths(body.changes) };
    for (const d of [...this.drafts.values()]) if (d.id) await this.store.remove(d.id);
    this.drafts.clear();
    this.conflicts.clear();
    for (const t of this.timers.values()) clearTimeout(t);
    this.timers.clear();
    if (res.branch !== this.branch) {
      this.emit("committed", { ...res, pushed, prUrl: this.pullUrl(body.branch, res.branch) });
      await this.switchBranch(res.branch);
    } else {
      this.head = res.sha;
      this.files.clear();
      await this.loadTree();
      this.emit("committed", { ...res, pushed, prUrl: null });
      this.emit("change");
    }
  }

  /** Where to open a pull request from `head` into `base`, in this repository. */
  pullUrl(base, head) {
    const repoBase = this.meta.base.replace(/\/workspace$/, "");
    return `${repoBase}/pulls/new?base=${encodeURIComponent(base)}&head=${encodeURIComponent(head)}`;
  }

  async newBranch(name) {
    if (!this.head) throw new ApiError(0, "invalid", "Commit a first file before making a branch.");
    const res = await this.api.post("/branches", { branch: this.branch, sha: this.head, name });
    return res;
  }

  // ---------------------------------------------------------------- commit on save

  /** Why commit on save cannot run now, or "" when it can. */
  autoCommitBlock() {
    if (!this.boot || this.branch === this.boot.repo.default_branch) return "Commit on save is never used on the default branch.";
    if (!this.boot.direct_commit) return "This branch is protected.";
    if (!this.canCommit) return "This tab cannot commit.";
    if (this.conflicts.size) return "Settle the conflicts first.";
    for (const c of this.changes()) {
      if (c.workflow) return "A change touches a workflow. Commit it by hand.";
      if (c.fromTerminal) return "A change came from the terminal. Commit it by hand.";
    }
    return "";
  }

  setAutoCommit(on) {
    this.autoCommit = on && !this.autoCommitBlock();
    this.emit("change");
    return this.autoCommit;
  }

  /** Save pressed: keep the draft, and commit if commit on save is on and allowed. */
  async save(path) {
    await this.flush();
    if (!this.autoCommit || this.committing || !this.drafts.size) return null;
    const block = this.autoCommitBlock();
    if (block) {
      this.autoCommit = false;
      this.emit("autocommit-paused", block);
      return null;
    }
    if (Date.now() - this.lastAutoCommit < AUTO_COMMIT_GAP_MS) return null;
    // The head must not have moved under the drafts.
    const moved = await this.checkHead(true);
    if (moved.length || this.conflicts.size) {
      this.autoCommit = false;
      this.emit("autocommit-paused", "The branch moved. Commit by hand.");
      return null;
    }
    this.lastAutoCommit = Date.now();
    const name = path.split("/").pop();
    return this.commit({ message: `Update ${name}` });
  }

  // ---------------------------------------------------------------- lock

  /** Access ended or this tab is stale: wipe everything readable and stop. */
  async lock(reason) {
    if (this.locked) return;
    this.locked = true;
    this.api.lock(reason);
    for (const [p, t] of this.timers) {
      clearTimeout(t);
      const d = this.drafts.get(p);
      // The key still works for this last write; it is dropped right after.
      if (d && this.store) await this.store.save(d).catch(() => {});
    }
    this.timers.clear();
    if (this.store) this.store.lock();
    releaseEditLocks();
    this.files.clear();
    this.drafts.clear();
    this.conflicts.clear();
    this.tree = new Map();
    this.boot = null;
    this.emit("lock", reason);
  }
}
