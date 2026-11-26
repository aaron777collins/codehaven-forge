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
   `ch_core::access::PUBLIC_ROUTES` or `ch_core::access::PUBLIC_REPO_ROUTES`. Do not.
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

## The browser workspace

`/{owner}/{repo}/workspace` is a full-screen editor, commit, run and terminal panel (epic ch-1ke,
stages 1 to 3, all live). The terminal runs in the visitor's browser, never on the server. The code is in `src/views/workspace/` and the browser side in `workspace/src/` (plain JS
modules and CSS, no Node in the Rust build). Monaco is vendored, prebuilt and gzipped, in
`workspace/vendor/`, with `LOCK` and `THIRD_PARTY_NOTICES`; `cargo xtask vendor-check` checks the
hashes and the size budgets.

- **Signed in only.** Every workspace route is a page route: only a browser session opens it,
  never a token or Basic auth. Signed out, every route answers 303 to `/login` (or 401), for
  every repository and every tier, public included. Nothing is added to `PUBLIC_ROUTES`.
- **Repo permission.** Reading needs read access, and a missing or unreadable repository gives
  the same 404. Commit, branch, dispatch, re-run and cancel need write access and the CSRF token.
  Data routes also need the workspace key, which is bound to the session, the repository and the
  account.
- **Access ends.** The log streams re-check the session and the permission and close within a
  second of sign-out, suspension, removal, a password change or the admin switch.
- **Isolation.** The page's CSP keeps repository code away from the session and the API: page
  script cannot reach `/api`, and no request leaves the instance origin.
- **Switch.** `[workspace] enabled` in the config is the ceiling. A site admin with two-factor
  sign-in turns the editor off at `/admin/workspace`; the change is audited and applies on the
  next request.

### The terminal

Stage 3, in this version: a bash shell that runs in the visitor's own browser, on a copy of the
repository's files. The server starts no shell and no container for it. `docs/terminal.md` is the
user and self-hoster page.

- **Code.** `src/views/workspace/terminal.rs` serves the frame; `workspace/frame/` holds the
  frame's script, `workspace/src/terminal.js` the page side and `workspace/src/shell-worker.js`
  the Worker. The shell is just-bash 3.6.0 (Apache-2.0), drawn with xterm.js 6.0.0 (MIT), both
  vendored prebuilt in `crates/ch-web/workspace/vendor/terminal/` with their own `LOCK` and
  `THIRD_PARTY_NOTICES`, and rebuilt only by `deploy/workspace-vendor/terminal/build.sh`. That
  script needs Node and npm; a normal Codehaven build needs neither.
- **Routes.** The terminal document, its assets and the file snapshot at
  `/{owner}/{repo}/workspace/snapshot` (length-framed bytes, needs the workspace key) are signed
  in only, like every workspace route: 303 to `/login` when signed out, 404 without read access
  to the repository. Read access is enough to use the terminal; committing still needs write
  access.
- **Isolation.** The shell runs in a Web Worker inside a sandboxed iframe (`allow-scripts`, an
  opaque origin) whose policy is `connect-src 'none'` and allows only one inline script, named by
  its SHA-256 hash. The frame never receives a cookie, CSRF token, workspace key, user name or
  URL with authority. The page checks the shell's bytes against this build's hashes and hands
  them over one `MessagePort`; any unexpected message, a second "ready" or a second load closes
  the frame. The server refuses any framed request (`Sec-Fetch-Dest: iframe` and the like) by a
  signed-in browser except the terminal document itself, so the frame cannot navigate to the API.
- **Limits.** The snapshot carries up to 2,000 files or 25 MB; a bigger repository asks for a
  folder. Each command line can run up to 30 seconds and 100,000 commands, and print up to
  1 MB. Stop or Ctrl+C ends a command line; after two seconds the shell is restarted
  from the files as they were after the last finished command.
- **Review.** Nothing the terminal changes reaches drafts on its own. "Review terminal changes"
  lists each changed file, nothing ticked; a file must be opened before it can be ticked. Binary
  files, symbolic links, files inside `.git` and files over 2 MB are listed but cannot be moved.
  Ticked files become drafts marked "From terminal"; committing them shows their changes and the
  warning "CI may run this code with secrets" and needs its own tick. Commit on save never
  commits a terminal file.
- **Switch.** `[workspace] terminal` in the config is the ceiling (true by default). A site admin
  with two-factor sign-in can turn the terminal on or off at `/admin/workspace`, separately from
  the editor; the change is audited and applies on the next request.

Stage 4, the optional Linux in the tab, is not in this version: `GET .../workspace/terminal/source.tar`
answers 404.

## Packages pages

