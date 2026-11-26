// The editor's Worker in each browser engine (ch-1ke.8). Monaco runs its diff and tokenising in a
// Worker. The workspace page answers with a policy that allows only its own asset folder, so the
// Worker must start from that folder. When it does not, Monaco says "Could not create web
// worker(s)" and does the work on the page's main thread, which still works but freezes the
// editor on a big file. Safari did that. This opens the workspace in the engine asked for and
// fails if the editor Worker did not start from the asset folder, or if a blob: Worker was tried.
//   node monaco_worker.mjs <session.json> <base-url> <public-origin> <owner/repo> <branch> [chromium|firefox|webkit]
import * as pw from 'playwright-core';
import fs from 'node:fs';

const [sessionFile, base, publicOrigin, slug, branch, engineName = 'chromium'] = process.argv.slice(2);
if (!sessionFile || !base || !publicOrigin || !slug || !branch) {
  console.error('usage: node monaco_worker.mjs <session.json> <base-url> <public-origin> <owner/repo> <branch> [chromium|firefox|webkit]');
  process.exit(1);
}
const sess = JSON.parse(fs.readFileSync(sessionFile, 'utf8'));
const cookieName = sess.cookie_name || '__Host-ch_session';
const token = sess.alice;
if (!token) { console.error(`monaco_worker: FAIL: no alice session cookie in ${sessionFile}`); process.exit(1); }

const engine = { chromium: pw.chromium, firefox: pw.firefox, webkit: pw.webkit }[engineName];
if (!engine) { console.error(`monaco_worker: unknown engine ${engineName}`); process.exit(1); }
const launch = engineName === 'chromium'
  ? { executablePath: process.env.E2E_CHROME || '/usr/bin/google-chrome', args: ['--no-sandbox', '--disable-gpu'] }
  : {};
const browser = await engine.launch(launch);

const origin = new URL(publicOrigin).origin;
const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'light' });
await ctx.route('**/*', async (route) => {
  const u = new URL(route.request().url());
  if (u.origin !== origin) return route.abort();
  const headers = { ...route.request().headers(), host: new URL(publicOrigin).host, 'x-forwarded-proto': 'https',
                    'x-forwarded-for': '10.9.2.9', cookie: `${cookieName}=${token}` };
  try {
    const resp = await route.fetch({ url: base + u.pathname + u.search, headers, maxRedirects: 0 });
    await route.fulfill({ response: resp });
  } catch {
    await route.abort();
  }
});

const problems = [];
const notes = [];
const page = await ctx.newPage();
const workers = [];
const said = [];
page.on('worker', (w) => workers.push(w.url()));
page.on('console', (m) => said.push(`${m.type()}: ${m.text().slice(0, 240)}`));
page.on('pageerror', (e) => said.push(`pageerror: ${String(e).slice(0, 240)}`));

try {
  await page.goto(`${publicOrigin}/${slug}/workspace?branch=${encodeURIComponent(branch)}&path=a.ts`, { waitUntil: 'domcontentloaded', timeout: 30000 });
  await page.waitForSelector('.monaco-editor .view-lines .view-line', { timeout: 20000 });
  // The Worker starts when the first model is made; give the page time to ask for it.
  await page.waitForTimeout(3000);
} catch (e) {
  problems.push(`the editor did not open: ${String(e).split('\n')[0].slice(0, 200)}`);
}

const fell = said.filter((s) => /could not create web worker|falling back to loading web worker/i.test(s));
const refused = said.filter((s) => /script address is not allowed|trusted type|content security policy|refused to (create|load)/i.test(s));
const assets = `${origin}/${slug}/workspace/assets/`;
const good = workers.filter((u) => u.startsWith(assets) && /editor\.worker-[^/]*\.js$/.test(u));
const blobs = workers.filter((u) => u.startsWith('blob:'));
notes.push(`${engineName}: workers started: ${JSON.stringify(workers)}`);
if (!good.length) problems.push(`no Worker started from ${assets}; Monaco would run the editor service on the main thread`);
if (blobs.length) problems.push(`a blob: Worker was started: ${blobs.join(', ')}`);
if (fell.length) problems.push(`Monaco fell back to the main thread: ${fell[0]}`);
if (refused.length) problems.push(`the browser refused something: ${refused[0]}`);

await browser.close();
for (const n of notes) console.log(`monaco_worker: ${n}`);
if (problems.length) {
  for (const p of problems) console.error(`monaco_worker: FAIL (${engineName}): ${p}`);
  process.exit(1);
}
console.log(`monaco_worker: PASS (${engineName}): the editor Worker started from the asset folder`);
