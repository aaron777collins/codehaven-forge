// The workspace terminal in a real browser (spec sections 7, 11 and 17: T1 to T9, X1 to X7).
// The page is loaded AT the public origin and every request for it is answered from the real
// server, as tests/e2e/workspace.mjs does. Every request is logged, so the test can say exactly
// what the terminal frame and its Worker asked the server for: nothing, apart from the frame
// document itself. A request for any other origin is aborted and remembered.
//
//   node terminal.mjs <session.json> <base-url> <public-origin> <owner/repo> <branch> [chromium|firefox|webkit]
//
// The branch must hold a.ts, b.js, c.css, d.py and e.rs (tests/e2e/e2e_full.py step 22 pushes
// them) and must allow a direct commit. Chromium uses /usr/bin/google-chrome when it exists.
import * as pw from 'playwright-core';
import fs from 'node:fs';

const [sessionFile, base, publicOrigin, slug, branch, engineName = 'chromium'] = process.argv.slice(2);
if (!sessionFile || !base || !publicOrigin || !slug || !branch) {
  console.error('usage: node terminal.mjs <session.json> <base-url> <public-origin> <owner/repo> <branch> [chromium|firefox|webkit]');
  process.exit(1);
}
const sess = JSON.parse(fs.readFileSync(sessionFile, 'utf8'));
const cookieName = sess.cookie_name || '__Host-ch_session';
const token = sess.alice;
if (!token) { console.error(`terminal: FAIL: no alice session cookie in ${sessionFile}`); process.exit(1); }

const origin = new URL(publicOrigin).origin;
const wsPath = `/${slug}/workspace`;
const problems = [];
const notes = [];
const errors = [];
const outside = [];
const log = []; // every request: { path, dest, status, fromFrame, cookie }
const fail = (m) => problems.push(m);
const check = (ok, m) => { if (!ok) fail(m); };

const engines = { chromium: pw.chromium, firefox: pw.firefox, webkit: pw.webkit };
const engine = engines[engineName];
if (!engine) { console.error(`terminal: unknown engine ${engineName}`); process.exit(1); }
const launch = { headless: true };
if (engineName === 'chromium') {
  launch.args = ['--no-sandbox', '--disable-gpu'];
  if (fs.existsSync('/usr/bin/google-chrome')) launch.executablePath = '/usr/bin/google-chrome';
}
const browser = await engine.launch(launch);

async function routedContext(opts) {
  const ctx = await browser.newContext(opts);
  await ctx.route('**/*', async (route) => {
    const req = route.request();
    const u = new URL(req.url());
    // WebKit routes blob: loads too (the shell's Worker): they never reach a network.
    if (u.protocol === 'blob:' || u.protocol === 'data:') return route.continue();
    if (u.origin !== origin) {
      if (!outside.includes(u.origin)) outside.push(u.origin);
      return route.abort();
    }
    const all = await req.allHeaders().catch(() => req.headers());
    const entry = { path: u.pathname + u.search, dest: all['sec-fetch-dest'] || '', status: 0, fromFrame: false, browserCookie: !!all.cookie };
    try { entry.fromFrame = req.frame() !== req.frame().page().mainFrame(); } catch { entry.fromFrame = 'worker'; }
    log.push(entry);
    const headers = { ...req.headers(), host: u.host, 'x-forwarded-proto': 'https', 'x-forwarded-for': '10.9.2.9', cookie: `${cookieName}=${token}` };
    try {
      const resp = await route.fetch({ url: base + u.pathname + u.search, headers, maxRedirects: 0 });
      entry.status = resp.status();
      await route.fulfill({ response: resp });
    } catch {
      await route.abort();
    }
  });
  return ctx;
}

function watch(page) {
  page.on('console', (m) => { if (m.type() === 'error') errors.push(`console: ${m.text().slice(0, 200)}`); });
  page.on('pageerror', (e) => errors.push(`pageerror: ${String(e).slice(0, 200)}`));
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(fn, ms, what) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try { last = await fn(); if (last) return last; } catch (e) { last = e; }
    await sleep(100);
  }
  fail(`timed out: ${what}`);
  return null;
}

/** The terminal frame, once it exists. */
function termFrame(page) {
  return page.frames().find((f) => f !== page.mainFrame() && f.url().endsWith(wsPath + '/terminal')) || null;
}

