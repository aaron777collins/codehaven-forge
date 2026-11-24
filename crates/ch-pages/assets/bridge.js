/* Codehaven Pages storage bridge (docs/design/pages.md, section 4). First script of every page
   served as content on the pages host. c: {o: pages origin, p: "/owner/project/", t: token
   prefix or "", q: storage cap in characters, a: 1 when the page asks the viewer to sign in}. */
(function (c) {
  "use strict";
  var w = window, own = Object.prototype.hasOwnProperty, raw = "";
  try { raw = w.name; w.name = ""; } catch (e) {}
  var secret = null, data = Object.create(null);
  if (raw && raw.charAt(0) === "{") {
    try {
      var p = JSON.parse(raw);
      if (p && p.chp === 1 && typeof p.s === "string" && p.d && typeof p.d === "object") {
        secret = p.s;
        for (var k in p.d) if (own.call(p.d, k) && typeof p.d[k] === "string") data[k] = p.d[k];
      }
    } catch (e) {}
  }
  var up = w.parent !== w ? w.parent : null;
  function post(m) {
    if (!up) return;
    if (secret) m.s = secret;
    try { up.postMessage(m, c.o); } catch (e) {}
  }
  function used(d) { var n = 0; for (var k in d) n += k.length + d[k].length; return n; }
  function store(d, persist) {
    var proto = {
      getItem: function (k) { k = String(k); return k in d ? d[k] : null; },
      setItem: function (k, v) {
        k = String(k); v = String(v);
        var was = k in d ? k.length + d[k].length : 0;
        if (persist && used(d) - was + k.length + v.length > c.q) {
          throw new DOMException("This site's storage is full.", "QuotaExceededError");
        }
        d[k] = v;
        if (persist) post({ t: "set", k: k, v: v });
      },
      removeItem: function (k) {
        k = String(k);
        if (k in d) { delete d[k]; if (persist) post({ t: "del", k: k }); }
      },
      clear: function () { for (var k in d) delete d[k]; if (persist) post({ t: "clear" }); },
      key: function (i) { var ks = Object.keys(d); return i >= 0 && i < ks.length ? ks[i] : null; }
    };
    Object.defineProperty(proto, "length", { configurable: true, get: function () { return Object.keys(d).length; } });
    return new Proxy(Object.create(proto), {
      get: function (t, k) { if (typeof k === "symbol" || k in proto || !(k in d)) return t[k]; return d[k]; },
      set: function (t, k, v) { if (typeof k === "symbol" || k in proto) return false; proto.setItem(k, v); return true; },
      deleteProperty: function (t, k) { if (typeof k !== "symbol") proto.removeItem(k); return true; },
      has: function (t, k) { return k in proto || (typeof k === "string" && k in d); },
      ownKeys: function () { return Object.keys(d); },
      getOwnPropertyDescriptor: function (t, k) {
        if (typeof k === "string" && k in d) return { value: d[k], writable: true, enumerable: true, configurable: true };
        return undefined;
      }
    });
  }
  function define(o, name, value) {
    try { Object.defineProperty(o, name, { configurable: true, enumerable: true, get: function () { return value; } }); } catch (e) {}
  }
  define(w, "localStorage", store(data, !!secret));
  define(w, "sessionStorage", store(Object.create(null), false));
  var jar = Object.create(null);
  try {
    Object.defineProperty(document, "cookie", {
      configurable: true,
      get: function () { return Object.keys(jar).map(function (k) { return k + "=" + jar[k]; }).join("; "); },
      set: function (v) {
        v = String(v);
        var pair = v.split(";")[0], i = pair.indexOf("=");
        var name = (i < 0 ? "" : pair.slice(0, i)).trim();
        if (!name) return;
        var gone = /;\s*max-age\s*=\s*(0|-)/i.test(v);
        var ex = /;\s*expires\s*=\s*([^;]+)/i.exec(v);
        if (ex && Date.parse(ex[1]) < Date.now()) gone = true;
        if (gone) delete jar[name]; else jar[name] = pair.slice(i + 1).trim();
      }
    });
  } catch (e) {}
  if (!up) return;
  if (!secret) {
    // Arrived without the shell's secret, for example after a script set location: ask the
    // shell to load this page properly. It does so only inside its own project, once.
    post({ t: "hello", href: location.href });
    return;
  }
  function report() { post({ t: "loc", href: location.href, title: document.title || "" }); }
  ["pushState", "replaceState"].forEach(function (n) {
    var f = history[n];
    if (typeof f !== "function") return;
    history[n] = function () { var r = f.apply(this, arguments); report(); return r; };
  });
  w.addEventListener("hashchange", report);
  w.addEventListener("popstate", report);
  w.addEventListener("load", report);
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", report); else report();
  function plain(path) { return c.t && path.indexOf(c.t + "/") === 0 ? path.slice(c.t.length) : path; }
  // On window, so it runs after the page's own click handlers and leaves alone a click a
  // router already handled.
  w.addEventListener("click", function (e) {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    var a = e.target && e.target.closest ? e.target.closest("a[href]") : null;
    if (!a || a.hasAttribute("download")) return;
    var target = (a.getAttribute("target") || "").toLowerCase();
    if (target && target !== "_self") return;
    var u;
    try { u = new URL(a.getAttribute("href"), document.baseURI); } catch (x) { return; }
    if (u.protocol !== "http:" && u.protocol !== "https:") return;
    var inside = u.origin === location.origin && plain(u.pathname).indexOf(c.p) === 0;
    if (inside && u.pathname === location.pathname && u.search === location.search && u.hash) return;
    e.preventDefault();
    if (inside) post({ t: "nav", href: u.href });
    else { try { w.open(u.href, "_top"); } catch (x) {} }
  });
  if (c.a) post({ t: "auth" });
})
