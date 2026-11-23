# Codehaven roadmap

How Codehaven gets built: seven streams, five milestones, one end-to-end test
that grows with each milestone. Design lives in `ARCHITECTURE.md`; this file is
the build plan.

## 1. How the work runs

- **Branches.** Each stream works on `stream/<name>` and merges to `main` by
  pull request. Bring main in with `git merge main`, never rebase a pushed
  branch, never `--force` a shared branch.
- **Where code is edited.** In a worktree inside the checkout:
  `.worktrees/<stream>`.
- **Where code is built and tested.** On the build host, always:
  `deploy/dev/remote.sh <stream> "cargo nextest run -p <crate>"`. It checks
  disk, syncs to `$CH_REMOTE_ROOT/streams/<stream>` (`/srv/codehaven` in the
  examples), takes one of 3 build slots and runs inside `codehaven-builder`.
  No cargo builds on the local machine, which runs other work too, so keep it
  to at most 2 heavy processes (rsync and ssh are not heavy).
- **Who writes what.** Each stream has a supervising agent. Narrow, settled
  coding goes to `glm -C <worktree> "..."`. The supervisor reads every diff,
  runs every command and test itself (glm cannot run commands), and owns the
  result. Judgement calls (API shape, schema, UI verdicts) stay with Claude.
- **Contracts.** `ch-core` traits, `ch-api-types` DTOs and the migrations belong
  to the foundation stream. Another stream needing a change opens a small pull
  request against those files and says why. The integrator merges it the same
  day.
- **Definition of done for a task.** Tests written first and green on the
  build host, clippy clean, `cargo build --workspace` green with main merged,
  and the milestone's e2e steps still green.
- **Commits** end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- **UI copy** follows plain writing: short, concrete, no em dashes, never blames
  the reader.
- **Dogfood.** Once M2 is merged, Codehaven's own issues move into the deployed
  instance, and once M3 is merged its own CI runs there too.

## 2. Milestones

| Milestone | Outcome | Streams merging |
| --- | --- | --- |
| **M0 Foundation** | Workspace compiles on the build host, full v1 schema migrates, every crate exists as a wired skeleton, the dev stack and remote build loop work | foundation |
| **M1 Push and browse, then public** | First admin from the CLI, invites, sign in with optional two-factor, lockout and audit, create a repo, push over HTTP and SSH, browse code with highlighting, commits, branches, tags, blame, diffs, edit a file. Ends with the public gate and going public | git, identity, web, then integration |
| **M2 Collaborate** | Issues, labels, milestones, assignees, mentions, PRs with inline review, approvals, merge, squash and rebase, protection, notifications, webhooks, search, orgs and teams | collab, identity, web |
| **M3 CI** | Workflows on push, PR, manual and schedule, matrix and needs, a registered runner in Docker, live logs, artifacts, cache, secrets, green checks on commits and PRs, merge gate on required checks | ci, runner, web |
| **M4 Polish** | Releases, container registry, forks, repo import, CLI, admin pages, backups, mobile and accessibility pass, performance pass | all |
| **M5 Later** | Cross-repo code search, LFS, pull mirrors, composite actions, service containers, WebAuthn and passkeys | as needed |

M1, M2 and M3 overlap. Every stream starts the moment M0 merges, builds against
fakes, and merges into the milestone where its features are needed.

## 3. M0: Foundation (goes first, alone)

Target: everything another stream needs to start without waiting. Nothing else
starts until this merges.

1. On the build host: `df -h / /opt /var /home`, then create `$CH_REMOTE_ROOT`
   with `src` and `streams` under it (`/srv/codehaven` in the examples) and
   clone the repo into `src`. Stop if any filesystem is at 85 percent.
2. Workspace `Cargo.toml` with every crate in ARCHITECTURE.md section 3.1 as a
   skeleton, `[workspace.dependencies]` pinning every external crate,
   `rust-toolchain.toml` pinned to the current stable, edition 2024,
   `[profile.dev] debug = "line-tables-only"`.
3. `deploy/images/builder.Dockerfile`, `deploy/compose.dev.yml` (`pg-test` on
   tmpfs, `builder`, no published ports), `deploy/dev/remote.sh`,
   `diskcheck.sh`, `clean.sh`. Create the `codehaven` buildx builder.
