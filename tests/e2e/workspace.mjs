// The browser workspace in a real browser. Five languages must get syntax colours in the editor,
// on a phone the top bar must stay visible on every pane, and a phone must edit and commit a
// long file with every control in reach and honest words about what will run. The workspace
// answers with a Content-Security-Policy that allows only the origin the server believes it has, so the page is
// loaded AT the public origin while request routing answers every request from the real server.
// A request for any other origin is aborted and remembered: the workspace must never reach one.
//   node workspace.mjs <session.json> <base-url> <public-origin> <owner/repo> <branch>
import { chromium } from 'playwright-core';
import fs from 'node:fs';

const [sessionFile, base, publicOrigin, slug, branch] = process.argv.slice(2);
if (!sessionFile || !base || !publicOrigin || !slug || !branch) {
  console.error('usage: node workspace.mjs <session.json> <base-url> <public-origin> <owner/repo> <branch>');
  process.exit(1);
}
const sess = JSON.parse(fs.readFileSync(sessionFile, 'utf8'));
const cookieName = sess.cookie_name || '__Host-ch_session';
const token = sess.alice;
if (!token) { console.error(`workspace: FAIL: no alice session cookie in ${sessionFile}`); process.exit(1); }

const FILES = ['a.ts', 'b.js', 'c.css', 'd.py', 'e.rs'];
const TABS = ['Files', 'Edit', 'Changes', 'Runs'];
const problems = [];
const errors = [];   // console errors, page errors and http >= 400 answers, from every page
const outside = [];  // the origins of the requests that were not for the public origin

const browser = await chromium.launch({ executablePath: '/usr/bin/google-chrome', args: ['--no-sandbox', '--disable-gpu'] });

// The browser asks for the public origin; every one of those requests is answered from the real
// server, with the session cookie, the public host and one client address. Nothing else may load.
async function routedContext(opts) {
  const ctx = await browser.newContext(opts);
  const origin = new URL(publicOrigin).origin;
  await ctx.route('**/*', async (route) => {
    const u = new URL(route.request().url());
    if (u.origin !== origin) {
      if (!outside.includes(u.origin)) outside.push(u.origin);
      return route.abort();
    }
    const headers = { ...route.request().headers(), host: new URL(publicOrigin).host, 'x-forwarded-proto': 'https',
                      'x-forwarded-for': '10.9.2.9', cookie: `${cookieName}=${token}` };
    try {
      const resp = await route.fetch({ url: base + u.pathname + u.search, headers, maxRedirects: 0 });
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
  page.on('response', (r) => { if (r.status() >= 400) errors.push(`http ${r.status()} ${new URL(r.url()).pathname.slice(0, 200)}`); });
}

// 1. Desktop: each of the five languages must open with syntax colours. Fewer than three distinct
// mtk classes on the text spans means Monaco is showing plain text.
const desktop = await routedContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'light' });
let coloured = 0;
for (const file of FILES) {
  const page = await desktop.newPage();
  watch(page);
  let n = 0;
  try {
    await page.goto(`${publicOrigin}/${slug}/workspace?branch=${encodeURIComponent(branch)}&path=${file}`,
                    { waitUntil: 'domcontentloaded', timeout: 30000 });
    await page.waitForSelector('.monaco-editor .view-lines .view-line', { timeout: 20000 });
    await page.waitForTimeout(1500);  // the colour modules load after the editor itself
    n = await page.evaluate(() => {
      const names = new Set();
      for (const s of document.querySelectorAll('.monaco-editor .view-lines span span'))
        for (const c of s.classList) if (c.startsWith('mtk')) names.add(c);
      return names.size;
    });
    if (n >= 3) coloured++;
    else problems.push(`${file}: ${n} colour class(es), expected at least 3`);
  } catch (e) {
    problems.push(`${file}: ${String(e).split('\n')[0].slice(0, 200)}`);
  }
  console.log(`workspace: ${file} ${n} colour classes`);
  await page.close();
}
await desktop.close();

// 2. Phone: the top bar must be shown, and nothing may sit on top of it, on every pane.
const phone = await routedContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, colorScheme: 'light' });
let panes = 0;
try {
  const page = await phone.newPage();
  watch(page);
  await page.goto(`${publicOrigin}/${slug}/workspace?branch=${encodeURIComponent(branch)}`,
                  { waitUntil: 'domcontentloaded', timeout: 30000 });
  await page.waitForSelector('.ws-phone-tabs', { timeout: 20000 });
  for (const name of TABS) {
    await page.locator('.ws-phone-tab', { hasText: name }).click();
    await page.waitForTimeout(300);
    const r = await page.evaluate(() => {
      const bar = document.querySelector('.ws-top');
      if (!bar) return { shown: false, covered: [] };
      const b = bar.getBoundingClientRect();
      if (b.height === 0) return { shown: false, covered: [] };
      const covered = [];
      for (const f of [0.1, 0.5, 0.9]) {
        const el = document.elementFromPoint(b.left + b.width * f, b.top + b.height / 2);
        if (el && el.closest('.ws-top')) continue;
        covered.push({ tag: el ? el.tagName.toLowerCase() : 'nothing', cls: String(el ? el.className : '').split(' ')[0] });
      }
      return { shown: true, covered };
    });
    if (!r.shown) problems.push(`${name}: the top bar is not shown`);
    for (const c of r.covered) problems.push(`${name}: the top bar is covered by ${c.tag}.${c.cls}`);
    if (r.shown && r.covered.length === 0) panes++;
    console.log(`workspace: phone ${name} top bar ${r.shown && r.covered.length === 0 ? 'visible' : 'not visible'}`);
  }
  await page.close();
} catch (e) {
  problems.push(`phone: ${String(e).split('\n')[0].slice(0, 200)}`);
}
await phone.close();