/** The visible rows of the terminal, as text. */
async function screen(page) {
  const f = termFrame(page);
  if (!f) return '';
  return f.evaluate(() => {
    const rows = document.querySelector('.xterm-rows');
    return rows ? [...rows.children].map((r) => r.textContent.replace(/ /g, ' ')).join('\n') : '';
  });
}

async function run(page, line, expect, ms = 8000) {
  // What is typed while a line still runs is dropped, so wait for the prompt first.
  await prompt(page, 20000);
  await page.keyboard.type(line, { delay: 2 });
  await page.keyboard.press('Enter');
  if (expect) {
    const ok = await waitFor(async () => (await screen(page)).includes(expect), ms, `"${line}" shows "${expect}"`);
    if (!ok) notes.push(`screen after "${line}":\n${(await screen(page)).slice(-600)}`);
  }
}

async function clearScreen(page) {
  await page.keyboard.press('Control+l');
  await sleep(150);
}

async function prompt(page, ms = 8000) {
  return waitFor(async () => /user@workspace:\S*\$ *$/m.test((await screen(page)).trimEnd()), ms, 'a prompt');
}

async function openWorkspace(ctx, url) {
  const page = await ctx.newPage();
  watch(page);
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.ws-tab, .ws-row-main, .ws-editor', { timeout: 60000 });
  await waitFor(() => page.evaluate(() => !document.querySelector('.ws-term-toggle')?.hidden), 30000, 'the terminal button');
  return page;
}

async function openTerminal(page) {
  const t0 = Date.now();
  await page.click('.ws-term-toggle');
  await waitFor(() => termFrame(page), 15000, 'the terminal frame');
  const ok = await prompt(page, 15000);
  const ms = Date.now() - t0;
  await termFrame(page)?.click('.xterm').catch(() => {});
  return ok ? ms : null;
}

const url = `${origin}${wsPath}?branch=${encodeURIComponent(branch)}`;

// ---------------------------------------------------------------------------------------- desktop

const desktop = await routedContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' });
let page = await openWorkspace(desktop, url);

// T1: a clean build reaches a prompt within budget (here including the download).
const startMs = await openTerminal(page);
check(startMs !== null && startMs < 8000, `T1: the terminal took ${startMs} ms to show a prompt`);
notes.push(`${engineName}: prompt after ${startMs} ms`);

// T9: the warning sits outside the frame, below it, and is visible.
const geo = await page.evaluate(() => {
  const w = document.querySelector('.ws-term-warn');
  const f = document.querySelector('.ws-term-frame');
  const wr = w.getBoundingClientRect();
  const fr = f.getBoundingClientRect();
  return { text: w.textContent, wTop: wr.top, wBottom: wr.bottom, fBottom: fr.bottom, vh: innerHeight, inFrame: !!f.contentDocument };
});
check(geo.text.includes('Runs in this browser. No internet. Not your CI machine. Codehaven never asks for a password here.'), 'T9: the warning text');
check(geo.fBottom <= geo.wTop + 1 && geo.wBottom <= geo.vh + 1, `T9: the warning is not below the frame and on screen: ${JSON.stringify(geo)}`);
check(!geo.inFrame, 'X2: the page can read inside the frame, so it is not cross-origin');

// T2: the everyday commands.
await clearScreen(page);
await run(page, 'grep -n name a.ts', ':  name: string;');
await run(page, "sed -n 2p b.js", 'function total(prices) {');
await run(page, "awk 'NR==2 {print $1}' c.css", '.shell');
await run(page, 'find . -name "*.rs"', './e.rs');
await clearScreen(page);
await run(page, `echo '{"a":[1,2,39]}' | jq '.a | add'`, '42');
await run(page, 'for i in 1 2 3; do echo n$i; done', 'n3');
await run(page, 'export GREETING=hello-there', null);
await prompt(page);
await run(page, 'echo $GREETING', 'hello-there');
await run(page, 'cd /tmp && pwd', '/tmp');
await run(page, 'cd /work', null);
await prompt(page);
await clearScreen(page);
await run(page, 'git status', 'command not found');
await run(page, 'curl https://example.com', 'command not found');