4. `ch-core`: `Actor`, `Permission`, ids as newtypes, `Error` and its
   `IntoResponse`, config with `figment`, `DomainEvent`, every service trait in
   ARCHITECTURE.md section 4.3, `Worker`, `AppState`, `text::refs` with property tests.
5. `ch-db`: pool, the full v1 schema as migrations (ARCHITECTURE.md section 5.3), transaction
   helper, outbox `emit` and `consume` with cursor tests, job queue with
   `SKIP LOCKED` and tests for concurrent claimers, filesystem `BlobStore`,
   sealing helpers for secrets.
6. `ch-api-types`: REST v3 DTOs for every group in ARCHITECTURE.md section 11, webhook
   payloads, runner protocol, each with a serde round-trip snapshot.
7. `ch-testkit`: `TestDb` around `#[sqlx::test]`, `TempRepo` that builds git
   repos with commits, branches and conflicts, actor and fixture builders.
8. `codehaven` binary: `serve`, `migrate`, `doctor`, `healthcheck`, `admin`
   subcommands (stubbed to the traits), `hook` (stub), and `wiring.rs` calling
   every crate's `api_routes`, `web_routes`, `workers` and `build`. The gate
   layer with `ch_core::access::PUBLIC_ROUTES`, the security headers layer, the
   trusted proxy handling for `X-Forwarded-For`, the internal listener on
   `127.0.0.1:9300`, and `tests/anonymous_surface.rs` all land here in M0, so
   every route any stream adds is private from its first commit.
9. `deploy/compose.yml` with `postgres`, `server`, `dind`, `runner`, networks
   `backend` and `runners`, ports and data bind mounts from `.env`
   (`127.0.0.1:3200` and `127.0.0.1:2222` in the examples), plus the two proxy
   overlays: `deploy/compose.caddy.yml` runs the proxy inside the project, and
   `deploy/compose.proxy-network.yml` joins a network a proxy already owns,
   under an alias. First check with `docker network inspect` that no container
   already uses that alias. Joining an existing network changes nothing for
   that proxy until its site block points at the alias, which happens at the
   end of M1, not here.
10. `tests/e2e`: the `ch-e2e` harness, `compose.e2e.yml`, a step runner, and step
    0: the stack comes up, `/readyz` is 200, `codehaven doctor` passes.
11. `cargo sqlx prepare --workspace`, commit `.sqlx/`.
12. Bring up the stack on the build host and check
    `ss -tlnp | grep -E '3200|2222'` shows only `127.0.0.1`. Do not touch the
    proxy's config.

Done when: `deploy/dev/remote.sh foundation "cargo nextest run --workspace"` is
green, the instance answers `/readyz` through `ssh -L`, and e2e step 0 passes.

After M0 the foundation stream becomes the **integration** stream: it owns
`crates/codehaven`, contract changes, `tests/e2e`, deploys to the `codehaven`
project, and merges stream pull requests in the order of section 5 below.

## 4. Stream backlogs

Each stream lists its work by milestone. Tests are written before code.

### 4.1 git (`crates/ch-git`, `crates/ch-ssh`, `crates/ch-registry`)

M1:
- `GitCommand` wrapper: clean env, timeouts, streaming, `--end-of-options`.
- Repo storage: create, delete, shard paths, hooks path, `Repos` trait over the
  `repositories`, `repo_collaborators`, `repo_redirects` tables.
- `gix` reads: refs, resolve, tree, blob (size limits, binary detection),
  log with path filter, commit, counts.
- Diff parser and `compare`, blame parser, archive, `git grep`.
- Smart HTTP for upload-pack and receive-pack, gzip bodies, protocol v2.
  Tested with the real `git` client against a live router.
- Hook socket and the push pipeline: pre-receive permission and protection
  checks, post-receive events.
- `ch-ssh`: russh server, key auth through `Authn`, exec of the three git
  commands, host key generation. Tested with `ssh` and `GIT_SSH_COMMAND`.
- `commit_file_change` with compare-and-swap.
- Repo REST endpoints in ARCHITECTURE.md section 11 (repos, branches, tags, commits, compare,
  contents, git data, readme, archives).