// 3. Phone, the whole edit and commit path on a real file. Each check below failed once:
//  - the editor must end above the bottom tabs, or the last lines and the caret hide under them;
//  - every control in the Changes pane and the commit dialog needs a 44px tap area (an input
//    counts its label row);
//  - nothing may stick out past the right edge (the side bar's resize grip did);
//  - the top bar keeps the repository name whole and shortens the branch instead, also with a
//    long branch name;
//  - the demo workflow listens to pushes to main only, so for this branch the dialog must say
//    nothing will run, and the run panel must say so too instead of waiting;
//  - the run panel's note needs a contrast of at least 4.5:1.
const phone3 = await routedContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, colorScheme: 'light' });
let phoneChecks = 0;

// Problems with tap areas inside `root`: [{ what, w, h }] for every shown control under 44px.
const smallTargets = (root) => {
  const out = [];
  for (const el of document.querySelectorAll(root + ' button, ' + root + ' a[href], ' + root + ' input, ' + root + ' select, ' + root + ' textarea')) {
    if (el.closest('[hidden]')) continue;
    let r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) continue;
    if (el.tagName === 'INPUT' && (el.type === 'checkbox' || el.type === 'radio')) {
      const label = el.closest('label') || (el.id && document.querySelector(`label[for="${el.id}"]`));
      if (label) r = label.getBoundingClientRect();
    }
    if (r.height < 43.5 || r.width < 43.5) {
      out.push({ what: `${el.tagName.toLowerCase()}.${String(el.className).split(' ')[0]} "${(el.textContent || el.getAttribute('aria-label') || '').trim().slice(0, 30)}"`, w: Math.round(r.width), h: Math.round(r.height) });
    }
  }
  return out;
};
// Shown elements that end past the right edge of the screen, or a page that scrolls sideways.
const offScreen = () => {
  const out = [];
  const W = window.innerWidth;
  if (document.documentElement.scrollWidth > W + 1) out.push(`the page is ${document.documentElement.scrollWidth}px wide`);
  for (const el of document.querySelectorAll('body *')) {
    const r = el.getBoundingClientRect();
    if (r.width <= 1 || r.height <= 1) continue;
    const cs = getComputedStyle(el);
    if (cs.visibility === 'hidden' || cs.display === 'none') continue;
    if (r.right > W + 0.5 || r.left < -0.5) out.push(`${el.tagName.toLowerCase()}.${String(el.className).split(' ')[0]} spans ${Math.round(r.left)} to ${Math.round(r.right)}`);
  }
  return out;
};
// The repository name in the top bar is cut when its text is wider than its box.
const repoCut = () => {
  const el = document.querySelector('.ws-top .ws-repo');
  if (!el) return 'no repository name in the top bar';
  return el.scrollWidth > el.clientWidth + 1 ? `the repository name shows ${el.clientWidth}px of ${el.scrollWidth}px` : '';
};
// The contrast of an element's text on what is behind it, from computed colours.
const contrastOf = (sel) => {
  const el = [...document.querySelectorAll(sel)].find((e) => !e.closest('[hidden]') && e.getBoundingClientRect().height > 0);
  if (!el) return null;
  const parse = (c) => {
    let m = c.match(/^rgba?\(([\d.]+),\s*([\d.]+),\s*([\d.]+)(?:,\s*([\d.]+))?\)/);
    if (m) return [m[1] / 255, m[2] / 255, m[3] / 255, m[4] === undefined ? 1 : Number(m[4])];
    m = c.match(/^color\(srgb ([\d.e-]+) ([\d.e-]+) ([\d.e-]+)(?: \/ ([\d.]+))?\)/);
    if (m) return [Number(m[1]), Number(m[2]), Number(m[3]), m[4] === undefined ? 1 : Number(m[4])];
    return null;
  };
  const lum = ([r, g, b]) => {
    const f = (v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4);
    return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b);
  };
  const fg = parse(getComputedStyle(el).color);
  let bg = null;
  for (let e = el; e; e = e.parentElement) {
    const c = parse(getComputedStyle(e).backgroundColor);
    if (c && c[3] > 0.99) { bg = c; break; }
  }
  if (!fg || !bg) return { text: el.textContent.trim().slice(0, 60), ratio: 0, why: `colours ${getComputedStyle(el).color} on ${bg}` };
  const [a, b] = [lum(fg), lum(bg)];
  return { text: el.textContent.trim().slice(0, 60), ratio: (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05) };
};

