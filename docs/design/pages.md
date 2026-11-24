# Codehaven Pages

Static sites published from a repository, like GitHub Pages. Epic ch-125. The addresses here
use `pages.example.com` for the pages host and `code.example.com` for the app.

## 0. The short version

- A repository admin turns Pages on and picks a branch and a folder (`/` or `/docs`). Every
  push to that branch publishes a new copy of the folder. The last good copy keeps serving
  when a publish fails, and an older copy can be put back.
- Sites live at `https://pages.example.com/<owner>/<project>/`. They never share the app's
  origin. Each response carries a CSP sandbox without `allow-same-origin`, so a page runs in
  an opaque origin: no cookies, no real storage, no credentialed reads of the app or of
  sibling sites. Cookies are stripped both ways.
- `localStorage` works per project through a bridge: a Codehaven page on the pages host
  holds the real storage and hands each project its own part.
- Who can see a site follows the repository: public is anyone, internal is any signed-in
  account, private is people with access. The admin may narrow a site, never widen it.
  Signed-in viewing uses a short-lived token made for one site and one person. It never
  carries the app session.
- Full mode, off by default: an instance admin switch serves every site at
  `https://<owner>.pages.example.com/<project>/` with no sandbox, like GitHub Pages. It needs
  a wildcard certificate for `*.pages.example.com`.

## 1. Settings

| Setting | Where | Default | Meaning |
| --- | --- | --- | --- |
| `pages.host` | config, `CODEHAVEN_PAGES_HOST` | none | The pages host name, for example `pages.example.com`. Unset turns Pages off: no tab, no routes, nothing served |
| `pages.max_files` | config | 2000 | Files in one published copy |
| `pages.max_bytes` | config | 50 MiB | Bytes in one published copy |
| `pages.reply_stall_secs` | config | 60 | A reply that sends nothing for this long is cut and gives its place back |
| `pages.reply_deadline_secs` | config | 600 | No reply takes longer than this |
| `pages.replies_per_address` | config | 32 | Replies one client address may have open at once |
| full mode | site admin, `/admin/pages` | off | Subdomain sites with no sandbox |

The pages host must differ from the app's host and must not be a parent of it. The server
refuses to start otherwise.

## 2. Routes

### On the pages host (`pages.example.com`)

The server picks the pages router by the `Host` header before any app layer runs, so no app
route, cookie, CSRF token or session lookup is reachable on the pages host.

| Request | Answer |
| --- | --- |
| `/<owner>/<project>/<path>`, `Sec-Fetch-Dest: document` | The shell: a Codehaven page that frames the content (section 4). No database lookup |
| `/<owner>/<project>/<path>`, any other destination | The content, sandboxed |
| `/-/t/<token>/<owner>/<project>/<path>` | The same two, for a signed-in viewer (section 5) |
| `/<owner>/<project>` | 301 to `/<owner>/<project>/` |
| `/robots.txt` | `Disallow: /` |
| anything else | 404 |

Only `GET` and `HEAD`. Anything else is 405.

### On a subdomain (`<owner>.pages.example.com`), full mode only

| Request | Answer |
| --- | --- |
| `/<project>/<path>` | The content, no sandbox, no shell, no bridge |
| `/-/t/<token>/<project>/<path>` | The same, for a signed-in viewer |
| `/<project>` | 301 to `/<project>/` |
| anything else, or full mode off | 404 |

With full mode on, a document request for the path form redirects to the subdomain form.

### In the app (`code.example.com`)

| Route | Who | What |
| --- | --- | --- |
| `GET /{owner}/{repo}/settings/pages` | repo admin | settings, status, copies |
| `POST /{owner}/{repo}/settings/pages` | repo admin | turn on, or change branch, folder or audience |
| `POST /{owner}/{repo}/settings/pages/deploy` | repo admin | publish the branch head again |
| `POST /{owner}/{repo}/settings/pages/rollback` | repo admin | serve an older copy |
| `POST /{owner}/{repo}/settings/pages/disable` | repo admin | turn off and delete every copy |
| `GET /{owner}/{repo}/pages/open?path=` | signed in, may see the site | make a token, 303 to the site |
| `GET`, `POST /admin/pages` | site admin with two-factor | the full mode switch |