M2:
- `merge` (merge commit, squash, rebase) and `test_merge`, with a conflict
  matrix in tests. `refs/pull/<n>/head` and `/merge`.
- Branch protection storage and evaluation, protection REST endpoints.

M4:
- Forks that copy their objects and borrow nothing, import from URL, `git gc` jobs.
- `ch-registry`: OCI distribution, token auth, uploads, mounts, GC, passing the
  OCI conformance suite in a container on the build host.

### 4.2 identity (`crates/ch-auth`, `crates/ch-notify`)

M1:
- Accounts: users, emails, Argon2id passwords with the dummy-hash timing guard
  and the hashing semaphore, the password policy (zxcvbn, see ARCHITECTURE.md 7.4).
- Invites (ARCHITECTURE.md section 7.4): create, list, revoke, single-use accept
  in one transaction, a test that races 20 accepts of one link and gets one
  account, and identical responses for expired, used, revoked and made-up links.
  There is no sign-up code path to write.
- One-time links for set-password and reset, `admin create-user`,
  `admin reset-password`, `admin unlock`, `admin suspend`, `admin invite`.
- Sessions per ARCHITECTURE.md section 7.6: `__Host-` cookie, rotation, idle and absolute
  expiry, sign out everywhere, the sessions list. CSRF tokens, the pre-session
  double-submit cookie, and the `Origin` check.
- Rate limits keyed by the trusted client IP, and lockout with doubling, with
  tests that a locked account still works with a token and an SSH key.
- TOTP and recovery codes (moved into M1 because admin pages need them before
  the instance goes public).
- Audit events for every action in ARCHITECTURE.md section 7.7 that M1 covers.
- `Authn`: session, Basic, bearer and PAT, SSH fingerprint lookup.
- `Authz`: the permission model in ARCHITECTURE.md section 7.2, with a table-driven test of
  every row.
- PATs with scopes, SSH keys with fingerprint checks, deploy keys.
- `admin create-user`, `admin reset-password`.
- User REST endpoints.

M2:
- Orgs, members, teams, team repo permissions, org REST endpoints.
- OIDC with a mock provider in tests (a small axum issuer in `ch-testkit`):
  link to an existing account, or finish an invite. A test proves an unknown
  OIDC identity with no invite gets no account.
- `ch-notify`: notification fan-out from events with reasons, inbox, watch
  levels, mark read and done, unsubscribe; webhooks with signing, retries,
  delivery log, redeliver, private address refusal; SMTP email behind config.

### 4.3 collab (`crates/ch-collab`)

M2:
- Issues: shared numbering, labels, milestones, assignees, comments, reactions,
  lock, close reasons, timeline events, templates, filter query language.
- Mentions and cross references with `ch_core::text::refs`.
- Pull requests: create from branch, draft, head and base tracking, `pr-sync`
  consumer (new commits, outdated comments, stale approvals, closing keywords,
  merged-by-push detection).
- Reviews: pending reviews, inline comments on lines and ranges, suggestions and
  "apply suggestion", threads and resolution, review requests, CODEOWNERS.
- Merge gate and the merge endpoint calling `GitService::merge`, delete branch,
  auto-merge.
- Search over repos, issues, PRs and users, plus code search through
  `GitService::grep`.
- Issue, PR, review and search REST endpoints.

M4:
- Releases, generated notes, assets, release REST endpoints.

### 4.4 ci (`crates/ch-workflow`, `crates/ch-ci`)

M3, in this order because the runner stream needs `ch-workflow` early:
- `ch-workflow` first: typed model with spans, validation with all errors at
  once, the expression engine with property tests, matrix expansion, trigger
  matching. Fixture corpus of 30 real-world workflow files from open source
  projects (licence permitting) that must parse or fail with the right error.
- `ci-triggers` consumer, workflow discovery and validation results. Runs are
  created only when the repo owner has `can_run_ci`, and `container.options`
  is checked against the safe list in ARCHITECTURE.md section 2.5.
- Runs, jobs, steps, `needs` scheduling, `if:`, concurrency groups, cancel, re-run.
- Runner registration tokens and the runner protocol in ARCHITECTURE.md section 9.4, with
  leases and expiry.
