// Screenshots of every main page: phone (390) and desktop (1440), light and dark.
// Runs on the build host with playwright-core and the system Chrome, against the throwaway server over its own
// network, with session cookies written by e2e_full.py (step 20).
//   node shoot.mjs <base-url> <session.json> <outdir>
// SHOTS_AXE=1 also runs axe-core (WCAG 2.0 and 2.1, A and AA) on every page, and fails on any
// serious or critical finding. axe-core is a pinned install next to playwright-core in
// $CODEHAVEN_SHOTS_NODE_MODULES (npm install --save-exact axe-core@4.10.2); the pages' own
// Content-Security-Policy stays as it is in production and is bypassed only here, to inject it.
import { chromium } from 'playwright-core';
import fs from 'node:fs';
import path from 'node:path';

const [base, sessionFile, outDir] = process.argv.slice(2);
const sess = JSON.parse(fs.readFileSync(sessionFile, 'utf8'));
fs.mkdirSync(outDir, { recursive: true });

// who: null for signed out, otherwise a key in session.json
const pages = [
  ['signin', null, '/login'],
  ['invite', null, sess.invite_path || '/invite/chi_doesnotmatter'],
  ['home', 'alice', '/'],
  ['explore', 'alice', '/explore'],
  ['new-repo', 'alice', '/new'],
  ['repo-home', 'alice', '/alice/demo'],
  ['repo-tree', 'alice', '/alice/demo/tree/main/src'],
  ['repo-file', 'alice', '/alice/demo/blob/main/src/lib.rs'],
  ['repo-blame', 'alice', '/alice/demo/blame/main/src/lib.rs'],
  ['commits', 'alice', '/alice/demo/commits/main'],
  ['commit', 'alice', '/alice/demo/commit/main'],
  ['branches', 'alice', '/alice/demo/branches'],
  ['issues', 'alice', '/alice/demo/issues'],
  ['issue', 'alice', '/alice/demo/issues/1'],
  ['pulls', 'alice', '/alice/demo/pulls?state=all'],
  ['pull', 'alice', '/alice/demo/pulls/2'],
  ['pull-files', 'alice', '/alice/demo/pulls/2/files'],
  ['pull-checks', 'alice', '/alice/demo/pulls/2/checks'],
  ['actions', 'alice', '/alice/demo/actions'],
  ['run', 'alice', `/alice/demo/actions/runs/${sess.run_id}`],
  ['job', 'alice', `/alice/demo/actions/runs/${sess.run_id}/jobs/${sess.job_id}`],
  ['repo-settings', 'alice', '/alice/demo/settings'],
  ['collaborators', 'alice', '/alice/demo/settings/collaborators'],
  ['branch-rules', 'alice', '/alice/demo/settings/branches'],
  ['repo-pages', 'alice', '/alice/demo/settings/pages'],
  ['notifications', 'alice', '/notifications'],
  ['settings-profile', 'alice', '/settings/profile'],
  ['settings-security', 'alice', '/settings/security'],
  ['settings-sessions', 'alice', '/settings/sessions'],
  ['settings-tokens', 'alice', '/settings/tokens'],
  ['settings-keys', 'alice', '/settings/keys'],
  ['admin', 'root', '/admin'],
  ['admin-users', 'root', '/admin/users'],
  ['admin-invites', 'root', '/admin/invites'],
  ['admin-runners', 'root', '/admin/runners'],
  ['admin-audit', 'root', '/admin/audit'],
  ['admin-pages', 'root', '/admin/pages'],
  ['profile', 'alice', '/alice'],
  ['releases', 'alice', '/alice/demo/releases'],
  ['release', 'alice', '/alice/demo/releases/tag/v0.0.1'],
  ['org-actions', 'alice', '/acme/slugify/actions'],
  ['org', 'alice', '/acme'],
  ['org-people', 'alice', '/orgs/acme/people'],
  ['org-teams', 'alice', '/orgs/acme/teams'],
  ['org-team', 'alice', '/orgs/acme/teams/1'],
  ['org-settings', 'alice', '/orgs/acme/settings'],
];

const sizes = [['phone', { width: 390, height: 844 }, true], ['desktop', { width: 1440, height: 900 }, false]];
const schemes = ['light', 'dark'];
const host = new URL(base).hostname;
const cookieName = sess.cookie_name || '__Host-ch_session';

