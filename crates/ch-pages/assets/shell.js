/* Codehaven Pages shell (docs/design/pages.md, section 4). It owns the pages host's real
   storage, decides the project from its own address, and frames the project's sandboxed
   content. The content only ever talks to it by postMessage, and only the frame it made with
   the current secret is listened to. */
(function () {
  "use strict";
  var cfg = JSON.parse(document.getElementById("chp").textContent);
  var SANDBOX = cfg.sandbox;
  var site = cfg.prefix.slice(1, -1).toLowerCase();
  var NS = "chp:s:" + site, TK = "chp:t:" + site, AK = "chp:a:" + site;
  var own = Object.prototype.hasOwnProperty;

  function storage(kind) {
    try { var s = window[kind]; s.getItem("chp"); return s; } catch (e) { return null; }
  }
  var real = storage("localStorage"), sess = storage("sessionStorage");

  var data = Object.create(null);
  if (real) {
    try {
      var saved = JSON.parse(real.getItem(NS) || "{}");
      for (var k in saved) if (own.call(saved, k) && typeof saved[k] === "string") data[k] = saved[k];
    } catch (e) {}
  }
  function used() { var n = 0; for (var k in data) n += k.length + data[k].length; return n; }
  function save() {
    if (!real) return;
    try {
      if (Object.keys(data).length) real.setItem(NS, JSON.stringify(data)); else real.removeItem(NS);
    } catch (e) {}
  }

  function expired(t) {
    var exp = parseInt(String(t).split(".")[3], 10);
    return !(exp > Date.now() / 1000 + 30);
  }
  var token = cfg.token || null;
  if (sess) {
    try {
      if (token) sess.setItem(TK, token);
      else { var held = sess.getItem(TK); if (held && !expired(held)) token = held; }
    } catch (e) {}
  }
  // A copied address never carries the token.
  if (cfg.token) history.replaceState(null, "", location.pathname.replace(/^\/-\/t\/[A-Za-z0-9_.-]+/, "") + location.search + location.hash);

  function inside(href) {
    var u;
    try { u = new URL(href, location.href); } catch (e) { return null; }
    if (u.origin !== location.origin) return null;
    var p = u.pathname.replace(/^\/-\/t\/[A-Za-z0-9_.-]+(?=\/)/, "");
    if (p.indexOf(cfg.prefix) !== 0) return null;
    return { path: p, search: u.search, hash: u.hash };
  }
  function show(t, push) {
    var url = t.path + t.search + t.hash;
    try { if (push) history.pushState(null, "", url); else history.replaceState(null, "", url); } catch (e) {}
  }
  function random() {
    var b = new Uint8Array(16);
    crypto.getRandomValues(b);
    return Array.prototype.map.call(b, function (x) { return ("0" + x.toString(16)).slice(-2); }).join("");
  }
  function htmlish(path) {
    var last = path.slice(path.lastIndexOf("/") + 1);
    return last === "" || last.indexOf(".") < 0 || /\.html?$/i.test(last);
  }

  var frame = null, secret = "", again = Object.create(null);
  // A fresh frame for every load: the name, which carries this project's data and the secret,
  // is set before the frame exists, so the only document that sees it is the one the server
  // sends for this project's address.
  function load(t) {
    if (frame) frame.remove();
    secret = random();
    var f = document.createElement("iframe");
    f.setAttribute("sandbox", SANDBOX);
    f.setAttribute("title", cfg.owner + "/" + cfg.project);
    f.name = htmlish(t.path) ? JSON.stringify({ chp: 1, s: secret, d: data }) : "";
    f.src = (token ? "/-/t/" + token : "") + t.path + t.search + t.hash;
    document.body.appendChild(f);
    frame = f;
  }
  function notice() {
    if (frame) { frame.remove(); frame = null; }
    var box = document.createElement("main");
    box.className = "notice";
    var h = document.createElement("h1");
    h.textContent = "This site is not available";
    var p = document.createElement("p");
    p.textContent = "It may not be published, or your account may not have access to it.";
    var a = document.createElement("a");
    a.href = cfg.open + encodeURIComponent(location.pathname.slice(cfg.prefix.length - 1) + location.search);
    a.textContent = "Try signing in again";
    box.appendChild(h); box.appendChild(p); box.appendChild(a);
    document.body.appendChild(box);
  }
  function signIn() {
    // A second sign-in within a minute means the token did not help: stop instead of looping.
    var last = 0;
    if (!sess) { notice(); return; }
    try { last = parseInt(sess.getItem(AK) || "0", 10) || 0; } catch (e) {}
    if (Date.now() - last < 60000) { notice(); return; }
    try { sess.setItem(AK, String(Date.now())); sess.removeItem(TK); } catch (e) {}
    location.assign(cfg.open + encodeURIComponent(location.pathname.slice(cfg.prefix.length - 1) + location.search));
  }

  window.addEventListener("message", function (e) {
    if (!frame || e.source !== frame.contentWindow) return;
    var m = e.data;
    if (!m || typeof m !== "object" || typeof m.t !== "string") return;
    if (m.t === "hello") {
      var h = typeof m.href === "string" ? inside(m.href) : null;
      var key = h ? h.path + h.search : "";
      if (!h || (again[key] && Date.now() - again[key] < 3000)) return;
      again[key] = Date.now();
      show(h, false);
      load(h);
      return;
    }
    if (m.s !== secret) return;
    if (m.t === "set" && typeof m.k === "string" && typeof m.v === "string") {
      var was = m.k in data ? m.k.length + data[m.k].length : 0;
      if (used() - was + m.k.length + m.v.length > cfg.quota) return;
      data[m.k] = m.v;
      save();
    } else if (m.t === "del" && typeof m.k === "string") {
      delete data[m.k];
      save();
    } else if (m.t === "clear") {
      data = Object.create(null);
      save();
    } else if (m.t === "nav" && typeof m.href === "string") {
      var n = inside(m.href);
      if (n) { show(n, true); load(n); }
    } else if (m.t === "loc" && typeof m.href === "string") {
      var l = inside(m.href);
      if (l) show(l, false);
      if (typeof m.title === "string") document.title = m.title.slice(0, 200) || cfg.owner + "/" + cfg.project;
    } else if (m.t === "auth") {
      signIn();
    }
  });
  window.addEventListener("popstate", function () {
    load({ path: location.pathname, search: location.search, hash: location.hash });
  });
  load({ path: location.pathname, search: location.search, hash: location.hash });
})();