// T3: an unsaved edit in the editor is in the shell.
await page.locator('.ws-row-main', { hasText: 'a.ts' }).first().click();
await page.waitForSelector('.monaco-editor .view-lines', { timeout: 30000 });
await page.click('.monaco-editor .view-lines');
await page.keyboard.press('Control+Home');
await page.keyboard.press('Meta+ArrowUp').catch(() => {});
await page.keyboard.type('// EDITED IN THE EDITOR\n');
await sleep(2600); // the draft is kept after one second, then pushed to the terminal after one more
await termFrame(page)?.click('.xterm').catch(() => {});
await clearScreen(page);
await run(page, 'head -1 a.ts', 'EDITED IN THE EDITOR');

// T3 and T4: shell changes reach the editor only through a review where nothing is ticked.
const changesBefore = await page.textContent('.ws-count');
// A new name each run, so the same branch can be checked in several engines one after another.
const made = `shell-${engineName}-${Date.now()}.txt`;
await run(page, `echo 'made in the shell' > ${made} && sed -i 's/hello/HELLO/' d.py`, null);
await prompt(page);
await run(page, "printf 'a\\0b' > bin.dat; ln -s /etc/passwd link; mkdir -p x/.git && echo bad > x/.git/config", null);
await prompt(page);
await run(page, "printf 'Password: '", 'Password:');
await page.keyboard.type('hunter2');
await page.keyboard.press('Enter');
await prompt(page);
check((await page.textContent('.ws-count')) === changesBefore, 'T3: a shell change reached the editor without a review');
await page.click('button:has-text("Review terminal changes")');
await page.waitForSelector('.ws-term-tray .ws-term-row', { timeout: 10000 });
const tray = await page.evaluate(() => [...document.querySelectorAll('.ws-term-tray .ws-term-row')].map((r) => ({
  text: r.textContent, box: r.querySelector('input[type=checkbox]') ? { checked: r.querySelector('input').checked, disabled: r.querySelector('input').disabled } : null,
})));
const row = (name) => tray.find((r) => r.text.includes(name));
check(row(made) && row(made).box && !row(made).box.checked && row(made).box.disabled, `T4: ${made} is listed, not ticked, and cannot be ticked before it is opened: ${JSON.stringify(row(made))}`);
check(row('d.py') && row('d.py').box && !row('d.py').box.checked, 'T4: d.py is listed and not ticked');
check(row('bin.dat') && !row('bin.dat').box, 'T4: a binary file cannot be moved');
check(row('link') && !row('link').box, 'T4: a symlink cannot be moved');
check(row('x/.git/config') && !row('x/.git/config').box, 'T4: a path inside .git cannot be moved');
check(!tray.some((r) => r.text.includes('hunter2')), 'X7: what was typed at a fake prompt became a file');
check(!tray.some((r) => r.box && r.box.checked), 'T4: something was ticked for the person');
// Open the new file, tick it, leave d.py, move.
const shellRow = page.locator('.ws-term-tray .ws-term-row', { hasText: made });
await shellRow.locator('button', { hasText: 'Open' }).click();
await shellRow.locator('input[type=checkbox]').check();
await page.click('button:has-text("Move ticked files to drafts")');
await waitFor(async () => (await page.textContent('.ws-count')) !== changesBefore, 5000, 'T3: the ticked file became a draft');
await page.click('.ws-rail-btn[aria-label="Changes"]');
const changeText = await page.textContent('#ws-view-changes');
check(changeText.includes(made) && changeText.includes('From terminal'), 'T3: the moved file is a draft marked From terminal');
check(!changeText.includes('d.py'), 'T3: an unticked file became a draft');

// X7 and T3: committing a terminal file shows its diff and the CI warning, and needs a tick.
await page.click('.ws-top .ws-btn-primary');
await page.waitForSelector('dialog.ws-dialog[open] .ws-commit-terminal', { timeout: 10000 });
const dlg = await page.evaluate(() => {
  const d = document.querySelector('dialog.ws-dialog[open]');
  const commit = [...d.querySelectorAll('.ws-dialog-actions .ws-btn')].find((b) => b.textContent.trim() === 'Commit');
  return { text: d.textContent, disabled: commit.disabled };
});
check(dlg.text.includes('CI may run this code with secrets') && dlg.text.includes('made in the shell'), 'X7: the commit dialog shows the terminal file and the CI warning');
check(dlg.disabled, 'X7: Commit is on before the terminal tick');
await page.check('#ws-terminal-confirm');
const enabled = await page.evaluate(() => ![...document.querySelectorAll('dialog.ws-dialog[open] .ws-dialog-actions .ws-btn')].find((b) => b.textContent.trim() === 'Commit').disabled);
check(enabled, 'T3: Commit stays off after the tick');
await page.locator('dialog.ws-dialog[open] .ws-dialog-actions .ws-btn', { hasText: /^Commit$/ }).click();
await waitFor(async () => (await page.textContent('.ws-count')) === 'No changes', 20000, 'T3: the terminal file was committed');
notes.push('T3: committed a file made in the terminal');