const browser = await chromium.launch({ executablePath: '/usr/bin/google-chrome', args: ['--no-sandbox', '--disable-gpu'] });
const results = [];
const axeOn = !!process.env.SHOTS_AXE;
const axeSource = axeOn ? fs.readFileSync('node_modules/axe-core/axe.min.js', 'utf8') : '';
for (const [sizeName, viewport, mobile] of sizes) {
  for (const scheme of schemes) {
    const byWho = {};
    for (const [name, who, url] of pages) {
      const key = who || 'anon';
      if (!byWho[key]) {
        const ctx = await browser.newContext({ viewport, colorScheme: scheme, isMobile: mobile, hasTouch: mobile, deviceScaleFactor: 1, bypassCSP: axeOn });
        // The server expects the `__Host-` cookie, which a browser will not send over plain http.
        // The pages are shot over the internal network, so the cookie goes in as a header.
        if (who) await ctx.setExtraHTTPHeaders({ Cookie: `${cookieName}=${sess[who]}` });
        byWho[key] = ctx;
      }
      const page = await byWho[key].newPage();
      let status = 0, err = '', overflow = 0, wide = [], axe = [];
      try {
        const r = await page.goto(base + url, { waitUntil: 'networkidle', timeout: 30000 });
        status = r ? r.status() : 0;
        await page.waitForTimeout(300);
        // The page must never be wider than the window: that is a sideways scroll on a phone.
        const m = await page.evaluate(() => {
          const w = document.documentElement.clientWidth;
          const out = [];
          for (const el of document.body.querySelectorAll('*')) {
            const b = el.getBoundingClientRect();
            const shut = el.closest('details:not([open])');
            if (shut && shut !== el && !el.closest('summary')) continue; // inside a closed menu
            if (b.width > 0 && b.right > w + 1 && !el.closest('.can-right, .can-left, [data-hscroll], .sprite, .sr-only')) out.push(`${el.tagName.toLowerCase()}.${String(el.className).split(' ')[0]}:${Math.round(b.right)}`);
          }
          return { over: Math.max(0, document.documentElement.scrollWidth - w), wide: out.slice(0, 4) };
        });
        overflow = m.over; wide = m.wide;
        const file = path.join(outDir, `${name}-${sizeName}-${scheme}.png`);
        await page.screenshot({ path: file, fullPage: true });
        if (axeOn) {
          await page.evaluate(axeSource);
          axe = await page.evaluate(async () => {
            const r = await window.axe.run(document, { runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'] } });
            return r.violations
              .filter(v => v.impact === 'serious' || v.impact === 'critical')
              .map(v => ({ id: v.id, impact: v.impact, count: v.nodes.length, where: v.nodes.slice(0, 3).map(n => n.target.join(' ')), help: v.help }));
          });
        }
      } catch (e) { err = String(e).split('\n')[0]; }
      results.push({ name, sizeName, scheme, status, err, overflow, wide, axe });
      await page.close();
    }
    for (const c of Object.values(byWho)) await c.close();
  }
}
await browser.close();
fs.writeFileSync(path.join(outDir, 'results.json'), JSON.stringify(results, null, 1));
const bad = results.filter(r => r.err || r.status >= 400 || r.overflow > 0 || r.axe.length > 0);
console.log(`shots: ${results.length}, problems: ${bad.length}`);
for (const b of bad.slice(0, 40)) console.log('  ', b.name, b.sizeName, b.scheme, b.status, b.err, b.overflow ? `sideways overflow ${b.overflow}px ${(b.wide || []).join(' ')}` : '', b.axe.map(a => `axe ${a.id} (${a.impact}, ${a.count}): ${a.where.join(' | ')}`).join('; '));
if (axeOn) {
  const seen = new Map();
  for (const r of results) for (const a of r.axe) {
    const k = `${a.id} ${a.where[0]}`;
    if (!seen.has(k)) seen.set(k, { a, pages: [] });
    seen.get(k).pages.push(`${r.name}/${r.sizeName}/${r.scheme}`);
  }
  console.log(`axe: ${seen.size} distinct serious or critical findings`);
  for (const [k, v] of seen) console.log('  ', k, `(${v.a.impact}, ${v.a.help})`, 'on', v.pages.slice(0, 6).join(', '), v.pages.length > 6 ? `and ${v.pages.length - 6} more` : '');
}