All of these are ordinary signed-in routes. None is on the public route lists, so signed out
they answer the standard redirect to `/login`, whatever the repository.

## 3. Path handling

1. The raw path is at most 2048 bytes and is split on `/`. Each segment is percent-decoded and
   must be UTF-8. A decoded segment of `.` or `..`, or one holding `/`, `\` or a control
   character, is a 404. Empty segments (`//`) are a 404.
2. `<owner>` and `<project>` must pass the account and repository name rules before any
   lookup. The project name is looked up without `.git`.
3. The rest names a file inside the published copy. An empty rest, or one ending in `/`,
   means `index.html` there.
4. Lookup is exact and case sensitive, in the copy's file list in the database. Nothing is
   ever read from a file system path built from the request.
5. Not found and no extension: try `<rest>.html`, then `<rest>/index.html`, which redirects to
   add the slash.
6. Still not found: the copy's own `404.html` with status 404, or Codehaven's 404 page.

Publishing reads the folder from git at one commit. Symbolic links, submodules and names that
are not UTF-8 are left out, so a link can never point a request outside the copy.

## 4. Isolation (path form)

### Headers

Content:

```
Content-Security-Policy: sandbox allow-scripts allow-forms allow-popups allow-modals
  allow-downloads allow-pointer-lock allow-top-navigation-by-user-activation
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
Cache-Control: no-cache  (ETag on everything but HTML)
X-Robots-Tag: noindex, nofollow
```

No `allow-same-origin`, no `allow-popups-to-escape-sandbox`, no `allow-top-navigation`
without a click. Popups inherit the sandbox.

The shell:

```
Content-Security-Policy: default-src 'none'; script-src https://pages.example.com/-/shell.js;
  style-src https://pages.example.com/-/shell.css; frame-src 'self'; base-uri 'none';
  form-action 'none'; frame-ancestors 'none'
X-Frame-Options: DENY, Referrer-Policy: no-referrer, Cache-Control: no-cache
```

### Cookies

The pages router drops `Cookie`, `Authorization` and `Proxy-Authorization` from every request
before a handler runs, and `Set-Cookie` from every response. The proxy does the same
(`deploy/Caddyfile.example`). Nothing on the pages host ever sets a cookie.

So a hostile page on the pages host, inside the single sign-on cookie's scope, cannot read a
cookie or set one for the parent domain, and its requests to the app or a sibling app go out
from an opaque origin, so no response is readable without CORS, which Codehaven never sends.

Whether `SameSite=Lax` cookies go with those requests depends on the browser. Chrome and
Firefox treat a request from an opaque origin as cross-site and keep them home. Safari
(WebKit) looks only at the top-level page, which is on the pages host, a sibling of the app
in the same site, so it sends them: the app's session and any single sign-on cookie of the
parent domain go with every request such a page makes. The app therefore never lets the
session cookie sign in a request that another site's page made. The gate reads
`Sec-Fetch-Site`, and unless it is `same-origin` or `none`, only a top-level `GET` or `HEAD`
navigation (`Sec-Fetch-Mode: navigate`, `Sec-Fetch-Dest: document`) keeps the session, which
is all `SameSite=Lax` was meant to allow. A browser that sends no fetch metadata is judged by
`Origin: null` on a `GET` or `HEAD`. Forms and other writes already fail the origin and CSRF
checks. Other apps under the same parent domain need the same care: in Safari, a page on the
pages host can send them blind requests that carry the single sign-on cookie.

### The storage bridge (option B)

A sandboxed page has no `localStorage`. The bridge gives it one, per project:

1. A document request gets the shell. The shell owns the real `localStorage` of the pages host
   and keeps one entry per project, `chp:s:<owner>/<project>` (lowercase), decided from its
   own address and never from the child.