- Logs: file append, step ranges, broadcast, SSE, retention.
- Artifacts and cache endpoints on the blob store, retention and eviction.
- Secrets and variables, sealed, scoped, never sent to fork PR jobs.
- Check runs and commit statuses, `Checks` trait, `required_satisfied`.
- Schedules with `croner`, with an injected clock in tests.
- Manual dispatch with inputs, fork PR approval.
- Actions, statuses and checks REST endpoints.

### 4.5 runner (`crates/haven-runner`, `crates/haven-cli`)

M3:
- `register` and `run` commands, `.runner` credential file, config with label to
  image mapping and concurrency.
- Protocol client from `ch-api-types::runner` with retries and backoff,
  tested against a fake server in the test itself.
- Docker executor with `bollard`: image pull, one container per job, `docker
  exec` per step, limits, cleanup on every exit path including cancel and
  runner shutdown.
- Step evaluation with `ch-workflow`, the `GITHUB_*` and `CODEHAVEN_*`
  environment, output, env, path and summary files, `hashFiles`.
- Secret masking with tests for multi-line and `::add-mask::`.
- Built-in actions: checkout, cache, upload-artifact, download-artifact,
  `docker://`.
- `codehaven-job:trixie` image.
- Disk guard and image pruning inside `dind`.

M4:
- `haven` CLI from ARCHITECTURE.md section 13, credential helper, keyring storage, built against
  `ch-api-types` and tested against a live server from the e2e stack.

### 4.6 web (`crates/ch-web`, `crates/ch-render`)

Starts in M0's shadow with the `demo` feature and fake services, so every page
exists before its backend.

M1:
- Design tokens, light and dark, theme toggle, layout, header, repo sidebar,
  the mark, Lucide sprite, empty states.
- `ch-render`: markdown (GFM, sanitized, mentions and refs linked, relative
  links resolved in READMEs), highlighting with our class palettes, ANSI to
  HTML, generated SVG avatars.
- Sign in, two-factor prompt, accept invite, set password, zero-accounts
  sign-in page, the bad-invite page. Site admin invites (create, copy link,
  list, revoke) and audit log, settings (profile, keys, tokens, two-factor,
  sessions, security log, appearance), new repo,
  repo home, tree, blob, raw, blame, history, commits, commit, branches, tags,
  compare, edit file and new file.

M2:
- Issues list and filters, issue page, new issue, labels, milestones.
- PR list, new PR, PR conversation, commits, files with unified and split diff,
  inline review comments and the review submit flow, merge box with the gate
  checklist.
- Notifications inbox and badge, search page, org and team pages, repo settings
  (collaborators, protections, webhooks).

M3:
- Actions: runs list, run graph of jobs and needs, job page with collapsible
  steps and live log over SSE, workflow list with validation errors, manual run
  form, secrets and runners settings, status icons everywhere a commit appears.

M4:
- Releases and packages pages, site admin, keyboard shortcuts, file finder.
- The phone pass at 390 px and the accessibility pass (ARCHITECTURE.md section 12.5) on every
  page, with screenshots in light and dark reviewed before merge.

Screenshots come from a headless Chromium container on the build host driven by
`chromiumoxide` in `crates/ch-web/tests/screens.rs`, at 390 x 844 and
1280 x 800, light and dark. A Sonnet agent describes what is on each one; the
look is judged by Claude.

## 5. Integration order

1. **foundation** merges alone. Everyone branches from it.
2. **git**, **identity** and **web** (demo mode) build in parallel. **ci**
   starts `ch-workflow` at the same time. **runner** starts against a fake server
   and `ch-workflow` as it lands.
3. M1 merge order: identity (Authn and Authz first, since git checks
   permissions with them), then git, then web's M1 pages switched from demo
   fakes to real services. Gate: e2e steps 0 to 9.
4. **Go public.** Integration deploys M1, runs the public gate, puts the
   instance behind the chosen proxy and makes the first admin account
   (section 7). Until then the instance is reachable only through `ssh -L`.
   Between going public and the first sign-in it has zero accounts, so all
   anyone sees is the sign-in page.
