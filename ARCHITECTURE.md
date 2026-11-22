# Codehaven architecture

Codehaven is a self-hosted code forge written in Rust: git hosting over HTTP and
SSH, issues, pull requests with review, CI with registered runners, releases and
a container registry. One server binary, one runner binary, one CLI, Postgres,
and bare git repositories on disk.

This file records the design decisions. `ROADMAP.md` says who builds what and in
which order. When the two disagree, fix whichever is wrong in the same commit.

## 1. Decisions at a glance

| Area | Decision | Why |
| --- | --- | --- |
| Language | Rust everywhere: server, runner, CLI, tests, tooling (`xtask`) | One toolchain, one type system across server and runner |
| Edition and toolchain | Edition 2024. `rust-toolchain.toml` pins the stable release current at bootstrap, exactly | Every stream and the builder image compile with the same compiler |
| HTTP | `axum` 0.8 on `tokio`, `tower-http` for compression, tracing, timeouts, CSP headers | Most used async stack, first-class streaming bodies for git and logs |
| Database | Postgres 17, `sqlx` 0.8 with compile-time checked queries and committed `.sqlx/` offline data | Schema drift becomes a compile error in every stream |
| Git reads | `gix` (gitoxide) for refs, trees, blobs, commit walks | Fast, in-process, no fork per page view |
| Git writes and protocol | The `git` binary (2.47 from Debian trixie): `upload-pack`, `receive-pack`, `merge-tree`, `commit-tree`, `update-ref`, `blame`, `diff`, `archive` | git's own algorithms are the reference. Rewriting pack negotiation or merge is a bug farm |
| SSH | `russh` server inside the server binary | No system sshd, no `authorized_keys` file games |
| Web UI | Server-rendered `askama` templates, `htmx` 2, a small vanilla `app.js` | Section 12 has the reasoning |
| Highlighting | `syntect` plus `two-face` syntaxes, class-based output, our own CSS themes | Light and dark switch in CSS with no re-render |
| Markdown | `comrak` (GFM) then `ammonia` sanitizer | GFM tables, task lists, footnotes, autolinks. Sanitized always |
| Runner | Separate binary `haven-runner`, registers with a token, long-polls for jobs, runs steps in Docker through `bollard` | Isolated from the server, can run on any host |
| CLI | `haven`, a REST v3 client | Day to day work from the terminal |
| API | REST at `/api/v3`, shaped like GitHub's v3 for the common endpoints | Tools that accept a GitHub Enterprise base URL can point at Codehaven |
| Background work | Postgres outbox for domain events plus a Postgres job queue (`FOR UPDATE SKIP LOCKED`) | No Redis, no broker. One less service to run |
| Config | `codehaven.toml` plus `CODEHAVEN__SECTION__KEY` env overrides, loaded with `figment` | Secrets come from files, never from the TOML |
| Exposure | Behind any reverse proxy chosen by `CODEHAVEN_PROXY`, no SSO needed, git over SSH published where `CODEHAVEN_SSH_BIND` says | People need to reach it, and the app secures itself. Section 2.4 |
| Access | Invite-only. No open sign-up, ever. Signed-out visitors see the sign-in page, the invite page and static files, nothing else | Only invited, signed-in accounts can do anything. Section 7.3 |
| First admin | Created by `codehaven admin create-user --admin` on the host. There is no web setup page | Nobody on the internet can claim a fresh instance. Section 7.5 |

## 2. Deployment

### 2.1 Layout

One install root holds everything, `/srv/codehaven` in the examples below,
written as `<install root>`. No deploy script has a host, domain, path or
project name built in: every one comes from the env file (`.env`, copied from
`.env.example`) or the command line, and a setting that is missing stops with
its name instead of falling back to a guess.

```
<install root>/
  src/                     main checkout (git clone of codehaven, branch main)
  streams/<stream>/        per-stream copies synced from the working machine for builds and tests
  data/                    CODEHAVEN_DATA_DIR
    postgres/              Postgres data directory
    git/                   bare repositories, sharded by repository id
    blobs/                 content-addressed blobs: artifacts, caches, release assets, registry layers
    logs/                  CI job logs, one file per job
    secrets/               master.key (0600), generated on first start
    dind/                  Docker data root of the runner's own Docker daemon
  secrets/                 CODEHAVEN_SECRETS_DIR: database credentials, made by setup.sh
  backups/                 CODEHAVEN_BACKUP_DIR, outside the checkout
  run/                     build slot lock files
  codehaven.env            instance settings (no secrets inline)
```

`deploy/prod/setup.sh` (run once, as root) checks the host, creates the
directories, makes the server's uid 10001 own the ones it writes to, and
generates the database credentials into `CODEHAVEN_SECRETS_DIR`. Build caches
(cargo registry, sccache, per-stream target dirs) are Docker named volumes
prefixed `codehaven-`, so they sit under Docker's own data root and are removed
by name. `deploy/dev/diskcheck.sh` runs before every build and stops it when a
watched disk (`CODEHAVEN_DISK_PATHS`) is at `CODEHAVEN_DISK_LIMIT` percent or
more, 85 by default.

### 2.2 Compose projects

| Project | File | Services | Purpose |
| --- | --- | --- | --- |
| `CODEHAVEN_PROJECT` (`codehaven` by default) | `deploy/compose.yml` | `postgres`, `server`, and behind the `ci` profile `dind`, `work-init`, `runner` | The running instance. CI starts when the runner exists: `--profile ci` |
| `codehaven-dev` | `deploy/compose.dev.yml` | `pg-test` (tmpfs), `builder` (run on demand) | Builds and tests for every stream |
| a throwaway project per run | `deploy/compose.e2e.yml` | a fresh copy of the instance plus the `e2e` harness container | The end-to-end test, torn down after each run |

Never run `docker compose` on `deploy/compose.yml` by hand:
`deploy/compose.sh` loads the env file, works out the host's public addresses,
adds the compose file for the proxy mode (section 2.4) and passes the project
name.

Networks inside the instance project:

- `backend`: `postgres` and `server`. Nothing else, and no route out.
- `runners`: `server`, `dind`, `runner`. Job containers are started by `dind`
  and route out through it, so they can reach the server's API and the
  internet but not Postgres.
- The proxy network, only in a proxy mode: an external network you already run
  (`network` mode) or an `edge` network the compose file creates (`caddy`
  mode). `server` only, under its alias. This is how the reverse proxy reaches
  Codehaven, with no host port involved. Postgres, `dind` and the runner are
  never on it.

Published host ports, where `CODEHAVEN_HTTP_BIND` and `CODEHAVEN_SSH_BIND` say,
loopback by default:

| Bind | Service | Use |
| --- | --- | --- |
| `127.0.0.1:3200` | server HTTP | operator API calls with a token through `ssh -L`, health checks from the host |
| `127.0.0.1:2222` | server SSH | git over SSH through a tunnel |

The `caddy` proxy mode also publishes 80 and 443. The dev project publishes no
ports at all, and the e2e harness runs inside the e2e network.

### 2.3 Reaching it

`server.external_url` is `CODEHAVEN_EXTERNAL_URL`, the address people open (in
the examples here `https://code.example.com`), so every link and clone URL the
UI shows is the public one. `ssh.external` is `CODEHAVEN_SSH_EXTERNAL`, and
when it is a loopback address the UI labels the clone URL "SSH (needs a tunnel
to the server)".

| Who | How |
| --- | --- |
| Anyone with an account | `https://code.example.com` in a browser |
| git over HTTPS | `git clone https://code.example.com/alice/demo.git` with a personal access token (or a password if the account has no two-factor) |
| git over SSH | `git clone ssh://git@127.0.0.1:2222/alice/demo.git`, against `CODEHAVEN_SSH_BIND`. When that is loopback it takes a tunnel first: `ssh -L 2222:127.0.0.1:2222 <the host>`. Only people with a login on the host can do this |
| Docker registry | `docker login code.example.com` with a username and a token |
| Operator | `ssh -L 3200:127.0.0.1:3200 <the host>` for API and git calls that skip the proxy. Browser forms only accept the public origin (section 7.6), so sign in through it |

### 2.4 Exposure and the reverse proxy

Codehaven is built to sit behind a reverse proxy, and it needs no SSO gate in
front of it: a forge behind one cannot serve git clients (they cannot answer a
two-factor prompt) or people who have no account on the SSO. Codehaven protects
itself instead: invite-only, no anonymous read (section 7.3), rate limits and
lockout (section 7.6). How the server is reached is one setting,
`CODEHAVEN_PROXY`, in three modes, and `deploy/compose.sh` adds the compose
file for the one in the env file:

- **`none`**: no proxy. The ports `CODEHAVEN_HTTP_BIND` and
  `CODEHAVEN_SSH_BIND` publish are all there is. Fine for a trial on one
  machine.
- **`network`**: a proxy you already run, on a docker network that already
  exists (`CODEHAVEN_PROXY_NETWORK`). The server joins that network under the
  alias `CODEHAVEN_PROXY_ALIAS` and your proxy sends to `<alias>:3200`.
  Codehaven creates nothing on that network and changes nothing about your
  proxy: the site that points at the alias is yours to add.
- **`caddy`**: a Caddy that comes with Codehaven (`deploy/compose.caddy.yml`
  and `deploy/Caddyfile.example`). It publishes 80 and 443 on the host, so the
  name in `CODEHAVEN_SITE_HOST` must already point at this machine, and it gets
  and renews the TLS certificate on its own. It sends everything to the server
  with `flush_interval -1`, so live CI logs and git packs stream.

**Trusted proxies.** `http.trusted_proxies` (`CODEHAVEN_TRUSTED_PROXIES`, a
JSON list, `[]` when nothing sits in front) names the network or networks the
proxy reaches the server from, for example `["10.20.16.0/24"]` for the subnet
the `caddy` mode creates. Codehaven trusts exactly two things from a peer
inside one of those networks: `X-Forwarded-For`, the client address used by
rate limits and the audit log, and `X-Forwarded-Proto`. From any other peer
both headers are ignored and the socket address is used.