2. For each load the shell makes a fresh iframe, with a new random secret, and sets the frame's
   `name` to `{chp, secret, that project's data}` before it is attached. The frame's first
   document is the empty initial one, so the only document that can see the name is the one
   our server returns for that project's address.
3. The server puts a small script first in every HTML file it serves as content (after a
   doctype and leading comments). It reads `window.name`, clears it before any page script
   runs, and replaces `localStorage` with an in-memory store filled from it. `sessionStorage`
   and `document.cookie` become in-memory stores for the page too, so code that touches them
   does not throw.
4. Writes go to the shell by `postMessage` with the secret. The shell checks
   `event.source` is its frame and the secret is the current one, enforces 1 MB per project,
   and saves. A document that navigated into the frame later, or a page on another site,
   never has the secret.
5. Clicks on links inside the project go to the shell, which makes a fresh frame for the new
   page, so each page starts with the current data. Links that leave the project open at the
   top. A page that arrives without the secret (a script set `location`) asks the shell to
   load it again properly; the shell does so only for addresses inside its own project, once.
6. The page title and address (including `pushState`) are reported to the shell, which shows
   them. Back and forward work through the shell's history.

The security review attacks this: a hostile project framing, navigating into, or posting to
another project's shell, and reading `window.name` at every step.

### What is left compared with GitHub Pages

- Storage is a snapshot per page load. Two tabs of one site do not see each other's writes
  until reload, and there is no `storage` event.
- No IndexedDB, service workers, real cookies or Cache API.
- Popups opened by a page stay sandboxed, so another site opened in a new tab from a page may
  not work.
- A page cannot be framed by another page of any site (the parent is an opaque origin).
- `Range` requests are not supported.
- Search engines are asked not to index, like the rest of Codehaven.
- No custom domains, no workflow-built sites, no REST API yet.

Full mode removes the first four.

## 5. Who can see a site

The site's tier is the lowest of: the tier the copy was published at, the repository's tier
now, and the audience the admin picked.

| Tier | Who |
| --- | --- |
| public | anyone, with the instance's public switch on |
| internal | any signed-in, active account |
| private | people with a grant on the repository |

### Signed out

Content of a site that is not public right now, and of an address with no site, is one
identical page: "Sign in to see this site", status 404. Private, internal, missing, disabled
and not-yet-published all look the same. The page asks the shell to go to the app's
`/pages/open` route, which signed out is the standard `/login` redirect.

### Signed in: the per-site token

