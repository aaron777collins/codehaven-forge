// The shell Worker for the workspace terminal (spec section 11). The terminal frame builds this
// Worker from two pieces the workspace page checked against the build: the just-bash bundle,
// which defines `JustBash`, then this file. It is a classic script, never a module, and it runs
// with the frame's policy: no network, no imports, no eval.
//
// Messages in: start, run, abort, files. Messages out: ready, done. The frame keeps the files as
// they were after the last finished line, so a stuck or killed shell starts again from there.
"use strict";

(() => {
  // Defence in depth: nothing the shell runs needs these, so even a bug that let it run script
  // would find no network, storage, messaging or code loading here. The frame's policy already
  // blocks the network; WebKit still offers an opaque-origin Worker a cache store, for one.
  for (const k of ["fetch", "XMLHttpRequest", "WebSocket", "EventSource", "WebTransport", "importScripts", "caches", "indexedDB",
    "Worker", "SharedWorker", "BroadcastChannel", "RTCPeerConnection", "webkitRTCPeerConnection"]) {
    try {
      Object.defineProperty(self, k, { value: undefined, writable: false, configurable: false });
    } catch {
      // Not present in this engine.
    }
  }
  const { Bash, InMemoryFs } = JustBash;

  // Everything just-bash offers in a browser except what needs Node, wasm or the network:
  // no curl, Python, JavaScript, SQLite, gzip, tar, yq, xan or html-to-markdown.
  const COMMANDS = [
    "echo", "cat", "printf", "ls", "mkdir", "mktemp", "rmdir", "touch", "rm", "cp", "mv", "ln", "chmod", "pwd",
    "readlink", "head", "tail", "wc", "stat", "grep", "fgrep", "egrep", "rg", "sed", "awk", "sort", "uniq", "comm",
    "cut", "paste", "tr", "rev", "nl", "fold", "expand", "unexpand", "strings", "split", "column", "join", "tee",
    "find", "basename", "dirname", "tree", "du", "env", "printenv", "alias", "unalias", "history", "xargs", "true",
    "false", "yes", "clear", "bash", "sh", "jq", "base64", "diff", "date", "sleep", "timeout", "seq", "expr",
    "md5sum", "sha1sum", "sha256sum", "file", "help", "which", "tac", "hostname", "od", "time", "whoami",
  ];

  const MB = 1024 * 1024;
  const LIMITS = {
    maxCommandCount: 100000,
    maxLoopIterations: 100000,
    maxStringLength: 8 * MB,
    maxHeredocSize: 8 * MB,
    maxOutputSize: MB,
    maxExecutionTimeMs: 30000,
    maxFileSystemBytes: 64 * MB,
  };

  const HELP = [
    "This shell runs in your browser, on a copy of the files in /work.",
    "It runs bash syntax, pipes, loops and scripts, with grep, sed, awk, find, sort, diff, jq and more.",
    "It cannot run git, make, Python, Node or other programs, and it has no internet.",
    "Variables you export carry from one line to the next. Functions and aliases do not.",
    "Changes stay here until you choose Review terminal changes and tick each file you want as a draft.",
    "Use Commit and run for real builds and tests.",
    "",
  ].join("\n");

  const BANNER = "Shell in this browser, on a copy of your files in /work. Type help to see what it can run.\n";

  const WORK = "/work/";

  /** An in-memory filesystem that remembers which paths a line touched. */
  class TrackedFs extends InMemoryFs {
    constructor() {
      super(undefined, { maxTotalBytes: LIMITS.maxFileSystemBytes });
      this.touched = new Set();
    }
    touch(p) {
      if (typeof p === "string") this.touched.add(this.resolvePath("/", p));
    }
    // Writing over a file keeps its mode, as bash does; just-bash would reset it.
    async writeFile(p, ...a) {
      this.touch(p);
      const mode = await super.lstat(p).then((s) => (s.isFile ? s.mode : null), () => null);
      await super.writeFile(p, ...a);
      if (mode !== null) await super.chmod(p, mode).catch(() => {});
    }
    async appendFile(p, ...a) { this.touch(p); return super.appendFile(p, ...a); }
    writeFileSync(p, ...a) { this.touch(p); return super.writeFileSync(p, ...a); }
    writeFileLazy(p, ...a) { this.touch(p); return super.writeFileLazy(p, ...a); }
    async createExclusive(p, ...a) { this.touch(p); return super.createExclusive(p, ...a); }
    async rm(p, ...a) { this.touch(p); return super.rm(p, ...a); }
    async mkdir(p, ...a) { this.touch(p); return super.mkdir(p, ...a); }
    mkdirSync(p, ...a) { this.touch(p); return super.mkdirSync(p, ...a); }
    async cp(s, d, ...a) { this.touch(s); this.touch(d); return super.cp(s, d, ...a); }
    async mv(s, d, ...a) { this.touch(s); this.touch(d); return super.mv(s, d, ...a); }
    async chmod(p, ...a) { this.touch(p); return super.chmod(p, ...a); }
    async symlink(t, p, ...a) { this.touch(p); return super.symlink(t, p, ...a); }
    async link(s, d, ...a) { this.touch(d); return super.link(s, d, ...a); }
    async utimes(p, ...a) { this.touch(p); return super.utimes(p, ...a); }
  }

  let fs = null;
  let sh = null;
  let cwd = "/work";
  let env = null;
  let busy = null; // the AbortController of the running line
  // The files under /work as the frame last heard of them: path -> {data, exec, kind}.
  const known = new Map();

  function rel(abs) {
    return abs.startsWith(WORK) ? abs.slice(WORK.length) : null;
  }

  function sameBytes(a, b) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
    return true;
  }

  async function entry(abs) {
    let st;
    try {
      st = await fs.lstat(abs);
    } catch {
      return null;
    }
    if (st.isDirectory) return undefined;
    if (st.isSymbolicLink) {
      const target = await fs.readlink(abs).catch(() => "");
      return { data: new TextEncoder().encode(target), exec: false, kind: "link" };
    }
    if (!st.isFile) return { data: new Uint8Array(0), exec: false, kind: "other" };
    const data = await fs.readFileBuffer(abs);
    return { data, exec: (st.mode & 0o111) !== 0, kind: "file" };
  }

  /** What changed under /work in the paths this line touched: [path, bytes or null, exec, kind]. */
  async function changes() {
    const out = [];
    if (!fs.touched.size) return out;
    const roots = [...fs.touched];
    fs.touched.clear();
    const under = (p) => roots.some((r) => p === r || p.startsWith(r.endsWith("/") ? r : r + "/"));
    const look = new Set();
    for (const p of fs.getAllPaths()) if (rel(p) && under(p)) look.add(p);
    for (const r of known.keys()) if (under(WORK + r)) look.add(WORK + r);
    for (const abs of look) {
      const r = rel(abs);
      if (!r) continue;
      const now = await entry(abs);
      if (now === undefined) {
        // A folder: what was a file there is gone.
        if (known.has(r)) {
          known.delete(r);
          out.push([r, null, false, "file"]);
        }
        continue;
      }
      const was = known.get(r);
      if (!now) {
        if (was) {
          known.delete(r);
          out.push([r, null, false, "file"]);
        }
        continue;
      }
      if (was && was.exec === now.exec && was.kind === now.kind && sameBytes(was.data, now.data)) continue;
      known.set(r, now);
      out.push([r, now.data, now.exec, now.kind]);
    }
    return out;
  }

  function start(m) {
    fs = new TrackedFs();
    known.clear();
    for (const dir of ["/work", "/tmp", "/home/user"]) fs.mkdirSync(dir, { recursive: true });
    for (const [p, data, exec] of m.files) {
      const abs = WORK + p;
      fs.mkdirSync(abs.slice(0, abs.lastIndexOf("/")), { recursive: true });
      fs.writeFileSync(abs, data, undefined, { mode: exec ? 0o755 : 0o644 });
      known.set(p, { data, exec: !!exec, kind: "file" });
    }
    fs.touched.clear();
    cwd = typeof m.cwd === "string" ? m.cwd : "/work";
    sh = new Bash({
      fs,
      cwd,
      env: { HOME: "/home/user", USER: "user", LOGNAME: "user", TERM: "xterm-256color", LANG: "C.UTF-8" },
      commands: COMMANDS,
      executionLimits: LIMITS,
    });
    env = m.env && typeof m.env === "object" ? m.env : sh.getEnv();
    postMessage({ t: "ready", banner: m.first ? BANNER : "" });
  }

  async function run(m) {
    const line = String(m.line);
    let out = "";
    let code = 0;
    if (line.trim() === "help") out = HELP;
    busy = new AbortController();
    try {
      const r = await sh.exec(line, { env, cwd, replaceEnv: true, signal: busy.signal });
      out += (r.stdout || "") + (r.stderr || "");
      code = r.exitCode;
      if (r.env) {
        env = r.env;
        if (typeof r.env.PWD === "string" && r.env.PWD.startsWith("/")) cwd = r.env.PWD;
      }
    } catch (e) {
      out += "bash: " + String((e && e.message) || e) + "\n";
      code = 1;
    }
    busy = null;
    const changed = await changes().catch(() => []);
    // Send copies, so `known` keeps its own bytes.
    const sent = [];
    const transfer = [];
    for (const [p, data, exec, kind] of changed) {
      const copy = data ? data.slice() : null;
      if (copy) transfer.push(copy.buffer);
      sent.push([p, copy, exec, kind]);
    }
    postMessage({ t: "done", id: m.id, out, code, cwd, env, changed: sent }, transfer);
  }

  async function files(m) {
    for (const [p, data, exec] of m.puts) {
      const abs = WORK + p;
      await fs.mkdir(abs.slice(0, abs.lastIndexOf("/")), { recursive: true }).catch(() => {});
      await fs.writeFile(abs, data).catch(() => {});
      await fs.chmod(abs, exec ? 0o755 : 0o644).catch(() => {});
      known.set(p, { data, exec: !!exec, kind: "file" });
    }
    for (const p of m.dels) {
      await fs.rm(WORK + p, { force: true }).catch(() => {});
      known.delete(p);
    }
    // These came from the page, so they are not the shell's own changes.
    fs.touched.clear();
  }

  let queue = Promise.resolve();
  onmessage = (e) => {
    const m = e.data;
    if (!m || typeof m.t !== "string") return;
    if (m.t === "abort") {
      if (busy) busy.abort();
      return;
    }
    queue = queue.then(() => {
      if (m.t === "start") return start(m);
      if (m.t === "run") return run(m);
      if (m.t === "files") return files(m);
      return undefined;
    }).catch(() => {});
  };
})();
