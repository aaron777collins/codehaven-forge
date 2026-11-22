// Screenshots of the connector's pages: consent, the return page, Connected apps and a run waiting for
// approval. Phone (390) and desktop (1440), light and dark. Run by connector_e2e.py on the build host
// with playwright-core and the system Chrome, against the throwaway server over its own network.
//   node shoot_connector.mjs <base-url> <session.json> <outdir>
// session.json: cookie_name, cookie, consent_path, return_html (a file), apps_path, run_path.
import { chromium } from 'playwright-core';
import fs from 'node:fs';
import path from 'node:path';

const [base, sessionFile, outDir] = process.argv.slice(2);
const sess = JSON.parse(fs.readFileSync(sessionFile, 'utf8'));
fs.mkdirSync(outDir, { recursive: true });
const returnHtml = fs.readFileSync(sess.return_html, 'utf8');

const pages = [
  ['consent', sess.consent_path],
  ['return', '(shown after Allow)'],
  ['connected-apps', sess.apps_path],
  ['run-waiting', sess.run_path],
];
const sizes = [['phone', { width: 390, height: 844 }, true], ['desktop', { width: 1440, height: 900 }, false]];
const browser = await chromium.launch({ executablePath: '/usr/bin/google-chrome', args: ['--no-sandbox', '--disable-gpu'] });
const results = [];
for (const [sizeName, viewport, mobile] of sizes) {
  for (const scheme of ['light', 'dark']) {
    const ctx = await browser.newContext({ viewport, colorScheme: scheme, isMobile: mobile, hasTouch: mobile, deviceScaleFactor: 1 });
    // The server expects the `__Host-` cookie, which a browser will not send over plain http.
    await ctx.setExtraHTTPHeaders({ Cookie: `${sess.cookie_name}=${sess.cookie}` });
    for (const [name, url] of pages) {
      const page = await ctx.newPage();
      let status = 0, err = '', overflow = 0, wide = [];
      const failed = [];
      page.on('response', (r) => { if (r.status() >= 400) failed.push(`${r.status()} ${new URL(r.url()).pathname}`); });
      page.on('requestfailed', (r) => failed.push(`failed ${new URL(r.url()).pathname} ${r.failure() ? r.failure().errorText : ""}`));
      try {
        // The return page is the answer to a POST the browser cannot make from here (the server checks the
        // Origin). It is shown as the server sent it, with its refresh to the app's own address taken out,
        // inside a real page of the same origin so that its stylesheets load.
        const r = await page.goto(base + (name === 'return' ? '/login' : url), { waitUntil: 'networkidle', timeout: 30000 });
        if (name === 'return') {
          await page.setContent(returnHtml.replace(/<meta[^>]*http-equiv="refresh"[^>]*>/i, ''), { waitUntil: 'networkidle' });
        }
        status = r ? r.status() : 0;
        await page.waitForTimeout(300);
        const m = await page.evaluate(() => {
          const w = document.documentElement.clientWidth;
          const out = [];
          for (const el of document.body.querySelectorAll('*')) {
            const b = el.getBoundingClientRect();
            const shut = el.closest('details:not([open])');
            if (shut && shut !== el && !el.closest('summary')) continue;
            if (b.width > 0 && b.right > w + 1 && !el.closest('.can-right, .can-left, [data-hscroll], .sprite, .sr-only')) out.push(`${el.tagName.toLowerCase()}.${String(el.className).split(' ')[0]}:${Math.round(b.right)}`);
          }
          return { over: Math.max(0, document.documentElement.scrollWidth - w), wide: out.slice(0, 4) };
        });
        overflow = m.over; wide = m.wide;
        await page.screenshot({ path: path.join(outDir, `${name}-${sizeName}-${scheme}.png`), fullPage: true });
      } catch (e) { err = String(e).split('\n')[0]; }
      results.push({ name, sizeName, scheme, status, err, overflow, wide, failed });
      await page.close();
    }
    await ctx.close();
  }
}
await browser.close();
fs.writeFileSync(path.join(outDir, 'results.json'), JSON.stringify(results, null, 1));
const bad = results.filter((r) => r.err || r.status >= 400 || r.overflow > 0 || r.failed.length > 0);
console.log(`connector shots: ${results.length}, problems: ${bad.length}`);
for (const b of bad) console.log('  ', b.name, b.sizeName, b.scheme, b.status, b.err, b.overflow ? `sideways overflow ${b.overflow}px ${(b.wide || []).join(' ')}` : '', b.failed.join(' '));
process.exit(bad.length ? 1 : 0);
