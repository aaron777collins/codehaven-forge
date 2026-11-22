# The web UI (`ch-web`, `ch-render`)

Server rendered pages: Askama templates, a little htmx, no framework, no build step beyond cargo.
`ch-web` calls the service traits in `AppState` and shapes the answers into plain view structs.
It never touches the database and never calls another domain crate.

## Layout

```
crates/ch-render/        markdown (GFM, sanitized), highlighting, ANSI logs, avatars. Pure.
crates/ch-web/
  src/lib.rs             web_routes()
  src/routes.rs          the route table. One registry, so tests can list every method and path
  src/ctx.rs             Ctx: who is calling, cookies, page(), redirect(), flash
  src/forms.rs           FormData: the only way to read a body. Checks Origin and CSRF first
  src/csrf.rs            tokens, signed cookies, the Origin rule
  src/views/*.rs         one module per area (auth, repo, issues, pulls, actions, settings, admin...)
  src/views/code.rs      HTML for code tables, blame and diffs (built in Rust, every piece escaped)
  src/demo/              the whole AppState from fixed data (feature `demo`)
  templates/             Askama templates. base.html, components.html (macros), one folder per area
  static/                app.css, code.css, forge.css, app.js, htmx, sprite is inlined from templates
  examples/demo.rs       serves the demo on 127.0.0.1:3291
  examples/snapshot.rs   renders demo pages to files for screenshots
  screens/shoot.sh       demo screenshots, shot with the headless Chrome on the build host
  tests/pages.rs         every page, signed in and out, CSRF, escaping, streaming
```

## Rules that are enforced by tests, not by care

- **CSP is `script-src 'self'; style-src 'self'`.** A template must not contain `style="..."`,
  `<script>`, `onclick=` or `javascript:`. Label colours are a palette of classes (`lbl-0` to
  `lbl-11`), progress bars are `<progress>`, behaviour is `data-*` attributes read by `app.js`.
  `every_page_renders_for_a_signed_in_user` fails on any of them.
- **Every POST goes through `FormData`.** It refuses the request unless `Origin` is ours and the
  token is valid (signed in: HMAC of the session cookie, signed out: double-submit cookie).
  `every_post_route_refuses_a_missing_token_and_a_foreign_origin` walks the route table.
- **Secrets are shown once, in the response to the request that made them** (invite links, tokens,
  recovery codes, runner tokens, one-time links). Never a redirect, a cookie or a URL.
- **A repository you cannot read is a 404**, the same as one that does not exist.
- Free text is escaped by Askama; `|safe` is used only for HTML from `ch_render` or `views/code.rs`.
- Raw files are served with `Content-Security-Policy: sandbox`, `nosniff` and `attachment` for
  HTML and SVG.
- Every page has one `id="main"`, an `<h1>`, a title, and alt text on images.

## Adding a page

1. A handler in `views/<area>.rs`: `pub async fn x(ctx: Ctx, Path(..)) -> WebResult<Response>`,
   with `form: FormData` last for a POST.
2. A template struct with `c: Chrome` first (`ctx.chrome("Title").await`), derive `Template`.
3. `templates/<area>/x.html` extending `base.html` (or `repo/layout.html` with `repo_body`).
4. Register it in `routes.rs`. A new route is private to signed out visitors until someone edits
   `ch_core::access::PUBLIC_ROUTES`. Do not.
5. Add the path to `PAGES` in `tests/pages.rs` with a word that must appear.

## Looking at it

`cargo run -p ch-web --example demo` serves every page from the demo data on 127.0.0.1:3291 of
whichever machine runs it (user `ada`, password in `src/demo/mod.rs`). The builder container
publishes no ports, so on a build host use screenshots, which need no port at all:

```sh
# in the main checkout: builds and renders the demo pages in the builder, shoots them with the
# headless Chrome on the build host, copies the PNGs to $TMPDIR/ch-screens (or the outdir
# argument) and removes everything it made there. Needs CH_HOST and CH_REMOTE_ROOT
# (deploy/dev/local.env.example) and headless Chrome on that host.
SHOTS=all bash .worktrees/web/crates/ch-web/screens/shoot.sh "repo-home,issue,pull*"
```

`SHOTS=light` (the default) is light desktop only. `all` adds dark desktop and a 390 px phone.

## Contract additions

The first service traits had no method for several pages. `ch_core::web_contract` adds them,
with `NotBuilt` answering until the owning stream implements them:

| Trait | Owner | For |
| --- | --- | --- |
| `Sessions` | ch-auth | sign in, two-factor, sessions, password, profile, security log |
| `Directory` | ch-auth | users, organisations, teams, site stats |
| `Tracker` | ch-collab | labels, milestones, inline review comments, creating releases |
| `Pipelines` | ch-ci | workflows, jobs and steps, runners, secrets, variables, artifacts |

Also `Repos::update` and `GitService::blame`. Usernames `settings`, `admin`, `new`, `explore`,
`notifications`, `search`, `login`, `logout`, `invite`, `password`, `orgs`, `avatar`, `static`,
`preview`, `api` and `v2` must be reserved by the identity stream, because `/{owner}` is a route.

## Not built

- Packages pages and "import a repository" (no service to call yet).
- A QR code for two-factor setup (no QR library; the key and the otpauth link are shown).
- Requesting a review from a person or team in the PR UI.

## Review threads, organizations and the live job log

- **Threads.** Files changed and the Conversation tab read `Reviews::threads`, so each thread has its
  id and sits once under the line of its first comment. Each thread has a Reply form (`POST
  .../pulls/{n}/inline` with `in_reply_to`) and, for the pull request's author and anyone with write
  access, a Resolve or Unresolve button (`POST .../pulls/{n}/resolve`). Both answer with a flash and
  return to the tab they came from (`back`).
- **Organizations.** An owner manages an organization from `/{org}` (New repository), `/orgs/{org}/people`
  (add, remove, role), `/orgs/{org}/teams` (new team with a permission), `/orgs/{org}/teams/{id}` (permission,
  members, repositories, delete) and `/orgs/{org}/settings` (name, description, default repository
  permission, delete). Everyone else sees the same pages read only, and the service checks every action
  again.
- **Live job log.** `GET .../jobs/{job}/log/stream` sends `log` events (JSON with the step number and
  the html of its new lines, routed by the step's byte range), `steps` events (the state, icon and
  elapsed time of every step) and `done`. The page applies each `log` event to the panel of its step,
  ticks the elapsed times every second, and reloads once when the job is done. `tests/e2e/livelog.mjs`
  checks it in a real browser.