**The Remote-* rule.** Codehaven never trusts an identity header: it ignores
`Remote-User` and every other identity header wherever the request comes from
(section 7.1). The proxy should still delete `Remote-User`, `Remote-Groups`,
`Remote-Email` and `Remote-Name` from what a client sends, which is what
`Caddyfile.example` does, so a client can never spoof an identity to anything
else behind the same proxy either.

Being on the proxy network means the other services on it can reach the server
directly. That is acceptable because Codehaven treats every request the same
way wherever it comes from: no session or token, no data.

### 2.5 Runner isolation

The runner never gets the host's Docker socket: other services on the host may
run on that daemon, and a job with the host socket is root on the host. Instead:

- `dind` is `docker:<pinned>-dind`, privileged, with its own data root at
  `data/dind` under `CODEHAVEN_DATA_DIR` and TLS on the `runners` network.
- `runner` talks to it with `DOCKER_HOST=tcp://dind:2376` and the client certs
  from a shared volume.
- A shared volume `runner-work` is mounted at `/work` in both `runner` and
  `dind`. The runner writes checkouts to `/work/<job>` and job containers bind
  `/work/<job>` at the same path. Built-in actions run inside the runner
  process, not in the job container.
- Each job container gets `--cpus 4 --memory 8g --pids-limit 4096`. Runner
  concurrency defaults to 2.
- The runner prunes images unused for 7 days inside `dind` (never the images its labels map
  to, which may exist only there) and refuses new jobs when a guarded file system passes 85
  percent. It guards `/work` and, through a read-only mount of the dind data root at
  `/dind-data`, the disk the images live on. A one-shot `work-init` service makes the shared
  volume writable by the runner's uid 10001.
- Every container the runner starts gets a bridge network of its own, removed with it (and
  swept after a crash), so two jobs never share a wire, and `NET_RAW` is dropped, so a job
  cannot sniff or spoof. `dockerd` also runs with `--icc=false`. A job reaches the internet
  and nothing else on the host: `deploy/dind/guard.sh` drops its traffic to the host's
  private ranges and to the host's own public addresses, except tcp/443 (the public https
  site, which the reverse proxy serves to anyone). `docs/runner.md`, "What a job can reach".
- Job containers are never privileged. `container.options` accepts only
  `--cpus`, `--memory`, `--env`, `--workdir`, `--user` and `--shm-size`, each
  capped by the runner's limits. `--privileged`, volumes, devices, added
  capabilities and host networking fail validation with the line number.
- The instance is public and every invited account can push, so workflows run
  only for repos owned by an account (or org) that a site admin has marked
  `can_run_ci`. Off by default. `ROADMAP.md` section 7.3.

## 3. Workspace and crates

### 3.1 Layout

```
codehaven/
  Cargo.toml              workspace, [workspace.dependencies] with every version pinned
  rust-toolchain.toml
  .sqlx/                  offline query metadata, one file per query
  crates/
    ch-core/              ids, errors, config, Actor, Permission, service traits, DomainEvent, text refs
    ch-db/                pool, migrations/, transactions, outbox, job queue
    ch-api-types/         REST v3 DTOs, webhook payloads, runner protocol DTOs
    ch-testkit/           Postgres test harness, temp git repos, fixture builders
    codehaven/            server binary: config, wiring, admin and hook subcommands
    ch-git/               repo metadata, on-disk storage, git reads and writes, smart HTTP, hooks, protections, repo API
    ch-ssh/               SSH server for git
    ch-registry/          OCI distribution registry
    ch-auth/              accounts, sessions, OIDC, TOTP, PATs, SSH keys, orgs, teams, authz, user and org API
    ch-notify/            notifications, webhooks, email
    ch-collab/            issues, labels, milestones, pull requests, reviews, merge gate, releases, search
    ch-workflow/          workflow YAML model, validation, expressions, matrix expansion (pure, no IO)
    ch-ci/                triggers, scheduler, job queue, runner API, logs, artifacts, cache, secrets, checks, statuses
    ch-render/            markdown, highlighting, ANSI to HTML, avatars
    ch-web/               HTML pages, askama templates, htmx partials, SSE, static/
    haven-runner/         runner binary
    haven-cli/            `haven` CLI binary
  deploy/                 compose files, Dockerfiles, dev scripts, example config
  tests/e2e/              `ch-e2e` end-to-end harness
  xtask/                  cargo xtask: fmt, lint, sqlx-prepare, release, e2e
```

### 3.2 The dependency rule

This is what makes parallel streams possible:

1. Domain crates (`ch-git`, `ch-ssh`, `ch-registry`, `ch-auth`, `ch-notify`,
   `ch-collab`, `ch-ci`, `ch-web`) depend only on `ch-core`, `ch-db`,
   `ch-api-types`, and the pure libraries `ch-workflow` and `ch-render`.
2. Domain crates never depend on each other. When `ch-collab` needs a merge, it
   calls `ch_core::services::GitService`, a trait. `ch-git` implements it.
3. Only the `codehaven` binary depends on every crate. It builds the concrete
   services and puts them into `AppState` as `Arc<dyn Trait>`.

So `cargo nextest run -p ch-collab` compiles without `ch-git`, and a stream can
finish its crate against fakes before the crates it calls exist.

`haven-runner` depends on `ch-api-types` and `ch-workflow` only. `haven-cli`
depends on `ch-api-types` only.

### 3.3 Mount points

The foundation stream creates every crate as a skeleton on day one, each with:

```rust
pub fn api_routes() -> axum::Router<AppState>;   // mounted under /api/v3
pub fn workers(state: &AppState) -> Vec<ch_core::Worker>; // background loops
pub fn build(b: &AppStateBuilder) -> ch_core::Result<XServices>; // XServices: ch_core::Install
```

Crates with a different mount add one more function: `ch_git::http_routes()` (smart HTTP, at the
root), `ch_ci::runner_routes()` (under `/api/runner/v1`), `ch_registry::registry_routes()` (at
`/v2`) and `ch_web::web_routes()` (at the root). A crate that builds several services returns a
struct holding them (`AuthServices`, `GitServices`, ...) whose `install` moves them into the
`AppStateBuilder`. Unbuilt services are `ch_core::not_built::NotBuilt`, which refuses everything,
so an unfinished crate can never grant access.

`crates/codehaven/src/wiring.rs` calls all of them from the start, in dependency order (auth,
notify, git, collab, ci), so wiring is written once and streams only fill in bodies.

## 4. Contracts in ch-core

The foundation stream writes these first. A change after M0 is a pull request
reviewed by the integrator and announced to every stream.

### 4.1 Identity of the caller

```rust
pub enum Actor {
    Anonymous,
    User   { id: UserId, is_admin: bool, has_2fa: bool, via: AuthVia }, // Session, Basic, Ssh
    Token  { user_id: UserId, token_id: TokenId, scopes: Scopes },  // personal access token
    Job    { job_id: JobId, repo_id: RepoId, perms: JobPerms },      // CI job token
    Runner { runner_id: RunnerId },
    System,                                                          // hooks, schedulers, migrations
}

#[derive(PartialOrd, Ord)]
pub enum Permission { None, Read, Triage, Write, Maintain, Admin }
```

`Anonymous` exists so the gate (section 7.3) has something to reject. The gate also limits
which credential opens which kind of route (`ch_core::access::actor_accepted`): a browser
session opens pages and the API, Basic and personal access tokens open the API, git and the
registry, a job token opens the runner API, git and the API, and a runner credential opens the
runner API only. A credential that is valid but not accepted for a route is treated as no
credential, so a token opens no web page and a session opens no git endpoint. No service
method grants `Anonymous` anything: `Authz` returns `Permission::None` for it on
every repo, public or not.

### 4.1a The public route list

`ch_core::access::PUBLIC_ROUTES` is the complete list of routes a signed-out
request may reach. It is a static table of method and path pattern, owned by
foundation, and every change to it is reviewed by Claude. The gate layer in the
`codehaven` binary wraps the whole router and rejects any other request from
`Anonymous` before a handler runs. Section 7.3 has the list.

### 4.2 Errors

`ch_core::Error` has `NotFound`, `Unauthorized`, `Forbidden`, `Validation(Vec<FieldError>)`,
`Conflict`, `RateLimited`, `Unavailable`, `Internal(anyhow::Error)`. One
`IntoResponse` impl maps them to HTTP and to the GitHub-style JSON body
`{ "message": ..., "errors": [{ "resource", "field", "code" }], "documentation_url": ... }`.
A private repository the caller cannot read is `NotFound`, never `Forbidden`, so
its existence does not leak.

### 4.3 Service traits

All are `#[async_trait]`, object safe, and take `&Actor` where permission matters.
The list below is the contract. Each implementing stream may add methods by
pull request against `ch-core`.