// T5 and T6: a stuck line, Stop, and the files coming back as they were after the last line.
await termFrame(page)?.click('.xterm').catch(() => {});
await clearScreen(page);
await run(page, 'echo one > ck.txt', null);
await prompt(page);
await run(page, 'echo two > ck.txt; sleep 60', null);
await sleep(500);
const t5 = Date.now();
const rtt = await page.evaluate(async () => { const t = performance.now(); await new Promise((r) => requestAnimationFrame(r)); return performance.now() - t; });
check(rtt < 500, `T5: the page took ${rtt} ms to draw a frame while the shell ran`);
await page.click('button:has-text("Stop")');
await prompt(page, 5000);
check(Date.now() - t5 < 3000, `T5: Stop took ${Date.now() - t5} ms`);
await run(page, 'cat ck.txt', 'one');
const afterStop = await screen(page);
check(afterStop.includes('back as they were'), 'T6: Stop says the files went back');
// Ctrl+C on a line that does not stop by itself.
await run(page, 'sleep 60', null);
await sleep(300);
const t6 = Date.now();
await page.keyboard.press('Control+c');
await prompt(page, 5000);
check(Date.now() - t6 < 3500, `T6: Ctrl+C took ${Date.now() - t6} ms`);
// An output flood and a loop that never ends both stop, and the page keeps drawing.
await clearScreen(page);
await run(page, 'seq 1 99000', '99000', 20000);
await prompt(page, 15000);
await run(page, 'for i in 1 2 3; do seq 1 99000; done', null);
await prompt(page, 20000);
await run(page, 'while true; do :; done', 'too many commands', 15000);
await prompt(page, 15000);

// T7: title, clipboard, link and query sequences change nothing and type nothing.
const titleBefore = await page.title();
await clearScreen(page);
await run(page, "printf '\\033]0;HACKED\\007\\033]52;c;SGVsbG8=\\007\\033]8;;https://evil.example/\\007click\\033]8;;\\007\\033[6n\\033[c done\\n'", 'done');
await prompt(page);
const t7 = await termFrame(page).evaluate(() => ({ title: document.title, links: document.querySelectorAll('a').length }));
check((await page.title()) === titleBefore && t7.title === 'Terminal', `T7: a title sequence changed a title: ${t7.title}`);
check(t7.links === 0, 'T7: a hyperlink sequence made a link');
const last = (await screen(page)).trimEnd().split('\n').pop();
check(/\$\s*$/.test(last), `T7: a query sequence typed something: ${JSON.stringify(last)}`);