`GET /{owner}/{repo}/pages/open?path=` checks the viewer may see the site at its tier now
(missing and not allowed are the app's ordinary 404), then makes a token and answers 303 to
`https://pages.example.com/-/t/<token>/<owner>/<project><path>`.

- The token is `v1.<repo id>.<user id>.<expiry>` and an HMAC-SHA256 of it, keyed from the
  master key with its own label. It lasts one hour. It names one site and one person and
  carries nothing of the session.
- Every request with a token checks the signature, the expiry, that it names this site, and
  then the person's access again, live. A suspended account or a revoked grant stops at once.
- The shell keeps the token in the tab's `sessionStorage` and shows the plain address, so a
  copied link carries no token. Relative links keep the token in content requests. When a
  token runs out, the sign-in page sends the viewer through `/pages/open` again, which is
  instant while signed in; a second try within a minute stops with a message instead.
- In a private or internal site, image, style and script addresses must be relative. Links
  between pages may be absolute.

## 6. Publishing

- Turn on: the admin picks a branch (`refs/heads` only, never `refs/pull`), a folder (`/` or
  `/docs`), and who can see it (up to the repository's tier). A copy of the branch head is
  queued.
- Each push or branch creation on that branch queues a copy of the new head. A worker builds
  one copy at a time: it reads the folder at that commit, refuses it over the limits, stores
  each file in the blob store and records the list. The newest ready copy becomes live.
- A failed copy says why on the settings page (too many files, too big, folder missing,
  empty) and the live copy stays.
- The five newest ready copies are kept. "Serve this copy" puts an older one back. Turning
  Pages off deletes the site and every copy. Deleting the repository does too.
- Every change is audited: `pages.enabled`, `pages.changed`, `pages.deploy`,
  `pages.rollback`, `pages.disabled`, `pages.full_mode_changed`.

## 7. Limits

| What | Limit |
| --- | --- |
| files in a copy | 2000 |
| bytes in a copy | 50 MiB |
| copies kept | 5 ready, plus the live one |
| request path | 2048 bytes |
| storage per project (bridge) | 1 MB |
| token lifetime | 1 hour |
| content requests being looked up at once | 64, then 503 with `Retry-After` |
| content replies still being sent | 512, then 503 with `Retry-After` |
| content replies open from one client address | 32; past that a request waits up to 1 second for one, then 503 with `Retry-After` |
| a reply that sends nothing | cut after 60 seconds |
| one reply, start to end | cut after 10 minutes |

No server-side code runs for a site. Files are served as stored with a type from the file
extension, and an unknown extension is `application/octet-stream`. Text says
`charset=utf-8` only when the file is valid UTF-8, which is checked once when the copy is
made.

Every file is streamed from the blob store. A reply never holds a whole file in memory: at
most one piece of it, and for HTML on the path form the first 64 KiB, read to place the
bridge. A reply keeps its place in the 512 until its body is sent or the client goes away,
so clients that open replies and never read them cannot grow the server's memory.

Those places cannot all be held by a few clients either. A reply that sends nothing for 60
seconds, or is still going after 10 minutes, gives its place back at once and ends in an
error, so the client never gets a file that only looks complete. The clock runs on its own,
not when the client reads, because a client that never reads is exactly the one to catch.
One client address (the trusted address the app uses, with IPv6 grouped by /64) may have 32
replies open at once, and a reply counts against its address until its connection lets it
go, so one address cannot pile up connections that never read. A request past the 32 waits
up to a second for one of them to finish, holding nothing else, so a page that asks for many
small files at once still gets them all. The server runs with 65536 open files
(`deploy/compose.yml`), since each open reply holds a socket and a file.

## 8. Failure pages

| Case | Status | Page |
| --- | --- | --- |
| no site, not visible, signed out, token expired | 404 | Sign in to see this site |
| file missing in a visible site | 404 | the site's `404.html`, or "Page not found" |
| bad path | 404 | Page not found |
| method other than GET or HEAD | 405 | plain text |
| too busy | 503 | plain text, `Retry-After: 5` |
| `/pages/open` not allowed or missing | 404 | the app's ordinary not found |

Codehaven's own pages on the pages host carry the bridge too, so they clear a frame's name.

## 9. Full mode

One instance setting, off by default, site admin with two-factor only, audited. Turning it on
shows: "Every account on this instance can then publish pages that run with full browser
features on a subdomain of the pages host. They can set cookies for the parent domain and
read their own storage. Only turn this on if you trust every account."

- Sites serve at `https://<owner>.pages.example.com/<project>/` with no CSP sandbox, no shell
  and no bridge: native storage, cookies, IndexedDB and service workers. Storage is shared
  within one owner, as on GitHub.
- Access, tokens, cookie stripping, limits and the never-the-app-origin rule are unchanged.
- Owner names are already valid DNS labels (letters, digits and single hyphens, at most 39).
  A host whose first label is not one is a 404.
- Certificates: a wildcard for `*.pages.example.com` (a DNS challenge in the proxy). A
  wildcard keeps owner names out of public certificate logs.

Codehaven's own defences against a hostile same-site page: its session and CSRF cookies are
`__Host-` cookies, so a sibling cannot overwrite them; every write checks `Origin` against the
app's own origin; and it never reads a cookie set for the parent domain.

## 10. Deploying it

1. Point the pages host at the server (DNS) and add a proxy site for it that strips cookies
   and identity headers and sends everything to Codehaven (`deploy/Caddyfile.example`).
2. Set `CODEHAVEN_PAGES_HOST`.
3. For full mode, add the wildcard name with a certificate, then turn the switch on at
   `/admin/pages`.
