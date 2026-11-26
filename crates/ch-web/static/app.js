// Codehaven front end. No framework, no inline script: the CSP is script-src 'self'.
// Everything here is progressive enhancement. Forms and links work without it.
(function () {
  'use strict';

  var doc = document;
  function qsa(sel, root) { return Array.prototype.slice.call((root || doc).querySelectorAll(sel)); }
  function csrf() {
    var b = doc.body.getAttribute('hx-headers');
    try { return JSON.parse(b)['X-CSRF-Token'] || ''; } catch (e) { return ''; }
  }
  function setCookie(name, value) {
    var secure = location.protocol === 'https:' ? '; Secure' : '';
    doc.cookie = name + '=' + value + '; Path=/; Max-Age=31536000; SameSite=Lax' + secure;
  }

  // ---- theme: system -> light -> dark, mirrored in a cookie so the sign-in page matches
  function currentTheme() { return doc.documentElement.getAttribute('data-theme') || 'system'; }
  function applyTheme(t) {
    doc.documentElement.setAttribute('data-theme', t);
    setCookie('ch_theme', t);
  }
  doc.addEventListener('click', function (e) {
    var t = e.target.closest('[data-theme-toggle]');
    if (t) {
      var order = ['system', 'light', 'dark'];
      applyTheme(order[(order.indexOf(currentTheme()) + 1) % 3]);
    }
  });

  // ---- copy buttons and permalinks
  function copyText(text, btn) {
    var done = function () {
      if (!btn) return;
      var old = btn.getAttribute('data-label') || btn.textContent;
      btn.setAttribute('data-label', old);
      btn.classList.add('copied');
      if (btn.textContent.trim()) btn.textContent = 'Copied';
      setTimeout(function () { btn.classList.remove('copied'); if (btn.textContent.trim()) btn.textContent = old; }, 1400);
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () { fallbackCopy(text); done(); });
    } else { fallbackCopy(text); done(); }
  }
  function fallbackCopy(text) {
    var ta = doc.createElement('textarea');
    ta.value = text; ta.setAttribute('readonly', ''); ta.style.position = 'fixed'; ta.style.opacity = '0';
    doc.body.appendChild(ta); ta.select();
    try { doc.execCommand('copy'); } catch (e) { /* nothing to do */ }
    doc.body.removeChild(ta);
  }
  doc.addEventListener('click', function (e) {
    var b = e.target.closest('[data-copy]');
    if (b) { e.preventDefault(); copyText(b.getAttribute('data-copy'), b); return; }
    var l = e.target.closest('[data-copy-link]');
    if (l) { e.preventDefault(); copyText(location.origin + l.getAttribute('data-copy-link'), l); }
  });

  // ---- confirm before destructive forms
  doc.addEventListener('submit', function (e) {
    var f = e.target.closest('form[data-confirm]');
    if (f && !window.confirm(f.getAttribute('data-confirm'))) e.preventDefault();
  });

  // ---- submit a form when a select or checkbox changes
  doc.addEventListener('change', function (e) {
    var el = e.target.closest('[data-autosubmit]');
    if (el && el.form) el.form.submit();
  });

  // ---- dropdown menus built on <details>: one open at a time, close on outside click and Escape
  doc.addEventListener('click', function (e) {
    qsa('details[data-menu][open]').forEach(function (d) { if (!d.contains(e.target)) d.removeAttribute('open'); });
  });
  doc.addEventListener('toggle', function (e) {
    var d = e.target;
    if (d.matches && d.matches('details[data-menu]') && d.open) {
      qsa('details[data-menu][open]').forEach(function (o) { if (o !== d) o.removeAttribute('open'); });
    }
    if (d.matches && d.matches('details.menu')) placeMenu(d);
  }, true);
  doc.addEventListener('keydown', function (e) {
    if (e.key === 'Escape') qsa('details[data-menu][open]').forEach(function (d) { d.removeAttribute('open'); });
  });

  // On a phone a menu spans the screen and is fixed, so a bar that scrolls sideways cannot clip
  // it. Fixed alone would pin it under the top bar, far from a button lower down the page, so
  // it is placed right under its button (or above, when there is no room below) and follows the
  // button as the page scrolls. Wider screens keep the plain CSS dropdown.
  var phone = window.matchMedia ? window.matchMedia('(max-width: 639px)') : null;
  function placeMenu(d) {
    var panel = d.querySelector(':scope > .menu-panel');
    var summary = d.querySelector(':scope > summary');
    if (!panel || !summary) return;
    if (!d.open || !phone || !phone.matches) { panel.style.top = ''; return; }
    var gap = 6;
    var bar = parseFloat(getComputedStyle(doc.documentElement).getPropertyValue('--topbar-h')) || 0;
    var r = summary.getBoundingClientRect();
    var h = panel.offsetHeight;
    var top = r.bottom + gap;
    if (top + h > window.innerHeight && r.top - gap - h >= bar) top = r.top - gap - h;
    panel.style.top = Math.round(top) + 'px';
  }
  function placeOpenMenus() { qsa('details.menu[open]').forEach(placeMenu); }
  window.addEventListener('scroll', placeOpenMenus, { passive: true });
  window.addEventListener('resize', placeOpenMenus);

  // ---- tabs: Write / Preview for markdown boxes
  function previewFor(tabs, button) {
    var pane = tabs.querySelector('[data-tab-pane="preview"]');
    var ta = tabs.querySelector('textarea');
    if (!pane || !ta) return;
    var body = new URLSearchParams();
    body.set('text', ta.value);
    body.set('owner', button.getAttribute('data-preview-owner') || '');
    body.set('repo', button.getAttribute('data-preview-repo') || '');
    body.set('_csrf', csrf());
    pane.innerHTML = '<p class="muted">Loading preview</p>';
    fetch(button.getAttribute('data-preview-url') || '/preview', {
      method: 'POST', credentials: 'same-origin',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded', 'X-CSRF-Token': csrf() },
      body: body.toString()
    }).then(function (r) { return r.text(); }).then(function (html) { pane.innerHTML = html; })
      .catch(function () { pane.innerHTML = '<p class="muted">The preview could not load.</p>'; });
  }
  doc.addEventListener('click', function (e) {
    var b = e.target.closest('[data-tab]');
    if (!b) return;
    var tabs = b.closest('[data-tabs]');
    if (!tabs) return;
    var name = b.getAttribute('data-tab');
    qsa('[data-tab]', tabs).forEach(function (x) { x.classList.toggle('current', x === b); x.setAttribute('aria-selected', x === b ? 'true' : 'false'); });
    qsa('[data-tab-pane]', tabs).forEach(function (p) { p.hidden = p.getAttribute('data-tab-pane') !== name; });
    if (name === 'preview') previewFor(tabs, b);
  });

  // ---- tab and list helpers in code and markdown text boxes
  doc.addEventListener('keydown', function (e) {
    var ta = e.target;
    if (!ta || ta.tagName !== 'TEXTAREA') return;
    if (e.key === 'Tab' && ta.classList.contains('code-input') && !e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault();
      var s = ta.selectionStart, en = ta.selectionEnd;
      ta.value = ta.value.slice(0, s) + '    ' + ta.value.slice(en);
      ta.selectionStart = ta.selectionEnd = s + 4;
    }
    if ((e.metaKey || e.ctrlKey) && e.key === 'Enter' && ta.form) { ta.form.requestSubmit ? ta.form.requestSubmit() : ta.form.submit(); }
  });

  // ---- line selection and permalinks in code views: click a number to select, shift for a range
  var lastLine = null;
  doc.addEventListener('click', function (e) {
    var a = e.target.closest('table.code td.ln a[data-line]');
    if (!a) return;
    var n = parseInt(a.getAttribute('data-line'), 10);
    if (e.shiftKey && lastLine) {
      e.preventDefault();
      var lo = Math.min(lastLine, n), hi = Math.max(lastLine, n);
      history.replaceState(null, '', '#L' + lo + '-L' + hi);
      markRange(lo, hi);
    } else { lastLine = n; }
  });
  function markRange(lo, hi) {
    qsa('table.code tr.range').forEach(function (r) { r.classList.remove('range'); });
    for (var i = lo; i <= hi; i++) { var r = doc.getElementById('LC' + i); if (r) r.classList.add('range'); }
  }
  function applyHash() {
    var m = /^#L(\d+)(?:-L(\d+))?$/.exec(location.hash);
    if (!m) return;
    var lo = parseInt(m[1], 10), hi = m[2] ? parseInt(m[2], 10) : lo;
    markRange(lo, hi);
    var row = doc.getElementById('LC' + lo);
    if (row) row.scrollIntoView({ block: 'center' });
  }
  window.addEventListener('hashchange', applyHash);
  applyHash();

  // ---- inline review comments on diffs
  doc.addEventListener('click', function (e) {
    var btn = e.target.closest('.line-comment-btn');
    var wrap = e.target.closest('.diffs[data-inline-action]');
    if (btn && wrap) {
      var tpl = doc.getElementById('inline-comment-template');
      if (!tpl) return;
      var row = btn.closest('tr');
      var next = row.nextElementSibling;
      if (next && next.classList.contains('inline-form-row')) { next.querySelector('textarea').focus(); return; }
      var clone = tpl.content.firstElementChild.cloneNode(true);
      var form = clone.querySelector('form');
      form.setAttribute('action', wrap.getAttribute('data-inline-action'));
      form.querySelector('[name=path]').value = btn.getAttribute('data-comment-path');
      form.querySelector('[name=side]').value = btn.getAttribute('data-comment-side');
      form.querySelector('[name=line]').value = btn.getAttribute('data-comment-line');
      var tok = doc.createElement('input');
      tok.type = 'hidden'; tok.name = '_csrf'; tok.value = wrap.getAttribute('data-csrf') || csrf();
      form.appendChild(tok);
      var cols = row.children.length;
      clone.firstElementChild.setAttribute('colspan', String(cols));
      row.parentNode.insertBefore(clone, row.nextSibling);
      form.querySelector('textarea').focus();
    }
    var cancel = e.target.closest('[data-cancel-inline]');
    if (cancel) { var r = cancel.closest('tr'); if (r) r.remove(); }
  });

  // ---- live job logs over Server-Sent Events (see the Actions pages).
  // The server routes each chunk of new log to the step that owns it and numbers the lines;
  // this page only applies what arrives. `steps` events refresh the step panels, and the
  // elapsed time of every started step ticks once a second until the stream is done.
  qsa('[data-job-stream]').forEach(function (wrap) {
    var url = wrap.getAttribute('data-job-stream');
    var offset = wrap.getAttribute('data-offset') || '0';
    var line = wrap.getAttribute('data-line') || '1';
    var lastId = parseInt(offset, 10) || 0;
    if (!window.EventSource) return;
    var es = new EventSource(url + (url.indexOf('?') >= 0 ? '&' : '?') + 'offset=' + encodeURIComponent(offset) + '&line=' + encodeURIComponent(line));

    function fmtDur(ms) {
      var s = Math.max(0, Math.floor(ms / 1000));
      if (s < 60) return s + 's';
      if (s < 3600) return Math.floor(s / 60) + 'm ' + (s % 60) + 's';
      return Math.floor(s / 3600) + 'h ' + (Math.floor(s / 60) % 60) + 'm';
    }
    var timer = setInterval(function () {
      var now = Date.now();
      qsa('details.step[data-started]', wrap).forEach(function (d) {
        var dur = d.querySelector('.step-dur');
        var started = parseInt(d.getAttribute('data-started'), 10);
        if (dur && started) dur.textContent = fmtDur(now - started);
      });
      var head = doc.querySelector('[data-job-started]');
      var hs = head ? parseInt(head.getAttribute('data-job-started'), 10) : 0;
      if (head && hs) head.textContent = fmtDur(now - hs);
    }, 1000);

    function setStatus(note) {
      var s = doc.querySelector('[data-live-status]');
      if (s) s.textContent = note;
    }
    es.addEventListener('log', function (ev) {
      var id = parseInt(ev.lastEventId, 10) || 0;
      if (id && id <= lastId) return;
      if (id) lastId = id;
      var msg;
      try { msg = JSON.parse(ev.data); } catch (e) { return; }
      var box = wrap.querySelector('[data-step-log="' + Number(msg.step) + '"]');
      if (!box) return;
      var none = box.querySelector('.log-none');
      if (none) none.parentNode.removeChild(none);
      var stick = box.scrollTop + box.clientHeight >= box.scrollHeight - 24;
      box.insertAdjacentHTML('beforeend', msg.html);
      if (stick) box.scrollTop = box.scrollHeight;
    });
    // A toggle only counts as the user's once it stops being one we caused (see below).
    wrap.addEventListener('toggle', function (e) {
      var d = e.target;
      if (!d.matches || !d.matches('details.step')) return;
      if (d.__autoOpen) { d.__autoOpen = false; return; }
      d.setAttribute('data-user', '1');
    }, true);
    es.addEventListener('steps', function (ev) {
      var list;
      try { list = JSON.parse(ev.data); } catch (e) { return; }
      list.forEach(function (s) {
        var d = wrap.querySelector('details.step[data-step="' + Number(s.n) + '"]');
        if (!d) return;
        d.setAttribute('data-state', s.state);
        if (s.started_ms) d.setAttribute('data-started', String(s.started_ms));
        else d.removeAttribute('data-started');
        var icon = d.querySelector('.step-status');
        if (icon && s.class) {
          icon.innerHTML = '<span class="status status-' + s.class + '" title="' + s.label + '">' +
            '<svg class="icon" aria-hidden="true"><use href="#i-' + s.icon + '"/></svg>' +
            '<span class="sr-only">' + s.label + '</span></span>';
        }
        var dur = d.querySelector('.step-dur');
        if (dur) dur.textContent = s.duration || '';
        if (s.open && !d.hasAttribute('data-user') && !d.open) {
          d.__autoOpen = true;
          d.setAttribute('open', '');
        }
      });
    });
    es.addEventListener('done', function () {
      es.close();
      clearInterval(timer);
      setStatus('Finished. Updating the page.');
      setTimeout(function () { location.reload(); }, 700);
    });
    es.onerror = function () {
      es.close();
      clearInterval(timer);
      setStatus('The live view stopped. Reload to see the latest.');
    };
  });

  // ---- go to file: filter a long list as you type (letters in order, like most file finders)
  qsa('[data-filter-input]').forEach(function (input) {
    var list = doc.querySelector('[data-filter-list]');
    var empty = doc.querySelector('[data-filter-empty]');
    if (!list) return;
    var items = qsa('li', list);
    function match(path, q) {
      var p = path.toLowerCase(), i = 0;
      for (var k = 0; k < q.length && i >= 0; k++) { i = p.indexOf(q.charAt(k), i); if (i >= 0) i++; }
      return i >= 0;
    }
    function run() {
      var q = input.value.toLowerCase().replace(/\s+/g, '');
      var shown = 0;
      items.forEach(function (li) {
        var ok = !q || match(li.getAttribute('data-path') || '', q);
        li.hidden = !ok;
        if (ok) shown++;
      });
      if (empty) empty.hidden = shown !== 0;
    }
    input.addEventListener('input', run);
    input.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') {
        var first = items.filter(function (li) { return !li.hidden; })[0];
        if (first) { e.preventDefault(); location.href = first.querySelector('a').getAttribute('href'); }
      }
    });
    run();
  });

  // ---- keyboard shortcuts: / search, t file finder (goes to the file search), g i, g p, . workspace
  var gPending = false;
  doc.addEventListener('keydown', function (e) {
    var tag = (e.target.tagName || '').toLowerCase();
    if (tag === 'input' || tag === 'textarea' || tag === 'select' || e.target.isContentEditable) return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.key === '/') {
      var s = doc.getElementById('global-search');
      if (s) { e.preventDefault(); s.focus(); }
      return;
    }
    if (e.key === '.') {
      var ws = doc.querySelector('[data-workspace-link]');
      if (ws) { e.preventDefault(); location.href = ws.getAttribute('href'); }
      return;
    }
    if (gPending) {
      gPending = false;
      var side = doc.querySelector('.repo-side');
      var map = { i: 'issues', p: 'pulls', c: 'code', a: 'actions' };
      if (side && map[e.key]) {
        var links = qsa('.side-link', side);
        var want = { issues: '/issues', pulls: '/pulls', code: '', actions: '/actions' }[map[e.key]];
        links.forEach(function (l) { var h = l.getAttribute('href'); if ((want === '' && /^\/[^/]+\/[^/]+$/.test(h)) || (want && h.slice(-want.length) === want)) location.href = h; });
      }
      return;
    }
    if (e.key === 'g') { gPending = true; setTimeout(function () { gPending = false; }, 1200); return; }
    if (e.key === 'y') {
      var pl = doc.querySelector('[data-copy-link]');
      if (pl) { copyText(location.origin + pl.getAttribute('data-copy-link'), pl); }
    }
    if (e.key === 't') {
      var go = doc.querySelector('a[href*="/find/"]');
      var base = doc.querySelector('.repo-title .repo-name');
      if (go) location.href = go.getAttribute('href');
      else if (base) location.href = base.getAttribute('href') + '/find/' + encodeURIComponent((doc.querySelector('.picker-label') || {}).textContent || 'main');
    }
  });

  // ---- rows and panes that scroll sideways: say so at the edge that has more, and bring the
  // current tab into view. The fade is a mask, so it works over any background.
  var SCROLLERS = '.tabs, .repo-side, .admin-side, .settings-nav, .settings-tabs, .blob-code, .job-graph, .run-graph, table.diff, [data-hscroll]';
  function scrollCue(el) {
    var max = el.scrollWidth - el.clientWidth;
    el.classList.toggle('can-left', el.scrollLeft > 2);
    el.classList.toggle('can-right', max - el.scrollLeft > 2);
  }
  function bringCurrentIntoView(el) {
    var cur = el.querySelector('.current, [aria-current="page"], [aria-selected="true"]');
    if (!cur || el.scrollWidth <= el.clientWidth) return;
    var box = cur.getBoundingClientRect(), row = el.getBoundingClientRect();
    if (box.left < row.left || box.right > row.right) {
      el.scrollLeft += (box.left - row.left) - (row.width - box.width) / 2;
    }
  }
  function setUpScrollers() {
    qsa(SCROLLERS).forEach(function (el) {
      if (!el.__cue) {
        el.__cue = true;
        el.addEventListener('scroll', function () { scrollCue(el); }, { passive: true });
        bringCurrentIntoView(el);
      }
      scrollCue(el);
    });
  }
  var cueTimer;
  window.addEventListener('resize', function () { clearTimeout(cueTimer); cueTimer = setTimeout(setUpScrollers, 100); });
  window.addEventListener('load', setUpScrollers);
  doc.addEventListener('htmx:afterSwap', setUpScrollers);
  if (doc.readyState !== 'loading') setUpScrollers(); else doc.addEventListener('DOMContentLoaded', setUpScrollers);

  // ---- passkeys (WebAuthn): the ceremony runs here, the server checks it. A form with
  // data-passkey-register (settings) or data-passkey-signin (second step) asks the server for
  // options, has the browser's authenticator answer, puts the answer in the hidden `credential`
  // field and submits the form as an ordinary post, so the server answers with a page.
  function b64uToBuf(s) {
    s = String(s).replace(/-/g, '+').replace(/_/g, '/');
    while (s.length % 4) s += '=';
    var bin = atob(s), out = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out.buffer;
  }
  function bufToB64u(buf) {
    var a = new Uint8Array(buf), s = '';
    for (var i = 0; i < a.length; i++) s += String.fromCharCode(a[i]);
    return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  }
  function passkeyFail(form, text) {
    var p = form.querySelector('[data-passkey-error]');
    if (p) { p.textContent = text; p.hidden = false; }
    var b = form.querySelector('[data-passkey-go]');
    if (b) b.disabled = false;
  }
  function passkeyBegin(form) {
    return fetch(form.getAttribute('data-begin-url'), {
      method: 'POST', credentials: 'same-origin',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded', 'X-CSRF-Token': csrf() }, body: ''
    }).then(function (r) {
      return r.json().then(function (j) {
        if (!r.ok) throw new Error(j && j.error ? j.error : 'That did not work. Try again.');
        return j;
      });
    });
  }
  function passkeyRegister(form) {
    return passkeyBegin(form).then(function (j) {
      var o = j.publicKey;
      o.challenge = b64uToBuf(o.challenge);
      o.user.id = b64uToBuf(o.user.id);
      (o.excludeCredentials || []).forEach(function (c) { c.id = b64uToBuf(c.id); });
      return navigator.credentials.create({ publicKey: o });
    }).then(function (c) {
      if (!c) throw new Error('No passkey was made.');
      var r = c.response;
      return {
        id: c.id, rawId: bufToB64u(c.rawId), type: c.type,
        response: {
          attestationObject: bufToB64u(r.attestationObject), clientDataJSON: bufToB64u(r.clientDataJSON),
          transports: r.getTransports ? r.getTransports() : null
        },
        extensions: {}
      };
    });
  }
  function passkeyAssert(form) {
    return passkeyBegin(form).then(function (j) {
      var o = j.publicKey;
      o.challenge = b64uToBuf(o.challenge);
      (o.allowCredentials || []).forEach(function (c) { c.id = b64uToBuf(c.id); });
      return navigator.credentials.get({ publicKey: o });
    }).then(function (c) {
      if (!c) throw new Error('No passkey answered.');
      var r = c.response;
      return {
        id: c.id, rawId: bufToB64u(c.rawId), type: c.type,
        response: {
          authenticatorData: bufToB64u(r.authenticatorData), clientDataJSON: bufToB64u(r.clientDataJSON),
          signature: bufToB64u(r.signature), userHandle: r.userHandle ? bufToB64u(r.userHandle) : null
        },
        extensions: {}
      };
    });
  }
  function setUpPasskeys() {
    qsa('form[data-passkey-register], form[data-passkey-signin]').forEach(function (form) {
      if (form.__passkeys) return;
      form.__passkeys = true;
      var supported = !!(window.PublicKeyCredential && navigator.credentials && window.isSecureContext);
      var go = form.querySelector('[data-passkey-go]');
      if (!supported) {
        if (go) go.hidden = true;
        var u = form.querySelector('[data-passkey-unsupported]');
        if (u) u.hidden = false;
        return;
      }
      form.addEventListener('submit', function (e) {
        var field = form.querySelector('input[name="credential"]');
        if (field && field.value) return; // the answer is in: let the post go
        e.preventDefault();
        var err = form.querySelector('[data-passkey-error]');
        if (err) err.hidden = true;
        if (go) go.disabled = true;
        var run = form.hasAttribute('data-passkey-register') ? passkeyRegister : passkeyAssert;
        run(form).then(function (cred) {
          field.value = JSON.stringify(cred);
          form.submit();
        }).catch(function (x) {
          var text = x && x.name === 'NotAllowedError' ? 'The passkey step was cancelled or timed out. Try again.'
            : x && x.name === 'InvalidStateError' ? 'This device already has a passkey for you here.'
            : (x && x.message) || 'That did not work. Try again.';
          passkeyFail(form, text);
        });
      });
    });
  }
  if (doc.readyState !== 'loading') setUpPasskeys(); else doc.addEventListener('DOMContentLoaded', setUpPasskeys);

  // ---- a banner so people notice when htmx fails to load a partial
  doc.addEventListener('htmx:responseError', function (e) {
    var t = e.target; if (t && t.insertAdjacentHTML) t.insertAdjacentHTML('afterend', '<p class="form-error" role="alert">That did not work. Reload the page and try again.</p>');
  });
})();