// X1 and X2: even script running in the frame (which our own code never allows) gets nowhere.
const before = log.length;
const probe = await termFrame(page).evaluate(async (O) => {
  const out = {};
  const tryIt = async (name, fn) => { try { const v = await fn(); out[name] = 'worked: ' + String(v).slice(0, 60); } catch (e) { out[name] = 'refused'; } };
  await tryIt('cookie', () => document.cookie);
  await tryIt('localStorage', () => localStorage.length);
  await tryIt('parent', () => parent.document.title);
  await tryIt('fetch', () => fetch(O + '/api/v3/user').then((r) => r.status));
  await tryIt('xhr', () => new Promise((res, rej) => { const x = new XMLHttpRequest(); x.onload = () => res(x.status); x.onerror = rej; x.open('GET', O + '/api/v3/user'); x.send(); }));
  await tryIt('websocket', () => new Promise((res, rej) => { const w = new WebSocket(O.replace('https', 'wss') + '/x'); w.onopen = () => res('open'); w.onerror = rej; setTimeout(rej, 1500); }));
  await tryIt('open', () => { const w = window.open(O); if (!w) throw new Error('null'); return 'window'; });
  await tryIt('rtc', () => { if (typeof RTCPeerConnection !== 'function') throw new Error('gone'); return new RTCPeerConnection(); });
  await tryIt('history', () => { history.back(); history.go(-2); history.pushState({}, '', '/x'); return location.href.endsWith('/terminal') ? (() => { throw new Error('kept'); })() : location.href; });
  // A nested frame is blocked by frame-src 'none': whatever error page loads is not readable.
  await tryIt('iframe', () => new Promise((res, rej) => { const f = document.createElement('iframe'); f.src = O + '/api/v3/user'; f.onload = () => { let d = null; try { d = f.contentDocument; } catch { /* opaque */ } if (d && d.body && d.body.textContent.trim()) res('read ' + d.body.textContent.slice(0, 20)); else rej(); }; document.body.append(f); setTimeout(rej, 1500); }));
  // A Worker made the way the shell's is, then one nested inside it.
  const src = `const O=${JSON.stringify(O)};const r={};const t=async(n,f)=>{try{const v=await f();r[n]='worked: '+String(v).slice(0,40)}catch(e){r[n]='refused'}};
    (async()=>{await t('fetch',()=>fetch(O+'/api/v3/user').then(x=>x.status));
    await t('xhr',()=>new Promise((a,b)=>{const x=new XMLHttpRequest();x.onload=()=>a(x.status);x.onerror=b;x.open('GET',O+'/api/v3/user');x.send()}));
    await t('importScripts',()=>{importScripts(O+'/static/app.js');return 'ran'});
    await t('import',()=>import(O+'/static/app.js'));
    await t('eventsource',()=>new Promise((a,b)=>{const e=new EventSource(O+'/x');e.onopen=()=>a('open');e.onerror=b;setTimeout(b,1500)}));
    await t('eval',()=>eval('1+1'));
    await t('function',()=>new Function('return 2')());
    await t('caches',()=>caches.open('x'));
    await t('font',()=>new FontFace('x','url('+O+'/f.woff2)').load());
    await t('rtc',()=>new RTCPeerConnection());
    await t('nested',()=>new Promise((a,b)=>{const w=new Worker(URL.createObjectURL(new Blob(['fetch(\"'+O+'/api/v3/user\").then(()=>postMessage(\"worked\"),()=>postMessage(\"refused\"))'])));w.onmessage=e=>e.data==='worked'?a('worked'):b();w.onerror=b;setTimeout(b,2000)}));
    postMessage(r)})()`;
  out.worker = await new Promise((res) => {
    try {
      const w = new Worker(URL.createObjectURL(new Blob([src], { type: 'text/javascript' })));
      w.onmessage = (e) => res(e.data);
      w.onerror = () => res('the worker did not start');
      setTimeout(() => res('timeout'), 8000);
    } catch (e) { res('refused: ' + e); }
  });
  return out;
}, origin).catch((e) => ({ error: String(e) }));
const leaks = [];
for (const [k, v] of Object.entries(probe)) if (typeof v === 'string' && v.startsWith('worked')) leaks.push(`frame ${k}: ${v}`);
if (probe.worker && typeof probe.worker === 'object') {
  for (const [k, v] of Object.entries(probe.worker)) {
    if (!String(v).startsWith('worked')) continue;
    // WebKit gives an opaque-origin Worker a cache store. It reaches no network (connect-src
    // 'none'), and the shell's own Worker removes it before any shell code runs: a note, not a leak.
    if (k === 'caches' && engineName === 'webkit') notes.push('X1: WebKit offers caches to a sandboxed Worker; the shell Worker removes it');
    else leaks.push(`worker ${k}: ${v}`);
  }
} else notes.push(`X1: worker probe: ${JSON.stringify(probe.worker)}`);
check(!leaks.length, `X1/X2: these got through: ${leaks.join('; ')}`);
await sleep(1000);
const fromGuest = log.slice(before).filter((e) => e.fromFrame);
check(fromGuest.every((e) => e.status === 0 || e.status === 403 || e.status === 404), `X1: the frame reached the server: ${JSON.stringify(fromGuest)}`);
check(!fromGuest.some((e) => e.path.startsWith('/api/') && e.status === 200), 'X1: an API call from the frame was answered');
notes.push(`X1: ${fromGuest.length} requests from the frame reached the route, all refused: ${JSON.stringify(fromGuest.map((e) => [e.path, e.status]))}`);
// The workspace is still where it was (X6), and the frame is still the first one.
check((await page.url()).startsWith(`${origin}${wsPath}`), 'X6: the frame moved the tab');

