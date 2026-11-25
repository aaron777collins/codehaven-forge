# The git stream: ch-git, ch-ssh, ch-registry

What was built, how it fits together, and what is left. `ARCHITECTURE.md` sections 5.1, 6 and
10 are the design; this file is the working notes for whoever integrates or extends it.

## One service, three traits

`ch_git::GitCore` implements `Repos`, `GitService` and `GitTransport`. `ch_git::build` puts it in
`AppState` as those three traits and also in `AppState.ext`, so ch-git's own REST handlers, its
hook server and its housekeeping worker get the concrete type back with
`state.ext.get::<GitCore>()`. ch-ssh and ch-registry never see it: ch-ssh starts git through
`GitTransport::open`, ch-registry uses only the database, the blob store and `Authz`.

`build` needs the real `Authz` in `b.authz` (identity builds first). It writes the hook scripts, so
it must run in the server's data directory; `codehaven admin` does the same, which is harmless.

## Changes to foundation files (small, all additive)

| File | Change |
| --- | --- |
| `ch-core/services.rs` | `GitTransport` trait with `GitCaller`, `GitWire`, `GitMode`, `GitSession`. `GitService::blame` and `update_ref`, `Repos::update_settings` and `list_accessible`, all with default bodies so another stream's fake keeps compiling |
| `ch-core/model.rs` | `Blame`, `BlameRange`, `RepoSettingsUpdate`; `Signature` is now `PartialEq, Eq` |
| `ch-core/state.rs` | `Extensions` and the `ext` field on `AppState` and `AppStateBuilder`; the `git_transport` service |
| `ch-core/not_built.rs` | `NotBuilt` implements `GitTransport` and fails closed |
| `codehaven/reqinfo.rs` | The request middleware also inserts a plain `ClientInfo`, because domain crates cannot name `RequestMeta` |
| `codehaven/main.rs` | `codehaven hook` calls `ch_git::hooks::client::run` |
| `Cargo.toml` | `russh` (ring backend, no aws-lc), `rand10` (the rand that russh uses), `async-compression`, `moka`, `regex` |
| `deploy/dev/remote.sh` | Works when run from inside a stream's own worktree |

## On disk

```text
data/git/<shard>/<repo_id>.git     bare repositories
data/git/_hooks/                   pre-receive and post-receive, rewritten at every start
data/git/_run/hook-<pid>-<id>.sock the hook socket (0600), swept when stale
data/git/_home/                    an empty HOME for git
data/git/_trash/                   deleted repositories, removed by housekeeping
data/blobs/_uploads/<uuid>         registry uploads in progress
data/secrets/ssh_host_ed25519      the SSH host key (0600, made on first start)
```

All of it is under the volumes the compose file already mounts.

## The push pipeline

Every ref change goes through two functions in `pipeline.rs`: `authorize` (permission, archived,
namespace, branch protection) and `record` (one transaction: repository row, `Push`, `RefCreated`
and `RefDeleted` events). A `git push` reaches them through the hooks; a merge, a web edit or an
API ref change reaches them through `GitCore::apply_ref_change`, which wraps a compare-and-swap
`git update-ref`. Same rules, same events, whoever asked.

- The server puts `CODEHAVEN_PUSH_ID`, `CODEHAVEN_REPO_ID` and `CODEHAVEN_SOCKET` in the
  environment of `receive-pack`. The hook sends the ref updates over the socket. A hook that is
  run by hand, or with a guessed id, is refused. Pushing straight into the bare directory is
  refused for the same reason (`core.hooksPath` is set at creation and passed with `-c`).
- Only `refs/heads/*` and `refs/tags/*` can be pushed. `refs/pull/*` is written only by the server
  (`Actor::System` with `Origin::System`; a deploy key carries `System` but `Origin::Push`).
- Branch protection (`protect.rs`): a protected branch refuses direct pushes, force pushes and
  deletion. A pull request merge (`Origin::PullRequestMerge`) may land; creating a branch under a
  pattern is allowed so a pull request can be opened. The default branch can never be deleted.
- `Push` is emitted for every ref that ends up pointing somewhere. Deleting a ref emits only
  `RefDeleted`, so consumers of `Push` never see an all-zero `after`.
- The first branch pushed to an empty repository becomes the default branch when the configured
  one was not pushed (`main`, then `master`, then the first by name).

## Tiers, forks and the rebuild before widening

A repository's tier is `repositories.access_tier`: `private` (only people granted access),
`internal` (also every signed-in, active account) or `public` (also anyone on the internet,
while the site admin allows public repositories). The legacy `visibility` column is kept in
step by a trigger, so older binaries still see the right value.