`/packages` lists every container image and NuGet package the signed-in person could pull, `/packages/{owner}`
the ones of one account, and `/{owner}/{repo}/packages` those linked to a repository they can read. A
package page (`/packages/{owner}/{kind}/{name}`, where the image name may have slashes) shows the
`docker pull` or `dotnet nuget` commands, the tags or versions with their digest, platforms, size and
push time. These pages are for signed-in people only: signed out, they ask you to sign in, and a package
you may not pull looks exactly like one that does not exist (no count of hidden ones anywhere).

The owner of a package (or an owner of the organization that owns it) can link it to a repository of the
same owner where they can write, unlink it, delete one tag or version, or delete the package. Deleting a
tag keeps its manifest when another tag or an index in the same package still points at it, so a multi-arch
image keeps working. Linking
shows who would gain access first (a public repository lets anyone pull it, signed out too) and needs a
confirmation. Unlinking leaves the package to its owner alone, never wider than before. Deleting needs the
package name typed. Each change is audited (`package.linked`, `package.unlinked`,
`package.version_deleted`, `package.deleted`). The rule for who can read is the one `docker pull` uses.

## Environments

`/{owner}/{repo}/settings/environments` (repository admins): create an environment (a name already in use,
in any case, is refused and that environment is left as it is), set its wait timer, its
branch filter and its required reviewers, and add its secrets. See docs/ci.md, "Deployment environments".
A job held at an environment shows a "Deployments" card on its run page, with Approve and Reject for a
required reviewer.

## Not built

- "Import a repository".
- Notification emails. Mail sends reset links and invites only.

## Pull requests, forks, search and mail

- **Required checks.** Branch protection takes one check name per line, so a name with a comma, such as `test (ubuntu, 1.22)`, works. The form lists the names of checks that ran on recent commits, to copy. When a required name never ran on the head commit, the merge gate names the nearest one that did.
- **Merge box.** "Edit the commit title and message" sets the merge or squash commit text. The form carries the head commit the page showed, so a push after the page loaded stops the merge. A branch of the same repository can be deleted with the merge, or afterwards with "Delete branch". The header says "merged" or "wanted to merge" once a pull request is merged or closed. An approval from someone without write access shows "does not count".
- **Side panel.** Reviewers can be requested and withdrawn by people with triage access, or by the author when they have access. Teams of the owning organization can be requested. Assignees and the milestone work as on issues.
- **Suggestions.** A ```suggestion block in a review comment is labelled "Suggested change". Someone with write access sees "Apply suggestion", which commits it to the head branch.
- **Forks.** "Fork" on the repository header opens `/{owner}/{repo}/fork`: pick your account or an organization you own, and a visibility no wider than the parent. A public fork starts as internal and is confirmed on the visibility page. `/{owner}/{repo}/forks` lists the forks you can read. On "Open a pull request", your own forks appear as a Repository choice. Without access to a repository, a pull request comes from a fork you own.
- **Watch and star.** The header has Watch (participating, all activity or ignore), Star with its count, and Fork with the number of forks you can see. Signed out, none of them show.
- **Code search.** The search page has a Code tab. It looks at the default branch of the 30 most recently pushed repositories you can read, and `repo:owner/name` narrows it to one. Each repository has a "Search code" page. Code search needs an account.
- **Issue filters.** The issue list filters by milestone and by who opened or is assigned an issue. A list narrowed to a milestone shows the milestone at the top, with its progress.
- **Notifications.** Each row links to the issue or pull request by its number and says whether it is open, closed or merged. A mention stays the reason while the row is unread.
- **Invites.** A site admin's invite can name an organization and a team. An invite for an email that already belongs to an account is refused.
- **Two-factor setup** shows a QR code as well as the key.
- **Forgot password.** `/password/forgot` asks for a username or email and always gives the same answer. With mail on, the account's primary email gets a link that works for an hour, at most one every ten minutes. Asking signs nobody out: choosing the new password does. With mail off the page says to ask a site admin. After choosing a password from any one-time link you are signed in, through two-factor when it is on.

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

## Accessibility and page times

Recorded 2026-11-23 on a throwaway stack seeded by the end-to-end run (ch-cks), with axe-core 4.14.1
in Chrome over the build host's internal network, so the times leave out the internet. 50 pages,
each at 390 and 1440 wide, 100 loads in all.

- **Before this pass**, 28 loads had serious or critical findings: link tab bars marked as ARIA tab
  lists without tabs, green text on green in diffs and on approved reviews, a scrollable diff that
  a keyboard could not reach, and a link in grey text told apart only by colour.
- **After it**, none. What is left is moderate or minor: pages with more than one banner or no
  single `main` landmark, content outside a landmark, some skipped heading levels and empty table
  headers on action columns.
- **Page times**: load event at 41 ms for half the pages, 76 ms for 95 of 100, 107 ms at most.

To run it again: `deploy/dev/shots.sh` with `SHOTS_KEEP=1`, then axe-core in each page of
`tests/e2e/shoot.mjs`, keeping findings marked serious or critical.
