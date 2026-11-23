// The terminal frame (spec sections 7 and 11). This document is a sandboxed frame with an opaque
// origin and `connect-src 'none'`. It runs only this script and xterm, both inlined and hashed
// into its policy. Repository code never runs here: the shell runs in a Worker made from bytes
// the workspace page checked and handed over. Nothing from the shell is ever turned into markup:
// it reaches xterm as plain text plus colour codes, and xterm draws it.
//
// The page talks to this frame over one MessagePort, offered once, from the parent window at the
// instance origin. Every message is checked for type, count and size; anything else stops the
// frame for good. The frame never receives a cookie, token, key, user name or URL.
(() => {
  "use strict";
  const ORIGIN = __CH_ORIGIN__;

  const MAX_RATE = 20; // messages a second from the page
  const MAX_FILES = 2000;
  const MAX_BYTES = 25 * 1024 * 1024;
  const MAX_WRITE_BYTES = 10 * 1024 * 1024;
  const MAX_RUNTIME = 4 * 1024 * 1024;
  const MAX_PATH = 4096;
  const MAX_OUT = 1024 * 1024; // the most one line may print
  const MAX_CHANGES = 500;
  const LINE_MS = 30000;
  const KILL_MS = 2000;
  const SLICE = 16 * 1024;
  const MAX_INPUT = 64 * 1024;
  const HISTORY = 500;

  // Taken before anything else could replace them.
  const MakeWorker = Worker;
  const MakeBlob = Blob;
  const blobUrl = URL.createObjectURL.bind(URL);
  const dropUrl = URL.revokeObjectURL.bind(URL);
  const parentWin = parent;

  // ---------------------------------------------------------------- paths

  // A path the frame accepts from the page or the shell: relative, plain segments, no controls.
  function validPath(p) {
    if (typeof p !== "string" || p.length === 0 || p.length > MAX_PATH) return false;
    if (/[\u0000-\u001f\u007f-\u009f\\]/.test(p)) return false;
    for (const seg of p.split("/")) {
      if (seg === "" || seg === "." || seg === "..") return false;
    }
    return true;
  }

  // ---------------------------------------------------------------- the screen

  const holder = document.getElementById("t");
  const term = new Terminal({
    fontFamily: 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
    fontSize: 13,
    lineHeight: 1.15,
    cursorBlink: true,
    scrollback: 5000,
    convertEol: true,
    allowProposedApi: false,
    linkHandler: { activate() {}, hover() {}, leave() {} },
    windowOptions: {},
  });
  term.open(holder);
  // Title, colour, hyperlink and clipboard sequences do nothing, whoever writes them.
  for (const n of [0, 1, 2, 4, 8, 10, 11, 12, 52]) term.parser.registerOscHandler(n, () => true);

  // The parent sizes the frame; the terminal fills it.
  const probe = document.createElement("span");
  probe.className = "probe";
  probe.textContent = "W".repeat(20);
  document.body.append(probe);
  function fit() {
    const r = probe.getBoundingClientRect();
    const cw = r.width / 20 || 8;
    // xterm sizes its screen to rows times the cell height; before it has drawn, guess.
    const scr = holder.querySelector(".xterm-screen");
    const drawn = scr ? scr.getBoundingClientRect().height / term.rows : 0;
    const ch = drawn > 4 ? drawn : Math.ceil(r.height * 1.15) || 17;
    const cols = Math.max(20, Math.floor((holder.clientWidth - 4) / cw));
    const rows = Math.max(4, Math.floor(holder.clientHeight / ch));
    if (cols !== term.cols || rows !== term.rows) term.resize(cols, rows);
  }
  new ResizeObserver(() => fit()).observe(holder);
  fit();
  // Once xterm has drawn, the real cell size is known: fit again.
  setTimeout(fit, 60);
  setTimeout(fit, 400);

  // Our own writes (prompt, line editor, notes) go straight to xterm. Shell output is cleaned
  // first and written in slices, so a flood never holds the frame's thread for long.
  function own(text) {
    term.write(text);
  }

  // Only printable text, tab, newline and SGR colour codes reach xterm from the shell, plus the
  // two codes `clear` prints. No title, clipboard, link or query sequence gets through, so no
  // reply can turn into typed input.
  const UNSAFE = /\x1b\[[0-9;]{0,32}m|\x1b\[[23]?J|\x1b\[H|[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g;
  function clean(s) {
    return s.replace(UNSAFE, (m) => (m.length > 1 ? m : ""));
  }

  const outQueue = [];
  let writing = false;
  let afterOutput = null;
  function guest(text) {
    const s = clean(text);
    for (let i = 0; i < s.length; i += SLICE) outQueue.push(s.slice(i, i + SLICE));
    if (!writing) pump();
  }
  function pump() {
    const next = outQueue.shift();
    if (next === undefined) {
      writing = false;
      const fn = afterOutput;
      afterOutput = null;
      if (fn) fn();
      return;
    }
    writing = true;
    term.write(next, pump);
  }
  function whenWritten(fn) {
    if (writing || outQueue.length) afterOutput = fn;
    else fn();
  }

  // ---------------------------------------------------------------- the port to the page

  let port = null;
  let dead = false;
  let windowStart = 0;
  let windowCount = 0;

  function tell(msg, transfer) {
    if (port && !dead) port.postMessage(msg, transfer || []);
  }

  // State changes go out at most ten times a second, the latest one winning, so a person holding
  // Enter never trips the page's message limit.
  let sentState = "";
  let wantState = "";
  let stateTimer = 0;
  function setState(state) {
    wantState = state;
    if (state === "failed") {
      clearTimeout(stateTimer);
      stateTimer = 0;
      sentState = state;
      tell({ type: "state", state });
      return;
    }
    if (stateTimer) return;
    stateTimer = setTimeout(() => {
      stateTimer = 0;
      if (wantState !== sentState) {
        sentState = wantState;
        tell({ type: "state", state: wantState });
      }
    }, 100);
  }

  /** The frame stops for good: no shell, no port. */
  function die(reason) {
    if (dead) return;
    setState("failed");
    dead = true;
    stopWorker();
    if (port) {
      port.onmessage = null;
      port.close();
    }
    port = null;
    mode = "stopped";
    own("\r\n\x1b[31mThe terminal stopped: " + reason + "\x1b[0m\r\n");
  }

  addEventListener("message", (e) => {
    if (e.source !== parentWin) return;
    if (port || dead) {
      // A second offer of a port is not something the workspace ever does.
      die("a second connection was offered");
      return;
    }
    if (e.origin !== ORIGIN || e.data !== "port" || !e.ports || e.ports.length !== 1) return;
    port = e.ports[0];
    port.onmessage = onPortMessage;
    port.onmessageerror = () => die("a message could not be read");
    tell({ type: "ack" });
  });

  function isObj(v) {
    return v !== null && typeof v === "object" && !Array.isArray(v);
  }
  function onlyKeys(v, keys) {
    return Object.keys(v).every((k) => keys.includes(k));
  }

  /** `[{path, data, exec}]` with every path valid, no repeats, and the byte cap held. */
  function fileList(list, maxCount, maxBytes) {
    if (!Array.isArray(list) || list.length > maxCount) return null;
    const out = new Map();
    let total = 0;
    for (const f of list) {
      if (!isObj(f) || !onlyKeys(f, ["path", "data", "exec"])) return null;
      if (!validPath(f.path) || out.has(f.path) || !(f.data instanceof ArrayBuffer) || typeof f.exec !== "boolean") return null;
      total += f.data.byteLength;
      if (total > maxBytes) return null;
      out.set(f.path, { data: new Uint8Array(f.data), exec: f.exec });
    }
    return out;
  }

  function onPortMessage(e) {
    if (dead) return;
    const now = Date.now();
    if (now - windowStart >= 1000) {
      windowStart = now;
      windowCount = 0;
    }
    if (++windowCount > MAX_RATE) return die("too many messages");
    const m = e.data;
    if (!isObj(m) || typeof m.type !== "string") return die("a message was not understood");
    switch (m.type) {
      case "init": {
        if (inited || !onlyKeys(m, ["type", "runtime", "files", "theme"])) return die("a message was not understood");
        if (!Array.isArray(m.runtime) || m.runtime.length !== 2 || !m.runtime.every((b) => b instanceof ArrayBuffer)) return die("the shell was not sent");
        if (m.runtime[0].byteLength + m.runtime[1].byteLength > MAX_RUNTIME) return die("the shell is too large");
        const files = fileList(m.files, MAX_FILES, MAX_BYTES);
        if (!files) return die("the files were not understood");
        applyTheme(m.theme);
        inited = true;
        runtime = m.runtime;
        for (const [p, f] of files) {
          baseline.set(p, f);
          mirror.set(p, f);
        }
        startWorker(true);
        return;
      }
      case "write": {
        if (!inited || !onlyKeys(m, ["type", "files"])) return die("a message was not understood");
        const files = fileList(m.files, 200, MAX_WRITE_BYTES);
        if (!files) return die("the files were not understood");
        for (const [p, f] of files) pending.push({ op: "put", path: p, data: f.data, exec: f.exec });
        drain();
        return;
      }
      case "delete": {
        if (!inited || !onlyKeys(m, ["type", "paths"]) || !Array.isArray(m.paths) || m.paths.length > 200) return die("a message was not understood");
        if (!m.paths.every(validPath)) return die("a path was not understood");
        for (const p of m.paths) pending.push({ op: "delete", path: p });
        drain();
        return;
      }
      case "scan":
        if (!inited || !onlyKeys(m, ["type"])) return die("a message was not understood");
        scan();
        return;
      case "stop":
        if (!onlyKeys(m, ["type"])) return die("a message was not understood");
        if (inited) kill("Stopped.");
        return;
      case "resize":
        if (!onlyKeys(m, ["type"])) return die("a message was not understood");
        fit();
        return;
      default:
        return die("a message was not understood");
    }
  }

  // The workspace's colours, as plain #rrggbb values. Anything else keeps xterm's own.
  function applyTheme(t) {
    if (!isObj(t)) return;
    const theme = {};
    for (const k of ["background", "foreground", "cursor", "selectionBackground", "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"]) {
      if (typeof t[k] === "string" && /^#[0-9a-fA-F]{6}$/.test(t[k])) theme[k] = t[k];
    }
    term.options.theme = theme;
    if (theme.background) document.body.style.background = theme.background;
  }

  // ---------------------------------------------------------------- files

  let inited = false;
  let runtime = null;
  // What the page last gave us, and the shell's files after the last completed line. Paths are
  // Map keys, never object keys.
  const baseline = new Map();
  const mirror = new Map();
  const pending = []; // writes from the page, applied when the shell is idle
  let cwd = "/work";
  let env = null;

  function same(a, b) {
    if (!a || !b) return a === b;
    // The executable bit is not compared: a commit from the workspace keeps a file's mode.
    if ((a.kind || "file") !== (b.kind || "file") || a.data.length !== b.data.length) return false;
    for (let i = 0; i < a.data.length; i++) if (a.data[i] !== b.data[i]) return false;
    return true;
  }

  /** Writes from the page: a file both sides changed is reported and left alone. */
  function drain() {
    if (mode !== "idle" || !pending.length) return;
    const conflicts = [];
    const puts = [];
    const dels = [];
    while (pending.length) {
      const w = pending.shift();
      const was = baseline.get(w.path);
      const now = mirror.get(w.path);
      const touchedHere = !same(was, now);
      if (w.op === "put") {
        const f = { data: w.data, exec: w.exec };
        if (touchedHere && !same(now, f)) {
          conflicts.push(w.path);
          baseline.set(w.path, f);
          continue;
        }
        baseline.set(w.path, f);
        mirror.set(w.path, f);
        puts.push([w.path, w.data, w.exec]);
      } else {
        if (touchedHere && now) {
          conflicts.push(w.path);
          baseline.delete(w.path);
          continue;
        }
        baseline.delete(w.path);
        mirror.delete(w.path);
        dels.push(w.path);
      }
    }
    if (worker && (puts.length || dels.length)) worker.postMessage({ t: "files", puts, dels });
    if (conflicts.length) {
      own("\r\n\x1b[33m" + conflicts.length + (conflicts.length === 1 ? " file was" : " files were") +
        " changed here and in the editor. The terminal kept its copy.\x1b[0m\r\n");
      tell({ type: "changes", reason: "conflict", files: [], conflicts: conflicts.slice(0, MAX_CHANGES) });
      // A fresh prompt, with whatever was being typed kept.
      const keep = buf;
      const at = cur;
      prompt();
      buf = keep;
      cur = at;
      redraw();
    }
  }

  /** What changed in the shell since the page's copy, for "Review terminal changes". */
  function scan() {
    const files = [];
    const transfer = [];
    let cut = false;
    const add = (f) => {
      if (files.length >= MAX_CHANGES) {
        cut = true;
        return;
      }
      files.push(f);
      if (f.data) transfer.push(f.data);
    };
    for (const [p, f] of mirror) {
      if (same(baseline.get(p), f)) continue;
      add({ path: p, kind: f.kind || "file", data: f.data.slice().buffer, exec: f.exec, added: !baseline.has(p) });
    }
    for (const p of baseline.keys()) if (!mirror.has(p)) add({ path: p, kind: "deleted", data: null, exec: false, added: false });
    tell({ type: "changes", reason: "scan", files, conflicts: [], cut }, transfer);
  }

  // ---------------------------------------------------------------- the shell Worker

  let worker = null;
  let workerUrl = "";
  let mode = "starting"; // starting, idle, running, stopped
  let lineTimer = 0;
  let killTimer = 0;
  let runId = 0;

  function stopWorker() {
    clearTimeout(lineTimer);
    clearTimeout(killTimer);
    if (worker) worker.terminate();
    worker = null;
    if (workerUrl) dropUrl(workerUrl);
    workerUrl = "";
  }

  function startWorker(first) {
    stopWorker();
    mode = "starting";
    setState("starting");
    const blob = new MakeBlob([runtime[0], "\n;\n", runtime[1]], { type: "text/javascript" });
    workerUrl = blobUrl(blob);
    try {
      worker = new MakeWorker(workerUrl, { name: "shell" });
    } catch (err) {
      die("this browser could not start the shell");
      return;
    }
    worker.onerror = (ev) => {
      ev.preventDefault();
      if (mode === "starting") die("the shell did not start");
      else kill("The shell failed.");
    };
    worker.onmessageerror = () => kill("The shell sent something that could not be read.");
    worker.onmessage = onWorker;
    const files = [];
    for (const [p, f] of mirror) if ((f.kind || "file") === "file") files.push([p, f.data, f.exec]);
    worker.postMessage({ t: "start", files, cwd, env, first: !!first });
  }

  /** Ends the shell now and starts again from the files after the last completed line. */
  function kill(reason) {
    if (dead || !inited) return;
    stopWorker();
    outQueue.length = 0;
    own("\r\n\x1b[33m" + reason + " The files are back as they were after the last finished command.\x1b[0m\r\n");
    startWorker(false);
  }

  function onWorker(e) {
    const m = e.data;
    if (!isObj(m) || typeof m.t !== "string") return kill("The shell sent something that could not be read.");
    if (m.t === "ready") {
      if (mode !== "starting") return;
      if (typeof m.banner === "string" && m.banner.length < 4096) own(m.banner.replaceAll("\n", "\r\n"));
      mode = "idle";
      setState("ready");
      prompt();
      drain();
      return;
    }
    if (m.t === "done") {
      if (mode !== "running" || m.id !== runId) return;
      clearTimeout(lineTimer);
      clearTimeout(killTimer);
      const out = typeof m.out === "string" ? m.out : "";
      if (typeof m.cwd === "string" && m.cwd.startsWith("/") && m.cwd.length <= MAX_PATH) cwd = m.cwd;
      if (isObj(m.env)) env = m.env;
      if (Array.isArray(m.changed)) checkpoint(m.changed);
      guest(out.length > MAX_OUT ? out.slice(0, MAX_OUT) + "\n[Output cut at 1 MB.]\n" : out);
      whenWritten(() => {
        if (mode !== "running") return;
        mode = "idle";
        setState("ready");
        prompt();
        drain();
      });
    }
  }

  /** The shell's files after a completed line: what a kill goes back to, and what a review sees. */
  function checkpoint(changed) {
    for (const c of changed) {
      if (!Array.isArray(c) || !validPath(c[0])) continue;
      const [p, data, exec, kind] = c;
      if (data === null) {
        mirror.delete(p);
        continue;
      }
      if (!(data instanceof Uint8Array)) continue;
      mirror.set(p, { data, exec: !!exec, kind: kind === "link" ? "link" : kind === "other" ? "other" : "file" });
    }
  }

  // ---------------------------------------------------------------- the line editor

  let buf = "";
  let cur = 0;
  let cursorRow = 0; // rows between the prompt's first row and the cursor
  const hist = [];
  let hpos = -1;
  let saved = "";

  function promptText() {
    const where = cwd === "/home/user" ? "~" : cwd.startsWith("/home/user/") ? "~" + cwd.slice(10) : cwd;
    return { text: "\x1b[32muser@workspace\x1b[0m:\x1b[34m" + clean(where) + "\x1b[0m$ ", width: 15 + [...where].length + 2 };
  }

  let shown = { text: "", width: 0 };
  function prompt() {
    if (mode !== "idle") return;
    shown = promptText();
    buf = "";
    cur = 0;
    cursorRow = 0;
    own("\r" + shown.text);
  }

  // Where the cursor is after `n` characters of the line, counting wraps and pasted newlines.
  function place(text, n) {
    const cols = term.cols;
    let row = 0;
    let col = shown.width;
    let i = 0;
    for (const ch of text) {
      if (i++ >= n) break;
      if (ch === "\n") {
        row++;
        col = 2;
        continue;
      }
      if (col >= cols) {
        row++;
        col = 0;
      }
      col++;
    }
    if (col >= cols) {
      row++;
      col = 0;
    }
    return { row, col };
  }

  function display(text) {
    return text.replaceAll("\t", " ").replaceAll("\n", "\r\n> ");
  }

  function redraw() {
    let s = cursorRow > 0 ? "\x1b[" + cursorRow + "A" : "";
    s += "\r\x1b[J" + shown.text + display(buf);
    const len = [...buf].length;
    const end = place(buf, len);
    const at = place(buf, cur);
    // A line that exactly fills the last column leaves xterm waiting to wrap: move down now.
    if (end.col === 0 && len > 0 && !buf.endsWith("\n")) s += "\r\n";
    const up = end.row - at.row;
    if (up > 0) s += "\x1b[" + up + "A";
    s += "\x1b[" + (at.col + 1) + "G";
    cursorRow = at.row;
    own(s);
  }

  function chars() {
    return [...buf];
  }

  function insert(text) {
    const cs = chars();
    const add = [...text];
    if (cs.length + add.length > MAX_INPUT) return;
    cs.splice(cur, 0, ...add);
    buf = cs.join("");
    cur += add.length;
    redraw();
  }

  function submit() {
    const line = buf;
    cur = [...buf].length;
    redraw();
    own("\r\n");
    cursorRow = 0;
    hpos = -1;
    if (!line.trim()) {
      prompt();
      return;
    }
    if (hist[hist.length - 1] !== line) hist.push(line);
    if (hist.length > HISTORY) hist.shift();
    mode = "running";
    setState("running");
    runId++;
    worker.postMessage({ t: "run", id: runId, line });
    lineTimer = setTimeout(() => kill("Stopped after 30 seconds."), LINE_MS);
  }

  function interrupt() {
    if (mode === "running") {
      own("^C");
      if (worker) worker.postMessage({ t: "abort" });
      clearTimeout(killTimer);
      killTimer = setTimeout(() => kill("Stopped."), KILL_MS);
      return;
    }
    if (mode === "idle") {
      cur = [...buf].length;
      redraw();
      own("^C\r\n");
      cursorRow = 0;
      prompt();
    }
  }

  function historyMove(d) {
    if (!hist.length) return;
    if (hpos === -1) {
      if (d > 0) return;
      saved = buf;
      hpos = hist.length - 1;
    } else {
      hpos += d;
      if (hpos >= hist.length) {
        hpos = -1;
        buf = saved;
        cur = [...buf].length;
        redraw();
        return;
      }
      if (hpos < 0) hpos = 0;
    }
    buf = hist[hpos];
    cur = [...buf].length;
    redraw();
  }

  const KEYS = new Map([
    ["\x1b[A", () => historyMove(-1)],
    ["\x1b[B", () => historyMove(1)],
    ["\x1b[C", () => { if (cur < chars().length) { cur++; redraw(); } }],
    ["\x1b[D", () => { if (cur > 0) { cur--; redraw(); } }],
    ["\x1b[H", () => { cur = 0; redraw(); }],
    ["\x1bOH", () => { cur = 0; redraw(); }],
    ["\x1b[1~", () => { cur = 0; redraw(); }],
    ["\x01", () => { cur = 0; redraw(); }],
    ["\x1b[F", () => { cur = chars().length; redraw(); }],
    ["\x1bOF", () => { cur = chars().length; redraw(); }],
    ["\x1b[4~", () => { cur = chars().length; redraw(); }],
    ["\x05", () => { cur = chars().length; redraw(); }],
    ["\x1b[3~", () => { const cs = chars(); if (cur < cs.length) { cs.splice(cur, 1); buf = cs.join(""); redraw(); } }],
    ["\x7f", () => { if (cur > 0) { const cs = chars(); cs.splice(cur - 1, 1); buf = cs.join(""); cur--; redraw(); } }],
    ["\b", () => { if (cur > 0) { const cs = chars(); cs.splice(cur - 1, 1); buf = cs.join(""); cur--; redraw(); } }],
    ["\x15", () => { buf = chars().slice(cur).join(""); cur = 0; redraw(); }],
    ["\x0b", () => { buf = chars().slice(0, cur).join(""); redraw(); }],
    ["\x17", () => {
      const cs = chars();
      let i = cur;
      while (i > 0 && cs[i - 1] === " ") i--;
      while (i > 0 && cs[i - 1] !== " ") i--;
      cs.splice(i, cur - i);
      buf = cs.join("");
      cur = i;
      redraw();
    }],
    ["\x0c", () => { term.clear(); redraw(); }],
  ]);

  term.onData((data) => {
    if (dead) return;
    if (data === "\x03") {
      interrupt();
      return;
    }
    // While a line runs, what is typed is dropped: the next line starts from a prompt.
    if (mode !== "idle") return;
    if (data === "\r") {
      submit();
      return;
    }
    const key = KEYS.get(data);
    if (key) {
      key();
      return;
    }
    if (data.startsWith("\x1b")) return;
    // Several characters at once is a paste (or an input method). It is inserted, and runs
    // only when Enter is pressed.
    const text = data.replace(/\r\n?/g, "\n").replace(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g, "");
    if (text) insert(text);
  });

  // Ctrl+backtick, F6 and Escape leave the terminal: the page moves focus to the panel's Close
  // button, never into the editor, where typed text would land in a file.
  term.attachCustomKeyEventHandler((ev) => {
    if (ev.type !== "keydown") return true;
    if ((ev.ctrlKey && (ev.key === "`" || ev.code === "Backquote")) || ev.key === "F6" || ev.key === "Escape") {
      ev.preventDefault();
      tell({ type: "blur" });
      return false;
    }
    return true;
  });

  // Focus on the frame goes to the terminal's own input.
  addEventListener("focus", () => term.focus());

  own("Starting the shell...\r\n");
  parentWin.postMessage({ type: "ready" }, ORIGIN);
})();