5. M2 merge order: git's merge and protection work, then collab, then identity's
   orgs, notify and webhooks, then web's M2 pages. Gate: e2e steps 0 to 17
   without the CI parts.
6. M3 merge order: ci, then runner, then web's actions pages. Gate: the full
   e2e test.
7. M4 items merge as they finish, each with its own e2e extension.

Each merge: the stream merges main into its branch, runs its tests and
`cargo build --workspace` on the build host, the integrator runs the e2e gate
in the `codehaven-e2e` project, then merges the pull request and redeploys.
After going public, every redeploy ends with `deploy/dev/public-smoke.sh`
(section 7), and a failure rolls the deploy back.

## 6. The end-to-end test

`tests/e2e` (crate `ch-e2e`) runs inside the `codehaven-e2e` compose project
on the build host against a fresh stack, with no published ports. It drives
the system the way a person would: real `git`, real `ssh`, HTTP forms with
CSRF, the REST API, and one SSE subscription. Each step is a function so
milestones can run a prefix.

The e2e stack sets `http.trusted_proxies` to the harness's own subnet, so the
harness can send `X-Forwarded-For` and act as many client IPs. That exercises
the same trusted proxy path the reverse proxy uses in production, and lets
rate limit and lockout checks run without tripping each other.

| Step | Action | Check |
| --- | --- | --- |
| 0 | Stack up with zero accounts | `/healthz` says `ok`, `codehaven healthcheck` and `codehaven doctor` pass, `/login` shows "No accounts yet" |
| 1 | Signed-out sweep: `/`, `/explore`, `/root`, `/nobody/nothing`, `/signup`, `/api/v3/user`, `/api/v3/meta`, `/api/v3/repos/x/y`, `/x/y.git/info/refs?service=git-upload-pack`, `/v2/`, `/api/runner/v1/jobs/request`, `/metrics`, `/readyz` | Pages give `303` to `/login`, API `401`, git `401` with a Basic challenge, registry `401` with a Bearer challenge, runner `401`. `/metrics` and `/readyz` are not on the public listener, so they get the same `303` as any unknown page. Responses for paths that exist and paths that don't are byte-identical. `/login` carries every header in ARCHITECTURE.md section 7.6 |
| 2 | `codehaven admin create-user --username root --admin --password-stdin`, then root signs in through the form | Signed in. Site admin pages answer "Turn on two-factor first". Root turns on TOTP with codes the harness computes, signs out and back in with a code, and the admin pages open |
| 3 | Root creates an invite for `alice` with CI allowed in Site admin > Invites, and one for `bob` with `codehaven admin invite create`. Alice and Bob accept through the invite form | Two accounts, each with `invited_by = root`. Reusing Alice's link, a revoked invite, an invite whose `expires_at` the harness moved into the past, and a made-up token all get the same "doesn't work any more" page and create nothing. 20 parallel accepts of one fresh link create exactly one account |
| 4 | Alice creates a token in settings and adds an ed25519 key through `POST /api/v3/user/keys` | Token works on `/api/v3/user`, key fingerprint stored. A token with an expiry in the past is refused |
| 5 | From one client IP, 11 sign-in attempts in a minute. From rotating IPs, 10 wrong passwords for Bob | The 11th is `429`. Bob is locked: his right password is refused with the lockout message, and his locked state does not block Alice. `codehaven admin unlock bob` clears it and Bob signs in |
| 6 | Alice creates `alice/demo` with `POST /api/v3/user/repos` | Repo page shows the empty-repo instructions. Bob (signed in, not a collaborator) gets `404` for it, the same as a repo that doesn't exist |
| 7 | Alice pushes `main` over HTTP with the token: a README, `src/lib.rs`, and `.codehaven/workflows/ci.yml` (push and pull_request; a `lint` job, then a `test` job that needs it with a 2-cell matrix, one `run` step printing `hello from codehaven`, and `upload-artifact`) | `git push` exits 0, `/branches` lists `main`, the blob page for `src/lib.rs` contains highlight classes. A push with no credentials and a push with a wrong token both fail with `401` |
| 8 | Alice branches `feature` from `main`, changes `src/lib.rs`, and pushes it over SSH on port 2222 with `GIT_SSH_COMMAND` | Push exits 0, branch listed, a shell login attempt is refused, an unknown key is refused |
| 9 | Alice adds Bob as a write collaborator; Bob clones over HTTP with his password | Clone matches `main`'s sha. After Bob turns on two-factor, the same password clone fails with the message to use a token |
| 10 | Alice protects `main`: 1 approval, required checks `ci / test (a)` and `ci / test (b)`, no direct push | A direct push by Bob to `main` is rejected with the protection message |
| 11 | Bob opens issue #1 with label `bug` mentioning `@alice` | Alice's `/api/v3/notifications` has a `mention` entry |
| 12 | Alice registers a webhook to the harness's own listener for `pull_request` and `push` | `ping` delivery arrives with a valid `X-Hub-Signature-256` |
| 13 | Alice opens PR #2 `feature` to `main` with body `Fixes #1` | PR page renders the diff, a workflow run starts for the PR head |
| 14 | Root issues a runner token in Site admin > Runners. The stack's `haven-runner` registers with it and picks up the jobs | Registering a second time with the same token fails, and so does a made-up token. The SSE stream for a `test` job delivers `hello from codehaven` before the job completes; `lint` finishes before `test` starts; both matrix check runs end `success`; the artifact downloads |
| 15 | Before review, Alice tries to merge | `405` with the gate reasons: approval missing |
| 16 | Bob adds an inline comment on line 1 of `src/lib.rs` in the PR, then approves | Review comment has path, line and side; review state `APPROVED` |
| 17 | Alice squash merges with `PUT /api/v3/repos/alice/demo/pulls/2/merge` | `merged: true`, `main` has one new commit, issue #1 is closed by the PR, the webhook got `pull_request` closed with `merged: true` |
| 18 | The push to `main` triggers the workflow again | `GET /api/v3/repos/alice/demo/commits/main/check-runs` shows every check run `completed` with conclusion `success`; the repo home page shows the success status icon beside the latest commit |
| 19 | Root opens Site admin > Audit log | It lists, with client IPs: root's sign-ins and two-factor setup, both invite creations and accepts, the refused invite attempts, Bob's lockout and unlock, Alice's token and key, the runner token and registration |
| 20 | Screenshots of the sign-in, invite, repo home, PR and job pages at 390 and 1280 px, light and dark | Files saved as test artifacts for visual review |

