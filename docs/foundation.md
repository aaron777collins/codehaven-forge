# What the foundation stream provides

This is the working guide for every other stream: the interfaces you build on, where to put your
code, and how to build and test. `ARCHITECTURE.md` has the design and `ROADMAP.md` the plan.
When this file disagrees with them, fix whichever is wrong in the same commit.

## Your first ten minutes

```sh
cp .env.example .env                     # once, in the main checkout: CODEHAVEN_BUILD_ROOT at least
git worktree add .worktrees/<stream> -b stream/<stream> main
make STREAM=<stream> test P=<your-crate> # builds and tests in the builder container
```

`make` is a thin wrapper over the build scripts in `deploy/dev/`. Builds run on this machine by
default (`deploy/dev/local.sh`). Set `CH_HOST` and `CH_REMOTE_ROOT` (in the environment, or in
the untracked `deploy/dev/local.env`; see `local.env.example`) and the same targets run on that
host over ssh instead: `deploy/dev/remote.sh` rsyncs your worktree to
`<CH_REMOTE_ROOT>/streams/<stream>` first. Nothing picks a host for you. Either way the command
runs in the `codehaven-builder` container against a throwaway Postgres: the wrapper checks disk,
takes one of the build slots (`CODEHAVEN_BUILD_SLOTS`, default 3) and starts the container, so
the machine needs Docker but no Rust. The first build also makes the builder image, which is
slow once.

| Task | Command (in your worktree) |
| --- | --- |
| Test one crate | `make STREAM=git test P=ch-git` |
| Test everything | `make STREAM=git test` |
| Lint | `make STREAM=git lint P=ch-git` |
| Formatting | `make STREAM=git fmt` |
| The merge gate | `make STREAM=git ci` (fmt, clippy `-D warnings`, `.sqlx` fresh, build, nextest) |
| After a query or migration change | `make STREAM=git sqlx` (refreshes `.sqlx/`, fetches it back) |
| Any command in the builder | `CH_FETCH=".sqlx" bash deploy/dev/local.sh <stream> "cargo ..."` (or `remote.sh`, when building over ssh) |
| End-to-end run | `make e2e E2E_TAG=test-x` (steps 0-1 by default, `STEPS=0-19` for the whole run) |
| Runner executor tests (real Docker) | `make STREAM=runner docker-test` (see `docs/runner.md`) |

Inside the builder: `DATABASE_URL` points at `pg-test` (tmpfs, `#[sqlx::test]` makes one
database per test), `SQLX_OFFLINE=true` uses the committed `.sqlx/`, `sccache` and the cargo
registry are shared volumes, and `/target` is a per-stream volume. The first build of a stream
is slow, later ones are fast.

When you add or change a `sqlx::query!` and have no `.sqlx/` entry yet, build once with
`SQLX_OFFLINE=false` (the database is migrated for you by `cargo xtask db-prepare`).

## The dependency rule

Domain crates (`ch-git`, `ch-ssh`, `ch-registry`, `ch-auth`, `ch-notify`, `ch-collab`, `ch-ci`,
`ch-web`) depend only on `ch-core`, `ch-db`, `ch-api-types` and the pure `ch-workflow` and
`ch-render`. They never depend on each other. When collab needs a merge it calls
`ch_core::services::GitService`. Only `crates/codehaven` knows every crate.

## Contracts