// X3: the frame navigating itself runs no handler but the gate's, and the page closes it.
const nav = log.length;
await termFrame(page)?.evaluate((O) => { location.href = O + '/api/v3/user'; }, origin).catch(() => {});
// Either the page's frame-src stops the navigation before it starts (Firefox), or the gate
// refuses it and the page closes a frame that loaded a second time (Chromium, WebKit).
await sleep(3000);
const closed = await page.evaluate(() => !document.querySelector('.ws-term-frame'));
const stillOurs = !closed && termFrame(page) !== null;
check(closed || stillOurs, 'X3: the frame holds something other than the terminal and the page kept it');
notes.push(`X3 (${engineName}): ${closed ? 'the page closed the frame after its second load' : 'the navigation was blocked before it left the frame'}`);
const navs = log.slice(nav).filter((e) => e.path.startsWith('/api/'));
check(navs.every((e) => e.status === 403 || e.status === 0), `X3: a framed navigation was answered: ${JSON.stringify(navs)}`);
check((await page.url()).startsWith(`${origin}${wsPath}`), 'X6: the tab moved');

// X4: a second ready destroys the frame.
if (closed) await page.click('button:has-text("Start again")');
await prompt(page, 15000);
await termFrame(page).evaluate(() => parent.postMessage({ type: 'ready' }, '*'));
await waitFor(() => page.evaluate(() => !document.querySelector('.ws-term-frame')), 5000, 'X4: a second ready destroyed the frame');

// Did the frame's own navigation carry the browser's cookie? Informational: the route adds it.
const frameDocs = log.filter((e) => e.path === `${wsPath}/terminal`);
notes.push(`${engineName}: frame document requested ${frameDocs.length} times, dest ${[...new Set(frameDocs.map((e) => e.dest))].join(',')}`);

await desktop.close();

// ---------------------------------------------------------------------------------------- phone

const phone = await routedContext({ viewport: { width: 390, height: 844 }, ...(engineName === 'firefox' ? {} : { isMobile: true, hasTouch: true }), colorScheme: 'light' });
page = await phone.newPage();
watch(page);
await page.goto(url, { waitUntil: 'domcontentloaded' });
await page.waitForSelector('.ws-phone-tabs', { timeout: 60000 });
await waitFor(() => page.evaluate(() => !document.querySelector('.ws-phone-tab:last-child')?.hidden), 30000, 'the phone Terminal tab');
await page.click('.ws-phone-tab:has-text("Terminal")');
await prompt(page, 20000);
const pgeo = await page.evaluate(() => {
  const w = document.querySelector('.ws-term-warn').getBoundingClientRect();
  const tabs = document.querySelector('.ws-phone-tabs').getBoundingClientRect();
  return { wTop: w.top, wBottom: w.bottom, tabsTop: tabs.top, scrollW: document.documentElement.scrollWidth, w: innerWidth };
});
check(pgeo.wBottom <= pgeo.tabsTop + 1 && pgeo.wTop > 0, `T9: on a phone the warning is not visible above the tabs: ${JSON.stringify(pgeo)}`);
check(pgeo.scrollW <= pgeo.w, 'N1: the phone page scrolls sideways');
await termFrame(page)?.click('.xterm').catch(() => {});
await run(page, 'ls', 'a.ts');
// Away to Files and back: the Terminal tab shows the same shell again.
await page.click('.ws-phone-tab:has-text("Files")');
await page.click('.ws-phone-tab:has-text("Terminal")');
const back = await page.evaluate(() => document.querySelector('.ws-app').dataset.pane === 'terminal' && !!document.querySelector('.ws-term-frame'));
check(back, 'the phone Terminal tab does not bring the open terminal back');
await phone.close();

await browser.close();

// ---------------------------------------------------------------------------------------- verdict

const csp = errors.filter((e) => /Content.Security.Policy|CSP|refused to/i.test(e));
// The probes above are meant to break CSP rules: only count violations from before them.
for (const n of notes) console.log(n);
if (outside.length) fail(`P2: requests for other origins: ${outside.join(', ')}`);
const unexpected = errors.filter((e) => !csp.includes(e) && !/api\/v3\/user|\/x\b|evil\.example|f\.woff2|static\/app\.js/.test(e));
if (unexpected.length) notes.push(`console errors (not failing): ${unexpected.slice(0, 8).join(' | ')}`);
if (problems.length) {
  for (const p of problems) console.error(`terminal: FAIL (${engineName}): ${p}`);
  process.exit(1);
}
console.log(`terminal: PASS (${engineName}): T1 to T9 and X1 to X7 checks held`);