M1 runs steps 0 to 9 (step 7 skips the workflow checks). M2 adds 10 to 13 and 15
to 17 with checks not yet required. M3 runs everything.

## 7. Going public

A checklist the integration stream runs once, at the end of M1, and never
before the gate passes.

1. **Pass the anonymous-surface gate with zero accounts.** E2e steps 0 to 9
   green in `codehaven-e2e`; `anonymous_surface.rs` green; the deployed
   instance still has zero accounts, so there is nothing to claim and no way
   for a stranger to make one. If the proxy network mode is used, `docker
   network inspect` shows the server's alias resolving to the Codehaven
   server and nothing else.
2. **Run the smoke test from outside.** `deploy/dev/public-smoke.sh
   <base-url>`, from a machine that is not the host, while the instance is
   still reachable only through `ssh -L`: `/login` is `200` with every
   security header, the step 1 sweep gives the same answers as on the server
   itself, a signed-out `git ls-remote` fails with `401`, and an identity
   header from a proxy signs nobody in.
3. **Put the instance behind the chosen proxy.** Either run the proxy inside
   the project (`deploy/compose.caddy.yml` with `Caddyfile.example`) or join
   the network of a proxy that already runs
   (`deploy/compose.proxy-network.yml`). Point its site block for the
   instance's host at the server's alias, set `CODEHAVEN_TRUSTED_PROXIES` to
   that network's subnet, and make the proxy drop any `Remote-User` style
   header a client sends. Back the proxy config up, change only that one
   site block, validate, reload gracefully, then run the smoke test from
   step 2 again against the public address (`https://code.example.com` in
   the examples): TLS is valid and every check still passes. Confirm the
   other sites the proxy serves still answer. If any check fails, restore
   the backup, validate and reload again.
