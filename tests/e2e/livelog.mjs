// Regression check for the live job log view. While a job with three steps runs, the log
// of the step that is running must grow, its elapsed time must tick, and a step that has
// not started must stay empty. The lead starts the job before running this; the script
// only watches, it never starts anything.
//   node livelog.mjs <session.json> <base-url> <owner/repo> <run-id> <job-id>
//
// The workflow the job must come from (push to the repo, then start it by hand or by push):
//   name: livelog-check
//   on: [push, workflow_dispatch]
//   jobs:
//     three:
//       runs-on: codehaven
//       steps:
//         - run: for i in 1 2 3 4 5 6 7 8; do echo A$i; sleep 3; done
//         - run: for i in 1 2 3 4 5 6 7 8; do echo B$i; sleep 3; done
//         - run: echo C
import { chromium } from 'playwright-core';
import fs from 'node:fs';

const [sessionFile, base, slug, runId, jobId] = process.argv.slice(2);
if (!sessionFile || !base || !slug || !runId || !jobId) {
  console.error('usage: node livelog.mjs <session.json> <base-url> <owner/repo> <run-id> <job-id>');
  process.exit(1);
}
const sess = JSON.parse(fs.readFileSync(sessionFile, 'utf8'));
const cookieName = sess.cookie_name || '__Host-ch_session';
const token = sess.alice ||
  Object.entries(sess).filter(([k]) => k !== 'cookie_name').map(([, v]) => v).find(v => typeof v === 'string');
if (!token) { console.error(`livelog: FAIL: no session cookie in ${sessionFile}`); process.exit(1); }

const browser = await chromium.launch({ executablePath: '/usr/bin/google-chrome', args: ['--no-sandbox', '--disable-gpu'] });
const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
// The server expects the `__Host-` cookie, which a browser will not send over plain http.
// The pages are watched over the internal network, so the cookie goes in as a header.
await ctx.setExtraHTTPHeaders({ Cookie: `${cookieName}=${token}` });
const page = await ctx.newPage();

async function fail(msg) {
  console.error(`livelog: FAIL: ${msg}`);
  await browser.close();
  process.exit(1);
}

const url = `${base}/${slug}/actions/runs/${runId}/jobs/${jobId}`;
let status = 0;
try {
  const r = await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 30000 });
  status = r ? r.status() : 0;
} catch (e) {
  await fail(`could not load ${url}: ${String(e).split('\n')[0]}`);
}
if (status >= 400) await fail(`the job page answered ${status}`);

const hasStream = await page.evaluate(() => !!document.querySelector('[data-job-stream]'));
if (!hasStream) await fail('the job page has no live stream; is the job still running?');

async function sample() {
  return page.evaluate(() => Array.from(document.querySelectorAll('details.step')).map(d => ({
    n: d.getAttribute('data-step'),
    state: d.getAttribute('data-state'),
    lines: d.querySelectorAll('.log-line').length,
    none: !!d.querySelector('.log-none'),
    dur: (d.querySelector('.step-dur') || {}).textContent || ''
  })));
}

// Every 2 s for 25 s: t = 0, 2, ..., 24.
const samples = [];
for (let i = 0; i < 13; i++) {
  samples.push(await sample());
  if (i < 12) await page.waitForTimeout(2000);
}
await browser.close();

const first = samples[0];
if (first.length === 0) { console.error('livelog: FAIL: the page shows no steps at all'); process.exit(1); }

// 1. A queued step never has any log lines.
for (const [i, s] of samples.entries()) {
  for (const step of s) {
    if (step.state === 'queued' && step.lines > 0) {
      console.error(`livelog: FAIL: queued step ${step.n} had ${step.lines} log lines at sample ${i}`);
      process.exit(1);
    }
  }
}

// 2. The running step's log grows between samples at least once.
const grew = samples.some((s, i) => i > 0 && s.some((step, j) =>
  step.state === 'running' && step.lines > samples[i - 1][j].lines));
if (!grew) { console.error('livelog: FAIL: no running step gained log lines in 25 s'); process.exit(1); }

// 3. The running step's elapsed time changes between consecutive 2 s samples.
const ticked = samples.some((s, i) => i > 0 && s.some((step, j) =>
  step.state === 'running' && step.dur && samples[i - 1][j].dur && step.dur !== samples[i - 1][j].dur));
if (!ticked) { console.error('livelog: FAIL: no running step duration changed between samples'); process.exit(1); }

const running = samples[12].filter(s => s.state === 'running').length;
const queued = samples[12].filter(s => s.state === 'queued').length;
console.log(`livelog: ok: queued steps stayed empty, the running log grew, its timer ticked ` +
  `(${running} running, ${queued} queued at 24 s)`);