// page.waitForFunction evaluates a string, which the workspace's CSP refuses, so poll with
// page.evaluate instead. Resolves with the first truthy answer, or null after `ms`.
async function until(page, fn, arg, ms) {
  const end = Date.now() + ms;
  for (;;) {
    const v = await page.evaluate(fn, arg).catch(() => null);
    if (v) return v;
    if (Date.now() > end) return null;
    await page.waitForTimeout(250);
  }
}

try {
  const page = await phone3.newPage();
  watch(page);
  const tab = async (name) => { await page.locator('.ws-phone-tab', { hasText: name }).click(); await page.waitForTimeout(300); };

  // The long branch first: the same commit under a long name, top bar only.
  await page.goto(`${publicOrigin}/${slug}/workspace?branch=${encodeURIComponent(branch + '-with-a-rather-long-name')}`,
                  { waitUntil: 'domcontentloaded', timeout: 30000 });
  await page.waitForSelector('.ws-phone-tabs', { timeout: 20000 });
  if (!await until(page, () => (document.querySelector('.ws-branch-name') || {}).textContent, null, 20000)) throw new Error('the long branch never showed');
  let cut = await page.evaluate(repoCut);
  if (cut) problems.push(`phone, long branch: ${cut}`);
  for (const o of await page.evaluate(offScreen)) problems.push(`phone, long branch: ${o}`);

  await page.goto(`${publicOrigin}/${slug}/workspace?branch=${encodeURIComponent(branch)}&path=long.txt`,
                  { waitUntil: 'domcontentloaded', timeout: 30000 });
  await page.waitForSelector('.ws-phone-tabs', { timeout: 20000 });
  cut = await page.evaluate(repoCut);
  if (cut) problems.push(`phone: ${cut}`);
  await tab('Edit');
  if (!await until(page, () => { const t = document.querySelector('.ws-textarea'); return t && t.value.includes('THE LAST LINE'); }, null, 20000)) throw new Error('long.txt never opened in the phone editor');
  // The caret at the very end, scrolled as far as it goes: the box must end above the tabs.
  const under = await page.evaluate(() => {
    const t = document.querySelector('.ws-textarea');
    t.focus();
    t.setSelectionRange(t.value.length, t.value.length);
    t.scrollTop = t.scrollHeight;
    const a = t.getBoundingClientRect();
    const b = document.querySelector('.ws-phone-tabs').getBoundingClientRect();
    return Math.round(a.bottom - b.top);
  });
  if (under > 0) problems.push(`phone: the editor runs ${under}px under the bottom tabs, so the last lines are hidden`);
  await page.keyboard.type('\nedited on a phone');
  if (!await until(page, () => /1 change/.test((document.querySelector('.ws-count') || {}).textContent || ''), null, 10000)) throw new Error('the edit was not counted as a change');

  await tab('Changes');
  await page.waitForSelector('.ws-changes-row', { timeout: 10000 });
  for (const t of await page.evaluate(smallTargets, '.ws-changes')) problems.push(`phone Changes: ${t.what} is ${t.w}x${t.h}, under 44px`);
  for (const o of await page.evaluate(offScreen)) problems.push(`phone Changes: ${o}`);

  await page.locator('.ws-top .ws-btn-primary').click();
  await page.waitForSelector('dialog.ws-dialog[open]', { timeout: 10000 });
  // The dialog asks which workflows listen, then says so in its runs box. (Its hidden workflow
  // warning also says "will run", so only the box counts.)
  await until(page, () => ((document.querySelector('dialog.ws-dialog[open] .ws-commit-runs') || {}).textContent || '').trim(), null, 10000);
  for (const t of await page.evaluate(smallTargets, 'dialog.ws-dialog[open]')) problems.push(`phone commit dialog: ${t.what} is ${t.w}x${t.h}, under 44px`);
  for (const o of await page.evaluate(offScreen)) problems.push(`phone commit dialog: ${o}`);
  const said = await page.evaluate(() => (document.querySelector('dialog.ws-dialog[open] .ws-commit-runs') || { textContent: '' }).textContent);
  if (/These workflows will run/.test(said)) problems.push(`phone commit dialog: it promises a run on ${branch}, but the workflow listens to main only`);
  if (!/Nothing will run/.test(said)) problems.push(`phone commit dialog: it does not say that nothing will run, it says "${said.slice(0, 120)}"`);
  await page.locator('dialog.ws-dialog[open] .ws-btn-primary').click();
  await page.waitForSelector('dialog.ws-dialog[open]', { state: 'detached', timeout: 20000 });

  await tab('Runs');
  // Not "Waiting for workflows": the panel knows at once that nothing listens.
  const verdict = await until(page, () => {
    const t = [...document.querySelectorAll('.ws-runs .ws-note')].filter((n) => !n.hidden).map((n) => n.textContent).join(' ');
    return /Nothing will run/.test(t) ? t : false;
  }, null, 8000);
  if (!verdict) {
    const t = await page.evaluate(() => [...document.querySelectorAll('.ws-runs .ws-note')].filter((n) => !n.hidden).map((n) => n.textContent).join(' | '));
    problems.push(`phone Runs: after the commit the panel says "${t.slice(0, 120)}", not that nothing will run`);
  }
  const c = await page.evaluate(contrastOf, '.ws-runs .ws-note');
  if (!c) problems.push('phone Runs: no note shown to measure');
  else if (c.ratio < 4.5) problems.push(`phone Runs: the note "${c.text}" has contrast ${c.ratio.toFixed(2)}:1, under 4.5:1${c.why ? ' (' + c.why + ')' : ''}`);
  else console.log(`workspace: phone Runs note contrast ${c.ratio.toFixed(2)}:1`);
  phoneChecks++;
  await page.close();
} catch (e) {
  problems.push(`phone edit and commit: ${String(e).split('\n')[0].slice(0, 200)}`);
}
await phone3.close();

await browser.close();
for (const e of errors) problems.push(e);
for (const o of outside) problems.push(`outside request: ${o}`);
if (problems.length) {
  // The same error repeats once per page, so each is printed once.
  for (const p of new Set(problems)) console.error(`workspace: FAIL: ${p}`);
  process.exit(1);
}
console.log(`workspace: ok, ${coloured} languages coloured, phone top bar visible on ${panes} panes, phone edit and commit checked ${phoneChecks}`);
process.exit(0);