| Trait | Implemented in | Main methods |
| --- | --- | --- |
| `Authn` | ch-auth | `from_session(cookie)`, `from_basic(user, secret, client_ip)`, `from_bearer(token, client_ip)`, `from_ssh_key(fingerprint)` |
| `Invites` | ch-auth | `create`, `list`, `revoke`, `peek(token)`, `accept(token, NewAccount)` |
| `Audit` | ch-db | `record(tx, AuditEvent)`, `query(filter)`. Every stream records through it |
| `Authz` | ch-auth | `repo_permission(actor, repo)`, `org_role(actor, org)`, `require(actor, repo, Permission)` |
| `Accounts` | ch-auth | users, emails, orgs, teams, memberships, SSH keys, tokens |
| `Repos` | ch-git | `create`, `get_by_name`, `rename`, `transfer`, `fork`, `settings`, `collaborators`, `protections` |
| `GitService` | ch-git | `refs`, `resolve`, `tree`, `blob`, `log`, `commit`, `compare`, `diff`, `blame`, `grep`, `archive`, `commit_file_change`, `create_ref`, `delete_ref`, `test_merge`, `merge(MergeRequest) -> MergeOutcome` |
| `Issues`, `Pulls`, `Reviews`, `Releases`, `Search` | ch-collab | the obvious CRUD, timeline, `merge_gate(pr) -> GateReport` |
| `Notifications`, `Webhooks` | ch-notify | `inbox`, `mark_read`, `subscribe`; hook CRUD, `deliveries`, `redeliver` |
| `Checks` | ch-ci | `for_sha(repo, sha)`, `combined(repo, sha)`, `create_status(..)`, `required_satisfied(repo, sha, contexts)` |
| `Sessions` | ch-auth | `sign_in`, `complete_two_factor`, `sign_out`, `sessions`, `revoke_session`, `security_log`, two-factor setup, `peek_link` and `redeem_link`, `change_password`, `update_profile` |
| `Directory` | ch-auth | `list_users`, `orgs_of`, `org_members`, `teams`, `team_members`, `create_org`, `site_stats` |
| `Tracker` | ch-collab | labels, milestones, `set_milestone`, `set_assignees`, `review_comments`, `create_release`, `delete_release` |
| `Pipelines` | ch-ci | `workflows`, `jobs` (with steps), `runners`, `revoke_runner`, secrets, variables, `artifacts` |

The last four came from the web stream (`ch_core::web_contract`): they back pages that the first list had no
method for. `Repos::update` and `GitService::blame` came with them.
| `Ci` | ch-ci | `runs`, `jobs`, `log_range`, `log_stream`, `dispatch`, `cancel`, `rerun`, `artifacts`, `secrets` |
| `BlobStore` | ch-db (filesystem) | `put_stream`, `get_stream`, `delete`, keyed by sha256 |
| `EventBus` | ch-db | `emit(tx, DomainEvent)`, `consume(consumer_name, handler)` |

### 4.4 Domain events

`DomainEvent` is a single serde enum stored in the outbox table: `Push`,
`RefCreated`, `RefDeleted`, `RepoCreated`, `RepoDeleted`, `IssueOpened`,
`IssueClosed`, `IssueReopened`, `IssueEdited`, `IssueLabeled`, `IssueAssigned`,
`CommentCreated`, `PrOpened`, `PrSynchronized`, `PrClosed`, `PrMerged`,
`ReviewRequested`, `ReviewSubmitted`, `ReviewCommentCreated`, `ReleasePublished`, `WorkflowRunUpdated`,
`WorkflowJobUpdated`, `CheckRunUpdated`, `StatusCreated`, `MemberAdded`, `PackagePublished`.

Producers emit inside the same transaction as the change. Consumers each keep a
cursor in `event_cursors` and process in id order, so a crash never loses an
event and never double-sends one without a retry record.

Consumers:

| Consumer | Crate | Reacts to |
| --- | --- | --- |
| `pr-sync` | ch-collab | `Push`: updates PR head shas and review comment positions, marks PRs merged when the head lands in base outside the UI, closes issues referenced by `Fixes #n` on the default branch. `CheckRunUpdated`, `StatusCreated` and `ReviewSubmitted`: tries auto-merge |
| `ci-triggers` | ch-ci | `Push`, `PrOpened`, `PrSynchronized`, `PrClosed`, `RefCreated` |
| `notifications` | ch-notify | issue, PR, comment, review, mention and CI events |
| `webhooks` | ch-notify | everything that maps to a webhook event |
| `collab-defaults` | ch-collab | `RepoCreated`: gives the new repository its starting labels |

There is no `search-index` consumer. The search columns on `issues` and `repositories` are generated by Postgres, so
there is nothing for a consumer to keep in step. Cross-repository code search with `tantivy` would add one later.

### 4.5 Text references

`ch_core::text::refs(&str) -> Refs` extracts `@user`, `@org/team`, `#12`,
`owner/repo#12`, full and short commit shas, and closing keywords
(`fixes`, `closes`, `resolves` and their forms). It is pure and lives in
foundation so `ch-collab`, `ch-notify` and `ch-render` agree on what a mention is.

## 5. Storage

### 5.1 Git on disk

- Path: `data/git/<shard>/<repo_id>.git` with `shard = format!("{:02x}", repo_id % 256)`.
  Names live only in Postgres, so rename and transfer are a row update plus a
  redirect row.
- Created with `git init --bare --initial-branch=<default>`. `core.hooksPath`
  points to the server's shared hooks directory, so hooks cannot be edited per repo.
- Forks: a new bare repo with `objects/info/alternates` pointing to the root of
  the fork network. Deleting a root with live forks is blocked until it is
  re-parented by repacking one fork without alternates.
- Pull request refs in the base repo: `refs/pull/<n>/head` and `refs/pull/<n>/merge`.
- Housekeeping job: `git gc --auto` after pushes, `git gc` weekly per active repo.

### 5.2 Blobs

`BlobStore` stores files at `data/blobs/sha256/<aa>/<bb>/<hash>` and writes
through a temp file and rename. Reference counts live in Postgres. A daily job
deletes unreferenced blobs older than 24 hours. An S3 implementation can come
later behind the same trait.

### 5.3 Postgres schema

The foundation stream owns `crates/ch-db/migrations/` and writes the whole v1
schema in M0, so no stream waits on a table. After M0, a schema change is a
small pull request to foundation with the migration and the `.sqlx` update.