4. **Make the first admin.** On the host, as root, run
   `deploy/prod/first-admin.sh <username> <email>`. It refuses unless the
   instance has no accounts at all. It prints a link that works for 1 hour:
   open it, choose a password, then scan the two-factor code with an
   authenticator app. If the link expires, `codehaven admin reset-password
   <username>` prints a fresh one. Every account after the first comes from
   an invite: in Site admin > Invites, make a link and send it to them. Each
   link works once and expires in 7 days.

Between steps 3 and 4 the instance is public with zero accounts, so all anyone
sees is the sign-in page. After going public, every redeploy ends with the
smoke test from step 2, and a failure rolls the deploy back.

### Who can run CI

Every invited account can push code, so CI is where an invite turns into code
running on the host. Jobs run inside the separate `dind` daemon with limits
and no host socket (ARCHITECTURE.md section 2.5), and on top of that
workflows only run for repos whose owner a site admin has marked
`can_run_ci`. The first admin account has it. An invite can grant it, off by
default. A repo without it shows "Workflows are off for this account. Ask an
admin to turn them on." on its Actions tab.

## 8. Risks

| Risk | Mitigation |
| --- | --- |
| Contract churn in `ch-core` stalls streams | Traits written in full in M0; changes are small same-day pull requests; fakes absorb the rest |
| Disk on the build host | `diskcheck.sh` before every build, capped sccache, `line-tables-only` debug info, `clean.sh` after merges, runner disk guard |
| Starving other services on the build host | 3 build slots, 3 CPUs each, runner concurrency 2 with per-job limits |
| A CI job escaping onto the host | Separate `dind` daemon, no host socket, jobs off the `backend` network |
| Git edge cases (huge repos, odd refs, protocol v2) | The real `git` binary does the protocol and writes; tests use the real client |
| Workflow compatibility gaps | Fixture corpus of real workflows; unsupported features fail with a line number and a plain message |
| UI drifts toward a copy of another forge | Our own mark, palette, sidebar navigation and icons; Claude reviews screenshots against ARCHITECTURE.md section 12.3 |
| Merging three-way conflicts in parallel branches | Disjoint directories per stream; shared files are only foundation's |
| A stranger gets in on the public instance | No sign-up route exists; deny-by-default gate in M0 with a test over every route; invites single use and hashed; no web setup page; Argon2id, lockout, rate limits, two-factor for admins; smoke test after every deploy |
| A new route leaks data to signed-out visitors | Routes are private unless listed in `PUBLIC_ROUTES` or `PUBLIC_REPO_ROUTES`, which only Claude reviews changes to; `anonymous_surface.rs` fails the build |
| Someone locks the admin out by guessing at the password | Lockout blocks only password sign-in; tokens and SSH keys keep working; `admin unlock` on the host |
| An invited friend's CI job attacks the host | `can_run_ci` off by default; jobs in the separate `dind` daemon with limits; `container.options` restricted to a safe list (ARCHITECTURE.md section 2.5) |
| The proxy edit breaks other sites | Backup, `diff` showing only this instance's block, config validation before a graceful reload, other sites checked after, one-step rollback |

## 9. Streams

STREAMS:
foundation | Cargo.toml, rust-toolchain.toml, .sqlx/, crates/ch-core, crates/ch-db, crates/ch-api-types, crates/ch-testkit, crates/codehaven, deploy/, tests/e2e, xtask/ (schema, contracts, deny-by-default gate, security headers, compose, e2e, going public) | none (goes first, then becomes integration) | build host
git | crates/ch-git, crates/ch-ssh, crates/ch-registry | foundation | build host
identity | crates/ch-auth, crates/ch-notify (accounts, invites, sessions, two-factor, lockout, audit, PATs, SSH keys, authz, orgs, notifications, webhooks) | foundation | build host
collab | crates/ch-collab | foundation (calls git and identity through ch-core traits) | build host
ci | crates/ch-workflow, crates/ch-ci | foundation | build host
runner | crates/haven-runner, crates/haven-cli | foundation, ci (ch-workflow) | build host (needs Docker for the executor tests)
web | crates/ch-web, crates/ch-render | foundation | build host (builds and screenshots; visual review through ssh -L from the local machine)