Git serves any object in a repository's store to anyone who may clone it, deleted history
included. So before any widening of the tier the store is rebuilt from its refs alone: a
fresh bare repository fetches every ref, passes `git fsck --connectivity-only`, must list the
same refs as the old one, and only then replaces it. The old store is parked, unreachable,
for 7 days under `data/git/_prepublish` and swept away after; a startup check puts back a
store whose swap was interrupted.

Only the account owner, an owner of the owning organization, or a site admin with two-factor
in a browser session can make a repository public, on `/{owner}/{repo}/settings/visibility`,
after typing the repository's name and ticking that the history was checked for secrets.

Forks: a new bare repository fetches every branch and tag of its source, so the fork copies
its objects and borrows nothing through `alternates`. A fork starts private and can never be
more visible than its source. A repository that still has forks cannot be deleted.

## Notes for the other streams

- **collab.** `GitService::merge` returns `Merged`, `Conflict { paths }` or `BaseMoved`.
  `test_merge` returns `Clean`, `Conflict` or `Unknown`; it never returns `Behind` or `Blocked`,
  which depend on protection rules collab owns. `diff` and `compare` are three-dot (from the
  merge base); for a commit and its parent that is the commit's own change. A pull request from a
  fork needs no work from collab: `merge`, `test_merge` and `create_ref` fetch the head commit
  from the fork network when the base repository lacks it. `create_ref` with `Actor::System` is
  how `refs/pull/<n>/head` and `/merge` are written.
- **web.** `GitService` reads take no actor: check access first with `Repos::get` or
  `Repos::get_by_name`, which answer `NotFound` for what the caller cannot read. Writes check it
  again. Settings go through `Repos::update_settings`.
- **identity.** `Authz::require` is called for every operation. `Actor::System` skips it. Deploy
  keys are not accounts: ch-ssh passes `GitCaller::Deploy` and ch-git checks the repository and
  `read_only` itself.
- **ci.** `Actor::Job` may read its own repository over smart HTTP and nothing else.

## REST

`ch_git::api_routes()` registers the repository endpoints in ARCHITECTURE.md section 11: repos,
topics, forks, transfer, collaborators, branches and protection, tags, commits, compare,
contents (read, create, update, delete, raw media type), readme, archives and the git data
refs, trees and blobs. Two path names need agreeing with ch-ci when the streams merge: this crate
registers `/repos/{owner}/{repo}/commits/{ref}`; ch-ci registers `/commits/{ref}/status`.

## Registry

`ch_registry::registry_routes()` is OCI Distribution 1.1 at `/v2/`: version check, token
exchange, blob uploads (chunked, monolithic, mounted), manifests and indexes, tags with paging,
referrers, deletes and garbage collection. Images are `owner/image`.

Not verified against the OCI conformance suite or a real `docker login`: the suite and the
Docker CLI need a running instance with real accounts, which means the integrated stack. The
tests in `crates/ch-registry/tests/registry.rs` make the same calls by hand, and the first thing
to run after integration is `docker login`, `docker push` and `docker pull` against a test
instance, then the conformance container.

## Tests and where they run

`make STREAM=git test P=ch-git` (also `ch-ssh`, `ch-registry`). The integration tests use a real
Postgres, real bare repositories, the real `git` and `ssh` clients and the `ch-hook` binary
(`src/bin/ch-hook.rs`, the same client `codehaven hook` runs). Identity does not exist in this
branch, so `tests/common/mod.rs` has stand-ins for `Authn` and `Authz` that follow section 7.2.
The test runtime is single-threaded, so client processes are started with `tokio::process` and
never block.

## Left undone, and why

- **Import from a URL.** It needs a fetcher that cannot be pointed at a private address
  (resolve, refuse private addresses, refuse redirects, pin the address). Without that it is a
  server-side request forgery hole, so it is not built.
- **`gix` for reads.** Dropped for now: git's own plumbing does every read and no behaviour depends on
  the choice. See ARCHITECTURE.md 6.1.
- **Re-parenting a fork network** so a root with forks can be deleted. Deleting such a root is
  refused with a message.
- **Mergeability recompute.** It reads pull request rows, so it belongs to collab; `test_merge`
  is the building block.
- **Docker schema 1.** Dropped: no client has sent it since 2017. Range requests on blob downloads
  (`crates/ch-registry/src/range.rs`) and the catalog endpoint (`catalog.rs`) are built.
- **Queries are `sqlx::query` with `FromRow`, not `query!`.** The citext columns and the joins make
  offline metadata awkward, and every query is exercised by the tests. Moving them to `query!`
  later is mechanical, and nothing needs it: dropped.
