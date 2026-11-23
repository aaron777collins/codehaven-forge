// Every request the workspace makes (spec section 6, "The page").
//
// - All fetches use `redirect: "manual"`, so a sign-in redirect is seen, never followed.
// - A redirect, a 401, a `stale` answer, or a read 404 or guard 403 that a failed `boot`
//   confirms, locks the page: the caller's `onLock` wipes what is on screen.
// - A network failure does not lock: it is reported so the page can offer Retry.
// - Nothing here ever builds HTML from a response.

export class ApiError extends Error {
  constructor(status, code, message, field) {
    super(message || code || "Request failed");
    this.status = status;
    this.code = code || "error";
    this.field = field || null;
  }
}

export class Api {
  constructor(meta, onLock) {
    this.base = meta.base;
    this.key = meta.key;
    this.csrf = meta.csrf;
    this.onLock = onLock;
    this.locked = false;
    this.ctrl = new AbortController();
  }

  /** Stops every request and stream at once, and refuses new ones. */
  lock(reason) {
    if (this.locked) return;
    this.locked = true;
    this.ctrl.abort();
    this.key = "";
    this.csrf = "";
    this.onLock(reason);
  }

  url(path, params) {
    const q = params ? "?" + new URLSearchParams(params).toString() : "";
    return this.base + path + q;
  }

  async raw(method, path, params, body, extra) {
    if (this.locked) throw new ApiError(0, "locked", "Sign in again.");
    const headers = { "X-Workspace-Key": this.key };
    if (method !== "GET") {
      headers["X-CSRF-Token"] = this.csrf;
      headers["Content-Type"] = "application/json";
    }
    let r;
    try {
      r = await fetch(this.url(path, params), {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        redirect: "manual",
        credentials: "same-origin",
        cache: "no-store",
        referrerPolicy: "same-origin",
        signal: (extra && extra.signal) || this.ctrl.signal,
      });
    } catch (e) {
      if (this.locked || (e && e.name === "AbortError")) throw new ApiError(0, "locked", "Stopped.");
      throw new ApiError(0, "network", "Codehaven could not be reached. Your drafts are kept.");
    }
    if (r.type === "opaqueredirect" || (r.status >= 300 && r.status < 400) || r.status === 401) {
      this.lock("signed_out");
      throw new ApiError(r.status, "locked", "Sign in again.");
    }
    if (r.ok) return r;
    let err = { code: "error", message: "" };
    const ct = r.headers.get("content-type") || "";
    if (ct.startsWith("application/json")) {
      try { err = await r.json(); } catch { /* keep the default */ }
    }
    if (err.code === "stale") {
      this.lock("stale");
      throw new ApiError(r.status, "locked", "This tab is out of date.");
    }
    // A plain 404 or a refusal from the request guard can mean access ended. Ask `boot`.
    if ((r.status === 404 || (r.status === 403 && !ct.startsWith("application/json"))) && path !== "/boot") {
      await this.confirmAccess();
    }
    throw new ApiError(r.status, err.code || (r.status === 404 ? "not_found" : "error"), err.message || r.statusText, err.field);
  }

  /** `boot` succeeds while access lasts. If it does not, lock. */
  async confirmAccess() {
    try {
      await this.raw("GET", "/boot", null);
    } catch (e) {
      if (e.code !== "network") this.lock("access");
    }
  }

  async json(path, params) {
    return (await this.raw("GET", path, params)).json();
  }

  async post(path, body) {
    return (await this.raw("POST", path, null, body)).json();
  }

  /** A file's raw bytes and what the server says about it. */
  async file(params) {
    const r = await this.raw("GET", "/file", params);
    const bytes = new Uint8Array(await r.arrayBuffer());
    return {
      bytes,
      mode: r.headers.get("x-file-mode") || "100644",
      size: Number(r.headers.get("x-file-size") || bytes.length),
      tooLarge: r.headers.get("x-too-large") === "1",
    };
  }

  /** Reads a server-sent event stream with fetch, so it carries the key header. */
  async stream(path, params, onEvent, signal) {
    const r = await this.raw("GET", path, params, undefined, { signal });
    const reader = r.body.getReader();
    const dec = new TextDecoder();
    let buf = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      buf += dec.decode(value, { stream: true });
      let i;
      while ((i = buf.indexOf("\n\n")) !== -1) {
        const block = buf.slice(0, i);
        buf = buf.slice(i + 2);
        const ev = { event: "message", data: "", id: null };
        for (const line of block.split("\n")) {
          if (line.startsWith(":")) continue;
          const c = line.indexOf(":");
          const k = c === -1 ? line : line.slice(0, c);
          const v = c === -1 ? "" : line.slice(c + 1).replace(/^ /, "");
          if (k === "event") ev.event = v;
          else if (k === "data") ev.data += (ev.data ? "\n" : "") + v;
          else if (k === "id") ev.id = v;
        }
        onEvent(ev);
      }
    }
  }
}