| Where | What |
| --- | --- |
| `ch_core::ids` | `UserId`, `RepoId`, ... newtypes over `i64`, with sqlx and serde support |
| `ch_core::actor` | `Actor` (`Anonymous`, `User`, `Token`, `Job`, `Runner`, `System`), `Permission`, `Scopes`. `Actor::is_site_admin()` needs a session with two-factor |
| `ch_core::error` | `Error` and its GitHub-shaped `IntoResponse`. Private repos you cannot read are `NotFound` |
| `ch_core::services` | Every service trait. `NotBuilt` (in `ch_core::not_built`) implements them all and fails closed. `GitTransport` is how ch-ssh and smart HTTP start git (added by the git stream) |
| `ch_core::model` | Plain data passed between services. No URLs: the REST edge adds them |
| `ch_core::events` | `DomainEvent`, the outbox payload |
| `ch_core::audit` | `AuditEvent` and the list of action names in `audit::action` |
| `ch_core::access` | `PUBLIC_ROUTES`, `classify`, `actor_accepted`, the `CurrentActor` extractor |
| `ch_core::state` | `AppState`, `AppStateBuilder`, `Install`, `Worker`, `Extensions` (a typed map: `build` inserts a crate's concrete service, its routes and workers read it back with `state.ext.get::<T>()`) |
| `ch_core::identity` | Sign-in, sessions, two-factor, orgs, directory traits for the web layer, and `CsrfToken`. See `docs/identity.md` |
| `ch_core::web_contract` | Added by the web stream: `Sessions`, `Directory`, `Tracker`, `Pipelines`, their models, `RepoUpdate` and `BlameHunk`. `NotBuilt` implements them, so the identity, collab and ci streams fill them in. See docs/web.md |
| `ch_core::text::refs` | `@user`, `#12`, `owner/repo#12`, shas and closing keywords |
| `ch_core::net` | Client IP from `X-Forwarded-For`, trusted proxies only |
| `ch_db` | `connect`, `MIGRATOR`, `PgAudit`, `PgEventBus`, `FsBlobStore`, `jobs::JobQueue`, `sealing::MasterKey`, `tokens` |
| `ch_api_types` | REST v3 DTOs per group, webhook payloads, `runner` protocol |
| `ch_testkit` | `TestEnv` (users, repos, collaborators, an `AppStateBuilder`), `TempRepo`, actor builders |

### Each crate's entry points

Every domain crate already has these, so wiring never changes. Fill in the bodies.

```rust
pub fn api_routes() -> axum::Router<AppState>;          // mounted under /api/v3
pub fn workers(state: &AppState) -> Vec<Worker>;        // background loops, stop on the token
pub fn build(b: &AppStateBuilder) -> Result<XServices>; // XServices: Install
```

Extra entry points: `ch_git::http_routes()` (smart HTTP, merged at the root),
`ch_ci::runner_routes()` (under `/api/runner/v1`), `ch_registry::registry_routes()` (at `/v2`),
`ch_web::web_routes()` (at the root, replaces the placeholder). Build order in
`wiring::install_services`: auth, notify, git, collab, ci. A service that needs another reads it
from the builder: `b.authz.clone()`, `b.git.clone()`.

### The gate: your routes are private until listed

`crates/codehaven/src/gate.rs` wraps the whole router. A signed-out request reaches a handler
only if its method and path are in `ch_core::access::PUBLIC_ROUTES`. Everything else gets the
rejection for its kind (page: 303 to `/login?next=`, API and runner: 401 JSON, git: 401 with a
Basic challenge, registry: 401 with a Bearer challenge). It also limits which credential opens
which door: a session opens pages and the API, Basic and tokens open the API, git and registry,
a job token opens the runner API and git, a runner credential opens the runner API only.

Reads on public repositories are the one exception, in `ch_core::access::PUBLIC_REPO_ROUTES`:
the gate admits those only while the site admin allows public repositories and the repository
the request names is public, and `Authz` must still say `View`.

You do not write any auth plumbing for "signed out". Your handler gets the caller with
`CurrentActor(actor): CurrentActor` and calls `Authz`. To make a route public you edit
`PUBLIC_ROUTES`, which only Claude reviews, and delete its line from `PUBLIC_NOT_BUILT_YET` in
`crates/codehaven/tests/anonymous_surface.rs` if it was waiting there.

`crates/codehaven/tests/anonymous_surface.rs` walks every route the router has registered, with
every method, signed out. A new route is covered the moment you register it. Run it with
`make STREAM=<s> test P=codehaven`.

### Database

`crates/ch-db/migrations/` is the whole v1 schema. Need a column or table? Open a small pull
request against foundation with the migration and the refreshed `.sqlx/`. Rules: ids are
`BIGINT GENERATED ALWAYS AS IDENTITY`, names people type are `citext`, times are `timestamptz`,
tokens are stored as 32 byte SHA-256 hashes (a CHECK enforces the length).

Write queries with `sqlx::query!` so a schema change is a compile error. In tests use
`#[sqlx::test(migrator = "ch_db::MIGRATOR")]` and `ch_testkit::TestEnv::new(pool)`.

Emit events and audit records in the same transaction as the change:

```rust
let mut tx = state.db.begin().await?;
// ... change rows ...
state.audit.record(&mut tx, AuditEvent::new(&actor, &client, action::TOKEN_CREATED, Outcome::Success).target("token", id)).await?;
state.events.emit(&mut tx, DomainEvent::RepoCreated { repo_id, actor: Some(user) }).await?;
tx.commit().await?;
```

`emit` takes a transaction-scoped advisory lock so event ids commit in order and a consumer's
cursor never skips one. Keep transactions that emit short.

### Runner protocol

`ch_api_types::runner` is the contract between `ch-ci` and `haven-runner`. Change it in a small
pull request announced to both streams. Secrets in a job payload are only the ones the job
references, never for fork pull request jobs.

## Running the stack

```sh
make up                     # builds the images, starts the instance the .env describes
make e2e E2E_TAG=test-x     # throwaway stack in a project of its own, tears down after
make down                   # stop it, keep the data
make clean                  # remove what Codehaven's builds made (never `docker system prune`)
```

With no `CH_HOST` these run on this machine; with one they run on the build host over ssh. The
first run on a host is `sudo bash deploy/prod/setup.sh` (directories, permissions, database
credentials), then `deploy/prod/up.sh`; `docs/integration.md` walks the whole first run, and
`.env.example` lists every setting. By default the web server and ssh bind to loopback only
(127.0.0.1:3200 and :2222).

Reach the instance from another machine with `ssh -L 3200:127.0.0.1:3200 <host>`, then
`http://127.0.0.1:3200`. Sign-in forms only accept the public origin (ARCHITECTURE.md 7.6), so a
browser sign-in on the tunnel needs `CODEHAVEN_EXTERNAL_URL=http://127.0.0.1:3200` in the compose
environment.

The first admin account is created on the server, never from the web:

```sh
sudo bash deploy/prod/first-admin.sh <username> <email>
```

It prints a one-time link (valid for 1 hour). The script refuses unless the instance has no
accounts at all, and it never deletes one; every account after the first comes from an invite
(`docs/integration.md`).

## What is deliberately not here

- No sign-up route, no setup page, no proxy-header login, no anonymous read outside public
  repositories, and no anonymous write. Ever.
- Rate limits, lockout, CSRF and the Origin check are built (see `docs/identity.md`). Handlers get the
  caller's address with the `ch_core::access::Client` extractor.
- `codehaven hook` is the git stream's push pipeline client (`ch_git::hooks::client`); see `docs/git.md`.
- Nothing in `ch-web` talks to the database or to another domain crate: it calls the service traits only. See docs/web.md.