All ids are `BIGINT GENERATED ALWAYS AS IDENTITY` (the API exposes integers like
GitHub's). Names that people type are `citext`. Times are `timestamptz`.
Extensions: `citext`, `pg_trgm`.

| Group | Tables |
| --- | --- |
| Accounts | `users` (`kind` user, org or bot; username, display name, password hash, is_admin, suspended_at, totp secret encrypted, invited_by, can_run_ci), `user_emails`, `sessions` (hash, user, created, last seen, absolute expiry, ip, user agent), `oidc_identities`, `personal_access_tokens` (hash, prefix, scopes, expiry, last used), `ssh_keys` (fingerprint unique), `totp_recovery_codes`, `user_settings` (theme, notification prefs) |
| Access | `invites` (token hash, created_by, optional email, grants_admin, grants_ci, optional org and team to join, expires_at, used_at, used_by, revoked_at), `one_time_links` (hash, user, purpose `set_password` or `reset_password`, expires_at, used_at), `login_failures` (account or ip key, count, window start, locked_until) |
| Orgs | `org_members` (role owner or member), `teams`, `team_members`, `team_repos` (permission) |
| Repos | `repositories` (owner, name, visibility, default_branch, fork parent, archived, empty, flags, next_number), `repo_redirects`, `repo_collaborators`, `branch_protections`, `stars`, `watches`, `deploy_keys` |
| Issues | `issues` (also the row for each PR, `is_pull`), `labels`, `issue_labels`, `milestones`, `issue_assignees`, `issue_comments`, `reactions`, `timeline_events`, `issue_subscriptions` |
| Pull requests | `pull_requests` (head repo and ref, head sha, base ref, merge base, mergeable state, merge commit, merged by, draft), `review_requests`, `reviews`, `review_threads`, `review_comments` (path, side, line, start_line, original_line, commit, diff_hunk, outdated) |
| Delivery | `notifications`, `webhooks` (scope repo or org, url, secret encrypted, events, active), `webhook_deliveries` |
| Events and jobs | `outbox_events`, `event_cursors`, `background_jobs` |
| Releases | `releases`, `release_assets` |
| CI | `workflows`, `workflow_runs`, `workflow_jobs`, `workflow_steps`, `runners`, `runner_registration_tokens`, `secrets`, `variables`, `artifacts`, `caches`, `check_runs`, `commit_statuses`, `schedules` |
| Registry | `packages`, `oci_blobs`, `oci_manifests`, `oci_tags`, `oci_uploads` |
| Ops | `audit_events` (at, actor, client ip, user agent, action, target type and id, outcome, details jsonb), `blobs` (refcounts) |

Search uses a generated `tsvector` on `issues(title, body)` and
`repositories(name, description, topics)`, with GIN indexes, plus `pg_trgm`
indexes on usernames and repo names for the quick finder.

### 5.4 Secrets at rest

`data/secrets/master.key` is 32 random bytes, created at first start with mode
0600. CI secrets, webhook secrets, TOTP secrets and OIDC client state are sealed
with AES-256-GCM (`aes-gcm`), with a random nonce per value and the row id as
associated data. Tokens, invite tokens, one-time links and session ids are
stored only as SHA-256 hashes. Passwords use Argon2id (`argon2`, 64 MiB memory,
3 passes, 1 lane, 16 byte salt), and the parameters are stored in the PHC string
so they can be raised later with a rehash at the next sign-in.

## 6. Git

### 6.1 Reads and writes

- Read paths (list refs, resolve a revision, list a tree, read a blob, walk
  history with path filtering, read commit metadata, blame, grep, archive) run the git
  plumbing through the same wrapper. The design called for `gix` on the hot paths; the
  first version uses plumbing because it is git's own behaviour, needs no second
  implementation to keep in step, and is fast enough at this scale. Each read is one
  function in `ch-git/src/read.rs`, so replacing one with `gix` changes no caller.
- Everything that writes, and everything where git's own algorithm is the
  standard, shells out to `git` through one wrapper, `GitCommand`, which sets a
  clean environment (`GIT_CONFIG_NOSYSTEM=1`, `HOME` to an empty dir,
  `GIT_TERMINAL_PROMPT=0`), enforces a timeout, streams stdout, and never passes
  user input as an option (`--end-of-options` before revisions and paths).
- Results that are costly and immutable (highlighted blobs, diffs between two
  shas, blame at a sha) are cached in-process with `moka`, keyed by sha.

### 6.2 Smart HTTP

```
GET  /{owner}/{repo}(.git)/info/refs?service=git-upload-pack|git-receive-pack
POST /{owner}/{repo}(.git)/git-upload-pack
POST /{owner}/{repo}(.git)/git-receive-pack
```

The handler authenticates first, always. There is no anonymous clone, even of a
repo marked public. With no credentials it answers `401` with
`WWW-Authenticate: Basic realm="Codehaven"`, the same response whether the repo
exists or not, so names cannot be probed. Basic auth takes a personal access
token as the password, or the account password if the account has no two-factor.
Failures count toward the same rate limits and lockout as the sign-in form. Then
it checks `Authz` and pipes the request body into
`git <service> --stateless-rpc` and streams the response. It honours
`Content-Encoding: gzip` and passes `Git-Protocol` through as `GIT_PROTOCOL`, so
protocol v2 works. Bodies are never buffered whole. An account with TOTP turned
on must use a token for git over HTTP, with an error message that says so.

### 6.3 SSH

`ch-ssh` runs a `russh` server on port 2222 with an ed25519 host key stored in
`data/secrets/ssh_host_ed25519`. Public key auth only. The key fingerprint maps
to a user (or a deploy key bound to one repo). Accepted exec commands:
`git-upload-pack '<path>'`, `git-receive-pack '<path>'`, `git-upload-archive '<path>'`.
Anything else, including a shell, gets a short message naming the user and
closes. The process is the same `GitCommand` path as HTTP, so hooks and
permissions behave identically.

### 6.4 Hooks and the push pipeline

Every git subprocess that can update refs gets `CODEHAVEN_PUSH_ID` (random,
registered in memory with the actor and repo), `CODEHAVEN_REPO_ID` and
`CODEHAVEN_SOCKET`. The shared hooks are two-line shell scripts that exec
`codehaven hook pre-receive` and `codehaven hook post-receive`. Those send the
ref updates over a Unix socket to the server, which:

- `pre-receive`: checks write permission per ref and branch protection (no
  direct pushes to protected branches unless allowed, no force pushes, no
  deletions) and returns a readable rejection that git prints to the user.
- `post-receive`: in one transaction updates `pushed_at`, sets the default
  branch on the first push, clears `is_empty`, and emits `Push`, `RefCreated`,
  `RefDeleted`. Consumers do the rest.

Refs changed from the UI or API (merges, web edits, branch creation) go through
the same pipeline function, so protection checks and events are identical no
matter where a change came from.

### 6.5 Merging

All merges run in the bare repo with no working tree:

- **Merge commit**: `git merge-tree --write-tree base head` gives a tree or a
  conflict list, then `git commit-tree -p base -p head`, then
  `git update-ref <ref> <new> <expected-old>`.
- **Squash**: same tree, one parent, message built from the PR title, number and
  commit list. Author is the PR author, committer is the merger.
- **Rebase**: for each commit in `base..head`, `git merge-tree --write-tree
  --merge-base=<parent> <onto> <commit>`, then `commit-tree` keeping the
  original author and message. Stops on the first conflict and reports it.

The expected-old value on `update-ref` makes a concurrent push fail the merge
cleanly instead of losing commits. Mergeability is recomputed in a background
job whenever head or base moves, and stored as `clean`, `conflict`, `behind`,
`blocked` or `unknown`.

### 6.6 Diffs and blame

- `git diff --no-color --find-renames -z --raw` and `--patch` between two shas,
  parsed into a structured `Diff { files: [FileDiff { status, old_path, new_path,
  binary, hunks: [Hunk { lines: [Line { kind, old_no, new_no, text }] }] }] }`.
  Files over 20,000 changed lines or 1 MB are collapsed with a "Load diff" link.
- `git blame --porcelain` parsed into line ranges grouped by commit.
- Compare and PR diffs are three-dot (from the merge base), like people expect.

### 6.7 Web edits

`commit_file_change` writes blobs with `git hash-object -w --stdin`, builds the
tree with a throwaway index (`GIT_INDEX_FILE`, `read-tree`, `update-index`,
`write-tree`), then `commit-tree` and a compare-and-swap `update-ref`. It
supports create, edit, rename and delete in one commit, and "commit to a new
branch and open a pull request".

### 6.8 Other

- Archives: `git archive --format=zip|tar.gz` streamed for any ref.
- In-repo code search: `git grep -n -I --max-count=20` on the default branch
  with a 5 second timeout.
- Import: one-shot `git clone --mirror` from a URL into a new repo (M4).

## 7. Accounts, auth and permissions

### 7.1 Ways in

| Method | Used by | Notes |
| --- | --- | --- |
| Username and password | web, git HTTP (accounts without two-factor) | Argon2id. Rate limits and lockout in section 7.6 |
| TOTP | web | `totp-rs`, 10 recovery codes. Optional for everyone, required before a site admin can open the admin pages |
| OIDC | web | `openidconnect`. Off by default. When configured it can only sign in an account that already exists and has linked that identity, or finish an invite. It never creates an account on its own |
| Session cookie | web | `__Host-ch_session`: 256-bit random id, stored hashed, `Secure`, `HttpOnly`, `SameSite=Lax`, `Path=/`. 7 days idle, 30 days absolute. Section 7.6 |
| Personal access token | API, git HTTP, CLI, registry | Format `chp_` plus 40 base62 characters, so secret scanners can match it. Scopes below. Expiry required, 90 days by default, 1 year at most |
| SSH key | git SSH | ed25519, ECDSA, RSA of 3072 bits or more. Fingerprint SHA-256 |
| Deploy key | git SSH | Bound to one repo, read-only unless marked write |
| Job token | CI jobs | Minted per job, valid until the job ends, scoped to its repo |
| Runner credential | runners | Issued at registration with an admin-issued token, stored hashed |

There is no proxy header login. Codehaven ignores `Remote-User` and every other
identity header, wherever the request comes from.

Token scopes: `repo`, `repo:read`, `user`, `read:org`, `admin:org`,
`write:packages`, `read:packages`, `workflow`, `admin:repo_hook`, `notifications`.
Names follow GitHub's where they mean the same thing. No token scope grants site
admin powers: inviting people, issuing runner tokens and managing users need a
signed-in browser session with two-factor.

### 7.2 Permission model

`repo_permission(actor, repo)` is the maximum of:

- site admin: `Admin`
- owner of a personal repo: `Admin`
- org owner of the repo's org: `Admin`
- direct collaborator permission
- each team permission for teams the user belongs to
- the org's base permission for members (default `Read`)
- `Read` if the repo is public and the actor is a signed-in user. On this
  instance "public" means "every account on this Codehaven", and the UI says so.
  `Anonymous` gets `None` everywhere

then capped by the token's scopes (a `repo:read` token never writes) and by
`archived` (writes fail with a clear message). Job tokens get `Read` on contents
plus write on statuses, checks, artifacts and caches for their own repo. Fork
pull request jobs get `Read` only and no secrets.

What each level allows:

| Level | Adds |
| --- | --- |
| Read | clone, browse, open issues and PRs, comment |
| Triage | labels, assignees, milestones, close and reopen issues |
| Write | push, merge PRs (subject to protection), edit files, manage releases, re-run workflows |
| Maintain | repo settings except danger zone, webhooks, protections |
| Admin | collaborators, teams, secrets, rename, transfer, delete |

Site admin is separate from repo admin. Only site admins invite people, issue
runner registration tokens, suspend accounts, and read the audit log.

### 7.3 What a signed-out visitor can reach

The instance is private as a whole. The gate layer (section 4.1a) lets a request
with no valid session, token or runner credential reach only these routes:

| Route | Why |
| --- | --- |
| `GET /login`, `POST /login`, `GET/POST /login/two-factor` | Signing in |
| `GET /invite/{token}`, `POST /invite/{token}` | Accepting an invite (section 7.4) |
| `GET /password/{token}`, `POST /password/{token}` | One-time set-password and reset links (section 7.5) |
| `GET /login/oidc/{provider}`, `GET /login/oidc/{provider}/callback` | Only when a provider is configured |
| `GET /static/*`, `GET /favicon.svg` | Styles, scripts, icons. Nothing user-generated |
| `GET /robots.txt` | `Disallow: /` |
| `GET /healthz` | Returns `ok` and nothing else |
| `POST /api/runner/v1/register` | Rejects anything without a valid admin-issued registration token |

Everything else, for a signed-out request:

- Web pages: `303` to `/login?next=<path>`, the same for paths that exist and
  paths that do not. `next` only accepts a local path.
- `/api/v3/*`: `401` with `{"message": "Requires authentication"}`. No `/meta`,
  no `/version`, no rate limit numbers.
- git smart HTTP: `401` with a Basic challenge, identical for every repo name.
- `/v2/` registry: `401` with a Bearer challenge. No anonymous pulls.
- `/api/runner/v1/*` other than register: `401` unless a runner credential or
  job token is present.
- `/readyz` and `/metrics`: served only on the internal listener (section 14),
  never through the proxy.

The sign-in page shows the instance name and the sign-in form. It does not list
users, repos or counts. With zero accounts it says "No accounts yet. The server
administrator creates the first one." There is no sign-up link and no sign-up
route.

A test in `crates/codehaven/tests/anonymous_surface.rs` walks every route the
router has registered, sends each one a signed-out request, and fails if
anything outside the table above returns anything but the rejection for its
kind. A new route is private the moment it is added, and stays private unless
someone edits `PUBLIC_ROUTES` on purpose.

### 7.4 Invites

There is no open sign-up and no setting to turn one on. Every account except the
first comes from an invite.

- **Who can invite.** Site admins, from Site admin > Invites or with
  `codehaven admin invite create` on the host. Org owners can invite people straight
  into their org only if a site admin turns on `invites.org_owners_can_invite`
  (off by default).
- **The link.** `https://code.example.com/invite/chi_<32 base62>`, about
  190 bits of randomness. Only the SHA-256 hash is stored. The admin copies the
  link and sends it however they like. If SMTP is set up, Codehaven can email it.
- **Options.** An email address the account must use (optional), whether it
  grants site admin (off by default, and only an admin with two-factor can set
  it), an org and team to join, and an expiry from 1 hour to 30 days, 7 days by
  default. A note to self, shown only in the admin list.
- **Single use.** Accepting runs one transaction: `UPDATE invites SET used_at =
  now(), used_by = $1 WHERE token_hash = $2 AND used_at IS NULL AND revoked_at
  IS NULL AND expires_at > now() RETURNING ...`, then creates the user. Two
  people racing the same link get one account between them.
- **Bad links.** Expired, used, revoked and made-up tokens all get the same page:
  "This invite link doesn't work any more. Ask the person who sent it for a new
  one." The page is rate limited per IP like the sign-in form.
- **The accept page.** Shows who sent the invite, then asks for a username,
  email (locked if the invite named one) and a password of at least 12
  characters that scores 3 or more on the zxcvbn guessability estimate (which embeds
  ranked lists of common passwords, names and keyboard patterns and recognises
  dates, repeats and substitutions) and does not contain the username or the
  email name. After
  that it offers two-factor setup, which can be skipped.
- **Admin list.** Pending, used (by whom, when) and expired invites, with revoke.
  Every create, revoke and accept is in the audit log.

### 7.5 The first admin

A new instance has no accounts and no way to make one from the web. The first
admin is made on the host by someone who already has root there:

```
sudo bash deploy/prod/first-admin.sh <username> <email>
```

The script refuses unless the instance has no accounts at all, then runs
`codehaven admin create-user --admin --can-run-ci` in the server container.
The command creates the account with no password and prints a one-time link,
`https://code.example.com/password/<token>`, valid for 1 hour. Opening it
sets the password, then goes straight to two-factor setup, which an admin needs
before the admin pages open. The password never passes through a shell, its
history or the process list. `--password-stdin` exists for the e2e harness.

The same pattern covers recovery: `codehaven admin reset-password <user>` prints
a fresh one-time link and signs that account out everywhere, and
`codehaven admin unlock <user>` clears a lockout. A self-service reset by email
exists only when SMTP is configured, and its form says the same thing whether or
not the address has an account.

### 7.6 Hardening

**Passwords.** Argon2id as in section 5.4. At most 4 hashes run at once (a
`tokio::sync::Semaphore`), so a flood of sign-in attempts cannot eat the CPU.
When the username does not exist, the server still verifies against a fixed
dummy hash, so timing does not reveal which usernames exist. The error is always
"That username and password don't match."

**Rate limits**, keyed by the real client IP (section 2.4) with `tower_governor`:

| What | Limit |
| --- | --- |
| Sign-in, two-factor, invite and password-link pages, per IP | 10 a minute, burst 5 |
| Failed Basic or token auth on API, git and registry, per IP | 30 a minute |
| All authenticated API calls, per token or session | 5,000 an hour, with `X-RateLimit-*` headers |
| Anything signed out, per IP | 120 a minute |

**Lockout.** 10 failed passwords for one account from one address (an IPv6 /64 counts
as one) within 15 minutes lock password sign-in for that account from that address for
15 minutes, never for the account as a whole, so a stranger who knows a username cannot
lock the owner out (ch-rx2). Wrong two-factor codes also count across all addresses.
The lock doubles on each repeat, up to
24 hours. Valid tokens and SSH keys keep working during a lockout, so an
attacker guessing at a user's password cannot cut off that user's git access. The
account owner sees the lockout and its source IPs at the next sign-in. Every
lockout is audited, and `admin unlock` clears it. An attempt is counted before
the password hash runs, under a row lock, so a burst of simultaneous guesses shares the
same budget of 10 as guesses made one at a time (the address is reserved first, so a
locked address cannot keep using up an account's budget).

**Sessions.** A new session id on every sign-in and on every change of privilege
(two-factor on or off, password change, admin granted). A password change signs
out every other session. Settings > Sessions lists each session with its IP,
browser and last use, with a sign-out button for each. Suspending an account
ends its sessions and disables its tokens at once.

**CSRF.** Every form and htmx request carries a per-session token, in a hidden
field or the `X-CSRF-Token` header set once on `<body hx-headers>`. The sign-in
and invite forms, which have no session yet, use a signed double-submit cookie
`__Host-ch_csrf`. As a second check, every unsafe request with a cookie must
have an `Origin` (or `Referer`) equal to `server.external_url`. Requests with a
token in `Authorization` skip CSRF because cookies are not used.

**Security headers**, set by Codehaven itself on every response so the e2e test
can check them:

```
Strict-Transport-Security: max-age=31536000
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self';
  img-src 'self' data:; font-src 'self'; connect-src 'self'; form-action 'self';
  frame-ancestors 'none'; base-uri 'none'
X-Frame-Options: DENY
X-Content-Type-Options: nosniff
Referrer-Policy: same-origin
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Resource-Policy: same-origin
Permissions-Policy: camera=(), microphone=(), geolocation=()
X-Robots-Tag: noindex, nofollow
```

`img-src 'self'` means images in markdown that point elsewhere render as links,
so reading an issue never leaks the reader's IP to a third party. Raw files keep
their stricter `sandbox` policy (section 12.6). HSTS is sent only when
`external_url` is `https`.

**Runners.** A runner registers only with a token a site admin issued in Site
admin > Runners or with `codehaven admin runner-token`. The token is single use
and lasts 1 hour. Each runner then has its own credential, stored hashed, which
an admin can revoke. Job tokens die when the job ends.

**Other edges.** Webhooks refuse private and loopback targets (section 8.5).
Uploads have size limits per kind (release asset 2 GB, artifact 1 GB, avatar
1 MB). Error pages never include stack traces or SQL. `/healthz` says `ok` and
nothing more.

### 7.7 Audit log

`audit_events` records, with time, actor, client IP and user agent:

- sign-in success and failure, two-factor failure, lockout and unlock, sign-out
- password set, changed or reset, two-factor turned on or off, recovery code used
- invite created, revoked and accepted, user created, suspended or made admin
- session revoked, token created or deleted, SSH key added or removed
- runner token issued, runner registered or removed
- secret created, changed or deleted (the name, never the value)
- repo visibility changed, transferred or deleted, branch protection changed

Writes go through `Audit::record` in the same transaction as the change.
Site admin > Audit log shows them with filters by user, action and IP, and the
user's own Settings > Security shows their sign-in history. Kept for 1 year.

## 8. Collaboration

### 8.1 Issues

Issues and PRs share one number sequence per repo (`repositories.next_number`,
taken with `UPDATE ... RETURNING`). Features: markdown body, labels with colors,
milestones with due dates and progress, multiple assignees, reactions, lock,
pin, close with reason (completed or not planned), cross references in the
timeline, `@mentions`, task list progress, templates from
`.codehaven/ISSUE_TEMPLATE/*.md` (and `.github/`), and saved filters with a small
query language (`is:open label:bug assignee:@me sort:updated-desc`).

### 8.2 Pull requests

- Create from a branch or a fork branch, draft or ready, with templates.
- Tabs: conversation, commits, checks, files changed.
- Files view: unified or split diff, per-file collapse, "viewed" checkboxes,
  whitespace toggle, jump list.
- Review: start a review, add inline comments on one line or a range on either
  side, suggest a change (a fenced `suggestion` block that renders as a diff and
  can be applied as a commit), then submit as comment, approve or request
  changes. Threads can be resolved. Comments on lines that later change are
  marked outdated and kept with their original diff hunk.
- Review requests to users or teams. `CODEOWNERS` (in `.codehaven/`, `.github/`
  or root) requests owners automatically and can be required by protection.
- Merge box: merge, squash or rebase (each can be turned off per repo), delete
  branch after merge, auto-merge when the gate passes.

### 8.3 The merge gate

`merge_gate(pr)` returns every reason a merge is blocked, so the UI shows a
checklist instead of a single "no":

- mergeable state is `clean`
- required approvals met, counting only the latest review per reviewer, and
  dropping approvals older than the head commit when "dismiss stale" is on
- no outstanding "changes requested"
- required checks pass on the head sha (`Checks::required_satisfied`)
- branch up to date with base when "strict" is on
- code owner approval when required
- all review threads resolved when required

### 8.4 Notifications

Reasons: author, assigned, mentioned, team mentioned, review requested,
commented, state change, subscribed, CI finished (only for runs you triggered).
Inbox with filters, mark read, mark done, unsubscribe from a thread, watch
levels per repo (all activity, participating only, ignore). The unread badge
refreshes with `hx-trigger="every 60s"`. Email through SMTP (`lettre`) is off
until `mail.smtp` is configured.

### 8.5 Webhooks

Repo and org hooks. Events: `ping`, `push`, `create`, `delete`, `issues`,
`issue_comment`, `pull_request`, `pull_request_review`,
`pull_request_review_comment`, `release`, `workflow_run`, `workflow_job`,
`check_run`, `status`, `member`, `package`. Payloads use GitHub's field names for
the fields common tools read. Each delivery is signed with HMAC-SHA256 in
`X-Hub-Signature-256` (the header name tools already verify) and carries
`X-Codehaven-Event` plus `X-GitHub-Event` and a delivery GUID. Retries at 1 min,
5 min, 30 min, 2 h, 6 h. The settings page lists recent deliveries with request,
response and a redeliver button. Outbound requests refuse private and loopback
addresses unless `webhooks.allow_private` is set. They also refuse the host's own
public addresses (`webhooks.blocked_ips`, filled from `deploy/prod/host-ips.sh --json`
by `up.sh`), because services the firewall hides from the internet still answer there.

### 8.6 Releases

A release is a tag plus notes and assets. "Generate notes" lists PRs merged since
the previous tag, grouped by label. Assets go to the blob store. Source zip and
tarball links are streamed from `git archive`. Publishing emits
`ReleasePublished`, which can trigger workflows (`on: release`).

### 8.7 Search

One search box, scoped to the current repo when inside one. Tabs for repos,
issues, pull requests, users and code. Issues and repos use Postgres full-text
search ranked by `ts_rank` then recency. Code search is `git grep` on the repo's
default branch. Cross-repo code search with `tantivy` is a later item.

## 9. CI

### 9.1 Workflow files

Read from `.codehaven/workflows/*.yml|yaml` and, unless the repo turns it off,
`.github/workflows/*.yml|yaml`. If a file name exists in both, the
`.codehaven` one wins. The file is read from the commit being built: the pushed
sha, the PR head, the default branch for `schedule`, the chosen ref for manual
runs.

Supported syntax, a deliberate subset of GitHub Actions:

```yaml
name: ci
on:
  push:          { branches: [main, 'release/**'], tags: ['v*'], paths: ['src/**'], paths-ignore: [...] }
  pull_request:  { types: [opened, synchronize, reopened], branches: [main] }
  workflow_dispatch:
    inputs: { level: { type: choice, options: [quick, full], default: quick } }
  schedule:      [{ cron: '0 3 * * *' }]
  release:       { types: [published] }
env: { RUST_LOG: info }
defaults: { run: { shell: bash, working-directory: . } }
concurrency: { group: ci-${{ github.ref }}, cancel-in-progress: true }
jobs:
  test:
    runs-on: ubuntu-latest
    container: { image: rust:1-trixie, env: {}, options: '' }
    strategy:
      matrix: { os: [a, b], include: [...], exclude: [...] }
      fail-fast: true
      max-parallel: 2
    needs: [lint]
    if: github.event_name != 'schedule'
    timeout-minutes: 60
    continue-on-error: false
    env: { ... }
    outputs: { ver: ${{ steps.v.outputs.ver }} }
    steps:
      - uses: actions/checkout@v4          # mapped to the built-in checkout
      - id: v
        run: echo "ver=1.2" >> "$GITHUB_OUTPUT"
      - uses: docker://alpine:3.20
        with: { args: echo hi }
      - uses: actions/upload-artifact@v4
        with: { name: report, path: target/report }
```

Not supported in v1, each with a clear validation error that names the line:
JavaScript actions from a marketplace, `services:`, reusable workflows
(`workflow_call`), `environment:` approvals, `pull_request_target` (it would run a
fork's code with the base repository's secrets), YAML anchors and aliases, and a
matrix axis written as an expression (the whole matrix may be one, see below).
Composite actions from the same repo (`uses: ./path`) are a later item.

`strategy.matrix` may be `${{ fromJSON(needs.setup.outputs.m) }}`. Such a job is planned
as one placeholder and expanded into its cells when the jobs it needs have finished.

### 9.2 ch-workflow (pure)

- Parses YAML with `serde_yaml_ng` into typed structs with source spans, so
  errors read "line 14: `runs-on` is required".
- Validates and reports all errors at once. The workflows page shows them for
  every file on the default branch.
- Expression engine for `${{ }}`: literals, property access, index, `!`, `&&`,
  `||`, `==`, `!=`, `<`, `>`, and functions `contains`, `startsWith`,
  `endsWith`, `format`, `join`, `toJSON`, `fromJSON`, `success`, `failure`,
  `always`, `cancelled`, `hashFiles` (evaluated by the runner). Contexts:
  `github` (also exposed as `codehaven`), `env`, `vars`, `secrets`, `inputs`,
  `matrix`, `strategy`, `needs`, `steps`, `job`, `runner`.
- Matrix expansion with include and exclude, capped at 256 jobs per run.
- Trigger matching with glob filters (`globset`).

The crate has no IO and no async. The server and the runner both use it, so a
step's `if:` means the same thing in both. Until this crate has an engine, `haven-runner`
carries its own copy (`docs/runner.md`, "Expression engine"), to be replaced by an adapter.

### 9.3 Server side (ch-ci)

- `ci-triggers` consumes events, reads workflow files at the right sha, matches
  triggers and creates `workflow_runs` and one `workflow_jobs` row per job and
  matrix cell, plus a queued check run for each.
- `schedule` worker: `croner` parses cron, `schedules.next_run_at` is
  recomputed after each fire, at most one fire per schedule per minute, all
  schedules in UTC. Schedules only come from the default branch.
- Manual runs: a "Run workflow" form built from `workflow_dispatch.inputs`, and
  `POST /api/v3/repos/{o}/{r}/actions/workflows/{id}/dispatches`.
- Dependencies: a job moves from `waiting` to `queued` when every `needs` job
  has finished and its `if:` (default `success()`) is true, otherwise it is
  `skipped`. Outputs flow into the `needs` context.
- Fork pull requests from first-time contributors wait for a maintainer to press
  "Approve and run".
- Concurrency groups cancel or queue per `concurrency`.
- Re-run all jobs or failed jobs. Cancel a run.

### 9.4 Runner protocol (`/api/runner/v1`)

All JSON, over HTTP inside the compose network, HTTPS when the runner is
remote. Types live in `ch-api-types::runner`.

| Call | What it does |
| --- | --- |
| `POST /register` with a registration token | Returns runner id and credential. The token is single use and expires in 1 hour |
| `POST /jobs/request` | Long poll up to 30 s. The server picks the oldest queued job whose `runs-on` labels are a subset of the runner's labels, with `FOR UPDATE SKIP LOCKED`, sets a 2 minute lease, and returns the job payload |
| `POST /jobs/{id}/heartbeat` | Every 30 s. Extends the lease. The response carries `cancel: true` when the run was cancelled |
| `POST /jobs/{id}/logs` | Batches of `{ step, seq, lines }`. `seq` makes retries idempotent: a batch the server already has is acknowledged and dropped, a batch from the future is refused with `409` and `{ next_seq }` |
| `POST /jobs/{id}/steps/{n}` | Step started or finished with conclusion and outputs |
| `POST /jobs/{id}/complete` | Conclusion, job outputs |
| `PUT /jobs/{id}/artifacts/{name}?retention_days=N` | Streams a zip into the blob store. Job token, not for fork pull request jobs. A second upload of the same name replaces the first |
| `GET /jobs/{id}/artifacts`, `GET /jobs/{id}/artifacts/{name}` | For `download-artifact` within the run. Job token. The list is an `ArtifactsResponse` |
| `POST /cache/lookup`, `PUT /cache/{key}`, `GET /cache/{id}` | Cache restore and save |

Authentication: `register` needs only the registration token in its body. Every other call
sends `Authorization: Bearer <runner credential>`, except artifacts and cache, which send the
job's own token from the payload. The runner follows the choices in `docs/runner.md` (log `seq`
is per job, `retention_days` rides on the artifact upload as a query parameter, cache saves carry
`?version=`).

The job payload contains the resolved job (runs-on, container image, env,
matrix values, needs outputs), the steps with their expressions left
unevaluated, the secrets the job may see, the job token, and the clone URL.

If a lease expires, the job fails with "The runner stopped responding" and can
be re-run.

### 9.5 Runner (haven-runner)

```
haven-runner register --url http://server:3200 --token <reg> --name runner-1 --labels ubuntu-latest,linux,x64
haven-runner run
```

The credential is saved to `.runner` with mode 0600. For each job the runner:

1. Creates `/work/<job>` and runs the built-in checkout there (`git` with the
   job token, depth 1 by default, `fetch-depth` honoured).
2. Pulls the job image. `runs-on` labels map to images in the runner config,
   for example `ubuntu-latest = codehaven-job:trixie` (our image: Debian trixie
   with git, curl, build-essential, ca-certificates, Node and Python). A job's
   `container.image` overrides it.
3. Starts one long-lived container per job with `/work/<job>` bound at the same
   path and runs each `run:` step with `docker exec`, using the step's shell
   (`bash -e -o pipefail` by default).
4. Evaluates each step's `if:` and `${{ }}` with `ch-workflow`, sets `CI=true`
   and every `GITHUB_*` variable people's scripts expect, plus the matching
   `CODEHAVEN_*` names, and honours `$GITHUB_OUTPUT`, `$GITHUB_ENV`,
   `$GITHUB_PATH` and `$GITHUB_STEP_SUMMARY` files.
5. Masks every secret value in log lines (each line of a multi-line secret too)
   before sending. `::add-mask::` works.
6. Streams logs in batches every 500 ms or 64 KB.
7. Removes the container and `/work/<job>` at the end, whatever happened.

Beyond `register` and `run`, `haven-runner check` reports on the credential, the settings
file, Docker, the work directory, disk and the server. Settings live in a TOML file
(`deploy/runner.example.toml`).

Built-in actions, matched by name with or without an owner prefix and version
(`checkout`, `actions/checkout@v4`, `codehaven/checkout@v1` are the same):

| Action | Inputs |
| --- | --- |
| `checkout` | `ref`, `fetch-depth`, `path`, `submodules` |
| `cache` | `key`, `restore-keys`, `path` (save happens after the job if no hit) |
| `upload-artifact` | `name`, `path` (globs), `retention-days`, `if-no-files-found` |
| `download-artifact` | `name`, `path` |
| `docker://image` | `args`, `entrypoint` |

### 9.6 Secrets and variables

Repo and org scope. Values are write-only in the UI and API. Names are
upper-case letters, digits and underscores. Secrets are only sent to the runner
for jobs that reference them, never to fork PR jobs. `vars` are plain settings.

### 9.7 Logs

The server appends batches to `data/logs/<run>/<job>.log` with step markers,
and records each step's byte range in `workflow_steps`. Live viewers subscribe
through SSE at `GET /api/v3/repos/{o}/{r}/actions/jobs/{job}/logs/stream`, which a
signed-in browser session may call. The page at `/{o}/{r}/actions/runs/{run}/jobs/{job}`
opens it with `EventSource`. A `tokio::sync::broadcast` channel per job only says "more
is available": the file is the truth. Each event's id is the byte offset reached, so a
viewer that reconnects (`Last-Event-ID`, or `?from=`) resumes exactly. A viewer that
connects late first gets the file contents, then the stream. `GET .../jobs/{job}/logs`
returns the whole log as plain text. ANSI colors become `<span>` classes in
`ch-render`. Logs are kept 90 days by default.

### 9.8 Checks and statuses

Each job owns a check run named `<workflow> / <job>` (matrix values in brackets)
on the run's head sha, updated as the job moves. External tools post commit
statuses through `POST /api/v3/repos/{o}/{r}/statuses/{sha}`. The commit list,
branch list, PR header and merge gate show both: a teal tick when all pass, a red
cross on any failure, an amber dot while anything runs.

## 10. Container registry

OCI Distribution Spec 1.1 at `/v2/` on the same host, because Docker clients
require `/v2/` at the root. `docker login code.example.com` with a
username and a token, answered with a Bearer challenge to `/v2/token`. Every
pull and push needs a token: there are no anonymous pulls, even for an image
linked to a public repo. Images are named
`<owner>/<name>` and can be linked to a repo, inheriting its permissions.
The token endpoint returns the credential the client just sent as Basic (an account name and
a personal access token) as the Bearer token it will use from then on, so nothing new is minted
and what a caller may do is decided on every request. A token needs the `read:packages` or
`write:packages` scope. An image with no repository belongs to its owner (an organization's
members pull, its owners push); one linked to a repository, which happens on first push when a
repository of the same name exists and the pusher can write to it, follows that repository's
permissions. Layers live in the blob store, deduplicated across images. Supports chunked and
monolithic uploads, cross-repo mounts, tag listing, manifest deletes and a
garbage collection job. A packages tab on profiles and repos lists images and
tags. Conformance is checked with the OCI distribution conformance suite in a
throwaway container. The reverse proxy terminates TLS, so Docker clients need
no insecure registry setting.

## 11. REST API

Base path `/api/v3`, the same as GitHub Enterprise Server, so tools with a
configurable GHES URL work. The goal is the common endpoints with matching
paths, methods, field names and status codes, not the whole surface.

- Auth: `Authorization: token <t>`, `Authorization: Bearer <t>`, or Basic.
- Pagination: `page` and `per_page` (max 100) with a `Link` header.
- `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` headers.
- Any `Accept` with JSON is fine. `X-GitHub-Api-Version` is accepted and ignored.
- Every resource has `url` and `html_url` pointing at this instance.
- An OpenAPI document is generated with `utoipa` at `/api/v3/openapi.json`.

Endpoint groups for v1, with their owner crate:

| Group | Paths | Crate |
| --- | --- | --- |
| Meta | `/`, `/meta`, `/rate_limit`, `/version` | codehaven |
| Users | `/user`, `/users/{u}`, `/user/keys`, `/user/emails`, `/user/repos`, `/user/orgs` | ch-auth, ch-git |
| Orgs and teams | `/orgs/{o}`, `/orgs/{o}/members`, `/orgs/{o}/teams`, `/orgs/{o}/repos` | ch-auth, ch-git |
| Repos | `/repos/{o}/{r}`, `/collaborators`, `/forks`, `/topics`, `/branches`, `/branches/{b}/protection`, `/tags`, `/commits`, `/commits/{sha}`, `/compare/{a}...{b}`, `/contents/{path}`, `/git/refs`, `/git/commits`, `/git/trees`, `/git/blobs`, `/readme`, `/zipball`, `/tarball` | ch-git |
| Issues | `/repos/{o}/{r}/issues`, `/issues/{n}`, `/comments`, `/labels`, `/milestones`, `/assignees`, `/events`, `/timeline`, `/reactions` | ch-collab |
| Pulls | `/repos/{o}/{r}/pulls`, `/pulls/{n}`, `/files`, `/commits`, `/merge`, `/reviews`, `/comments`, `/requested_reviewers` | ch-collab |
| Search | `/search/repositories`, `/search/issues`, `/search/users`, `/search/code` | ch-collab |
| Releases | `/repos/{o}/{r}/releases`, `/releases/latest`, `/releases/tags/{t}`, `/assets` | ch-collab |
| Statuses and checks | `/statuses/{sha}`, `/commits/{ref}/status`, `/commits/{ref}/statuses`, `/commits/{ref}/check-runs` | ch-ci |
| Actions | `/actions/workflows`, `/dispatches`, `/actions/runs`, `/runs/{id}/jobs`, `/jobs/{id}/logs`, `/runs/{id}/cancel`, `/rerun`, `/artifacts`, `/actions/secrets`, `/actions/variables`, `/actions/runners` | ch-ci |
| Hooks and notifications | `/repos/{o}/{r}/hooks`, `/orgs/{o}/hooks`, `/notifications`, `/repos/{o}/{r}/subscription` | ch-notify |

The `gh` CLI is not a target because it relies on GraphQL. Octokit's REST
client with a custom `baseUrl`, curl scripts, Renovate's REST paths, and our own
`haven` CLI are.

## 12. Web UI

### 12.1 Why Askama and htmx, not Leptos

- **The work is pages, not an app.** A forge is mostly documents: file views,
  diffs, timelines, lists. Server rendering is the natural fit and pages load
  fast on a phone because there is no bundle to download and hydrate.
- **Templates are checked at compile time.** Askama turns each template into Rust
  code. A missing field is a compile error in the stream that caused it.
- **No second build pipeline.** Leptos needs a WASM target, `cargo-leptos`, and
  hydration that must match the server render exactly. That is another toolchain
  on the build host, longer builds, and a whole class of mismatch bugs.
- **It parallelises.** `ch-web` only calls service traits. With demo fakes it can
  build every page before the backends exist. Leptos server functions would tie
  the UI crate to backend crates directly.
- **The interactive parts are small.** Live logs are SSE, which htmx's SSE
  extension handles. Review comments, label pickers, reactions and the merge
  box are htmx partial swaps. Everything works without JavaScript except live
  updates, which fall back to a refresh link.

Maud was the other server-side option. Askama wins because templates are plain
HTML files that read like the page, which helps visual review.

### 12.2 Shape of ch-web

- One handler per page, returning a template. Requests with `HX-Request` get
  the partial instead of the full page.
- Shared layout and components as Askama macros: header, repo sidebar, file tree,
  blob view, diff view, timeline item, comment editor, label pill, avatar,
  status icon, empty state, pagination, flash messages.
- Static assets in `crates/ch-web/static`, embedded with `rust-embed`, served
  with content hashes in the file names and a one-year cache.
- JavaScript budget: vendored `htmx.min.js` and its SSE extension, plus
  `app.js` under 15 KB for theme toggle, copy buttons, line selection and
  permalinks, keyboard shortcuts (`/` search, `t` file finder, `g i` issues,
  `g p` pull requests, `y` permalink), and the textarea's tab and list helpers.
  No CDN. No inline script, so the CSP is `script-src 'self'`.
- File editor: a monospace textarea with a Write and Preview tab. Preview is a
  server render. A richer editor can come later as progressive enhancement.
- A `demo` cargo feature serves every page from fake services with fixed data,
  for visual review and screenshots before the backends land.

### 12.3 Look and feel

The layout is familiar to anyone who has used a forge. The look is our own:

- **Name and mark**: "Codehaven", with an original SVG mark: a rounded arch
  (a harbour entrance) with a short beam of light across it. One colour, works at
  16 px. No resemblance to any existing forge logo.
- **Colour**: harbour teal is the accent (`#0f766e` light, `#2dd4bf` dark).
  States: open is teal, merged is deep blue (`#1d4ed8`), closed is brick
  (`#b42318`), draft is slate. Backgrounds are warm off-white (`#fbfaf7`) in
  light and blue-black (`#0e1416`) in dark. All text meets WCAG AA contrast.
- **Type**: the system UI font stack for text and `ui-monospace` for code. No web
  fonts, so nothing is fetched from outside the instance.
- **Icons**: Lucide (ISC licence) as an inline SVG sprite.
- **Structure**: inside a repo, sections (Code, Issues, Pull requests, Actions,
  Releases, Packages, Settings) sit in a left sidebar on wide screens, with the
  file tree beside the content. This is the main visual difference from the
  usual top tab bar.
- **Tokens**: every colour, radius, space and shadow is a CSS custom property on
  `:root`, redefined for dark under `prefers-color-scheme: dark` and under
  `[data-theme=dark]`. The theme choice (system, light, dark) is a user setting,
  mirrored in a cookie so the sign-in page matches.
- **Syntax colours**: syntect emits classes (`hl-keyword`, `hl-string` and so
  on). We write the two palettes ourselves from the tokens.

### 12.4 Phone friendly

- Breakpoints at 640 px and 1024 px. Tested at 390 x 844 and 1280 x 800.
- Below 1024 px the repo sidebar becomes a scrollable chip row under the repo
  name, the file tree moves behind a "Files" button, and diffs are unified only.
- Below 640 px tables become stacked cards and the header search becomes an icon.
- Tap targets at least 44 px. No horizontal page scroll. Code and diff blocks
  scroll inside themselves.
- Long lines in code views wrap on request (a toggle), not by default.

### 12.5 Accessibility

Landmarks on every page, a skip link, visible focus rings, labelled form
controls, status icons with text alternatives, and diff lines announced with
"added" and "removed" rather than colour alone. `prefers-reduced-motion` turns
off transitions.

### 12.6 Pages

Home (dashboard with your repos, recent activity, assigned issues and review
requests), explore (accounts only), sign in, two-factor, accept invite, set
password, profile, org page and members, team pages, settings (profile, emails,
password, two-factor, sessions, security log, SSH keys, tokens, notifications,
appearance), new repo, import repo, repo home (README,
about, branches, latest commit with status), tree, blob (highlighted, line
anchors, raw, blame, history), blame, commits, commit, branches, tags, compare,
edit and new file, issues list, issue, new issue, labels, milestones, pull
requests list, PR (conversation, commits, checks, files), new PR, actions (runs
list, run, job with live log, workflow list with validation errors, manual run
form), releases, release, new release, packages, notifications inbox, search,
repo settings (general, collaborators and teams, branches and protection,
webhooks, secrets and variables, runners, danger zone), site admin (invites,
users, repos, runners and runner tokens, audit log, queue, health and disk). Site
admin pages need the admin to have two-factor turned on.

Raw files are served from `/{o}/{r}/raw/{ref}/{path}` with
`Content-Security-Policy: sandbox`, `X-Content-Type-Options: nosniff`, and
`Content-Disposition: attachment` for HTML and SVG, so a file in a repo cannot
run script on the instance's origin.

## 13. CLI (haven)

```
haven auth login --host https://code.example.com   # paste a token; stored in the OS keyring (keyring crate)
haven auth git-credential                           # git credential helper
haven repo create|clone|view|fork|list
haven issue list|view|create|close|comment
haven pr create|list|view|checkout|diff|review|merge
haven run list|view|watch|logs|rerun|cancel
haven workflow list|run
haven release create|list|upload
haven api <path> [-X METHOD] [-f key=value]         # raw REST calls
```

Built on `clap`, `reqwest` with rustls, and the DTOs from `ch-api-types`, so the
CLI breaks at compile time when the API contract changes.

## 14. Server binary, config and operations

```
codehaven serve [--migrate]        # HTTP, SSH and workers in one process
codehaven migrate
codehaven admin create-user --username u --email e [--admin] [--password-stdin]   # prints a 1 hour set-password link
codehaven admin invite create [--email e] [--admin] [--org o --team t] [--expires 7d]   # prints the invite link
codehaven admin invite list|revoke <id>
codehaven admin runner-token [--repo o/r]
codehaven admin reset-password u   # prints a 1 hour link, signs the account out everywhere
codehaven admin unlock u
codehaven admin suspend u
codehaven hook pre-receive|post-receive   # called by git, not people
codehaven healthcheck              # used by the compose healthcheck, hits the internal listener
codehaven doctor                   # git version, paths, permissions, disk, database, public route list
```

Every `admin` subcommand runs inside the server container, so only someone with
root on the host can use it, and each one writes an audit event with the actor
`System (cli)`.

The server opens two listeners:

- **Public**, `0.0.0.0:3200` in the container: everything in sections 6 to 12,
  behind the gate. The reverse proxy reaches it by its alias on the proxy
  network, and the host has it where `CODEHAVEN_HTTP_BIND` says.
- **Internal**, `127.0.0.1:9300` inside the container only: `/readyz` and
  `/metrics`. Nothing outside the container can reach it.

- One process runs HTTP, SSH and all workers. Single instance by design. The
  in-process log broadcast is the only part that assumes it, and moving it to
  Postgres `LISTEN/NOTIFY` is the path to more than one.
- Logs: `tracing` with JSON output. Every request has a request id.
- `/healthz` (process up, public, says only `ok`) and `/readyz` (database
  reachable, data dir writable, internal listener only).
- Prometheus metrics at `/metrics` on the internal listener only.
- Security headers on every response, listed in section 7.6.
- Backups (M4): `codehaven admin backup` writes `pg_dump` plus a tar of
  `data/git` and `data/blobs` to a target directory.

## 15. Build and test workflow

Edits happen anywhere. Compilation and tests run in a builder container, on
this machine or on another host chosen by `CH_HOST` (an ssh name) and
`CH_REMOTE_ROOT` (the directory on that host that holds the checkouts), both
from the environment or from `deploy/dev/local.env` (see
`deploy/dev/local.env.example`). There is no default host: with `CH_HOST` unset,
`deploy/dev/local.sh` runs the same job on this machine and no ssh is involved.
The `Makefile` wraps the scripts in `deploy/dev/*.sh`.

### 15.1 Builder image

`deploy/images/builder.Dockerfile`: `rust:<pinned>-trixie` plus `git`,
`openssh-client`, `sccache`, `cargo-nextest`, `sqlx-cli` and `insta`'s
`cargo-insta`, built once as `codehaven-builder:<toolchain>`
(`deploy/dev/builder-image.sh`).

### 15.2 Caches

| Volume | Shared by | Holds |
| --- | --- | --- |
| `codehaven-cargo-home` | all streams | registry index, crate sources, git deps |
| `codehaven-sccache` | all streams | compiled dependency objects, capped at 20 GB |
| `codehaven-target-<stream>` | one stream | that stream's `target/` |

A target dir per stream, because cargo holds a lock on a target directory for
the whole build and one shared dir would serialise every stream. sccache makes
the per-stream dirs cheap: a dependency compiled for one stream is a cache hit
for the next. Debug info is `line-tables-only` in the dev profile to keep target
dirs near 3 GB each.

### 15.3 Running a build

`deploy/dev/remote.sh <stream> <command>` from the working machine, or
`deploy/dev/local.sh <stream> <command>` with no build host:

1. `rsync`s the stream's worktree to `<CH_REMOTE_ROOT>/streams/<stream>/` on
   the build host, skipping `target/`, `.git/` and the env files.
2. Runs `deploy/dev/diskcheck.sh` there and stops if a watched disk
   (`CODEHAVEN_DISK_PATHS`) is at `CODEHAVEN_DISK_LIMIT` percent or more.
3. Takes one of `CODEHAVEN_BUILD_SLOTS` slots (3 by default) with `flock` on
   `<CODEHAVEN_BUILD_ROOT>/run/slot.<n>`, waiting if all are busy, so a few
   builds run at once and the host keeps room for everything else.
4. `deploy/dev/slot.sh` runs `docker compose -p codehaven-dev run --rm builder
   bash -c <command>` (not `-lc`: a login shell on Debian resets `PATH` and
   loses cargo), where `compose.dev.yml` caps the builder at
   `CODEHAVEN_BUILD_CPUS` cpus (default `min(nproc, 3)`) and
   `CODEHAVEN_BUILD_MEMORY` memory (default 8g), with `SQLX_OFFLINE=true` and
   `DATABASE_URL` pointing at `pg-test`.

`deploy/dev/host.sh` syncs the same way but runs the command on the host
itself, not in the builder, for docker and compose work such as release builds
and the end-to-end run.

`#[sqlx::test]` creates a fresh database per test inside `pg-test`, so streams
never see each other's data. Git tests use the real `git` in the builder image.

### 15.3a Docker tests

The runner's executor tests need a Docker daemon, and the builder has none (it never gets the
host's). `make STREAM=runner docker-test` starts `dind-test`, a throwaway daemon in the
`docker-tests` profile of `deploy/compose.dev.yml` on the private `dev` network, builds the job
image into it, runs `crates/haven-runner/tests/docker.rs` with `HAVEN_REQUIRE_DOCKER=1`, and stops
it. Its images live in the `codehaven-dind-test-data` volume, which `clean.sh` removes.

### 15.4 The end-to-end run

`deploy/dev/e2e.sh` (or `make e2e E2E_TAG=test-<word>`) builds the images under
a throwaway tag and runs the harness from `tests/e2e` in a compose project of
its own (`deploy/compose.e2e.yml`), then tears that project down whatever
happened. Throwaway is enforced, not requested: the project name and the image
tag must both be `test-`, `e2e-`, `shots-` or `tmp-` plus a word
(`deploy/lib/throwaway.sh`), a project that already holds a container that was
not started as a throwaway one is left alone, and a tag the running instance
uses is refused. There is no flag that relaxes any of this.

### 15.5 Cleanup

`deploy/dev/clean.sh` removes the `codehaven-target-*` volumes and the synced
checkouts of the streams named on its command line, the runner's test volumes,
dangling `codehaven-*` images and the codehaven buildx cache
(`CODEHAVEN_BUILDX_BUILDER` when set). It never runs `docker system prune`.

### 15.6 Release images and tags

`deploy/dev/release.sh <tag>` (or `make release TAG=<tag>`) builds the release
binaries inside the builder, so they use the same cargo and sccache volumes and
the same build slots as every other build, then `docker buildx build --load`
packs them into runtime images from a `dist/` directory
(`deploy/images/runtime.Dockerfile`). Runtime images are `debian:trixie-slim`
with `git`, `ca-certificates` and `tini`, run as uid 10001: `codehaven-server`,
`codehaven-e2e` (the harness, which also holds the server binary so steps can
run `codehaven admin`), `codehaven-runner`. `codehaven-job:trixie` is built by
`deploy/dev/job-image.sh` from `deploy/images/job.Dockerfile` and loaded into
`dind` by `deploy/prod/ci-up.sh`. Building the binaries in the builder rather
than in a Dockerfile stage keeps the release build inside the same CPU cap as
every other build.

The tag is required and has no default: it is how an instance picks what runs.
`deploy/prod/up.sh` uses `CODEHAVEN_TAG` from the env file, else the short
commit id of the checkout, and never a name something else may already run;
`release.sh` refuses a tag a running instance uses unless `--deploy` says
replacing it is the point.

### 15.7 The portability check

`deploy/check-portable.py` (or `make portable`) fails when something that
belongs to one deployment is in the tree: a person's or host's name, an address
outside the documentation ranges, a real email domain, or a path of one
particular machine. The blocked words are stored as SHA-256 hashes, so the
check itself publishes none of them. Codehaven is meant to be cloned and run by
anyone, so no file may carry a name, an address or a path that belongs to one
instance. `deploy/export-public.sh` runs the same check over a clean snapshot
of the tree, with the publisher's own private patterns added, before anything
is published.

## 16. Quality gates

Every stream, before every merge:

- `cargo fmt --check`
- `cargo clippy -p <crates> --all-targets -- -D warnings`
- `cargo nextest run -p <crates>` in the builder
- `cargo sqlx prepare --check --workspace`
- `cargo build --workspace` in the builder with main merged in
- the end-to-end steps for the current milestone (`ROADMAP.md` section 6)

Plus, per stream: property tests (`proptest`) for parsers (pkt-line, diff,
expressions, cron, refs), `insta` snapshots for templates and API JSON, and for
the web stream, screenshots at phone and desktop width in light and dark,
reviewed before merge.

## 17. Not in v1

GraphQL, a wiki, project boards, Git LFS, pull mirrors, JavaScript actions from
a marketplace, service containers, reusable workflows, environments with
approval, WebAuthn, more than one server instance, S3 storage. Each fits behind
an existing boundary and is listed in `ROADMAP.md` under later work.

Never, by design: open sign-up, anonymous read access, a web setup page for the
first admin, and login by trusted proxy header.
