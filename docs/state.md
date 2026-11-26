# State of the project

Written at the end of batch C14, 2026-11-26. Every open bead of the three big epics is closed. This page
says what is built, how each part is checked, and what is left out on purpose.

## What is built

| Area | What works | Where to read |
| --- | --- | --- |
| Git | Clone, fetch and push over HTTP and SSH, branch protection enforced in a pre-receive hook, forks with lineage, large raw files streamed from git | `docs/git.md` |
| Accounts | Invite-only sign-up, passwords with two-factor (authenticator app or passkey), personal access tokens with scopes, SSH keys, organizations and teams, a lockout and rate limits on every sign-in path | `docs/identity.md` |
| Collaboration | Issues, pull requests with review, line comments, labels, milestones, reactions, notifications, webhooks, merge queue, releases with files and digests | `docs/collab.md` |
| Visibility | Private, internal and public repositories. Signed out, a private, an internal and a missing repository look the same. Public repositories need the site admin's switch | `docs/identity.md`, `tests/access/` |
| CI | Workflows in `.codehaven/workflows` or `.github/workflows`, real actions fetched from GitHub at pinned commits, reusable workflows, matrix, services, YAML anchors, caches, artifacts, job tokens with `permissions:`, live logs | `docs/ci.md`, `docs/runner.md` |
| Packages | A container registry at `/v2`, a NuGet feed at `/nuget/{owner}`, package pages, push with a job token | `docs/ci.md` |
| Pages | A static site per repository from a branch, on a host of its own, sandboxed | `docs/design/pages.md` |
| Browser workspace | Monaco editor, tree, search, diff, many-file commits, run panel with live logs, a shell in the tab that touches nothing until you tick it in review | `docs/web.md`, `docs/terminal.md` |
| Claude connector | An MCP endpoint behind OAuth with per-repository grants for read, write and workflows | `docs/connector.md` |
| Operations | One-command setup, daily backups with restore, an optional off-host backup target, upgrade and rollback by image tag | `docs/operations.md` |

## How it is checked

- `make ci` is the gate: the portability check, formatting, clippy with warnings denied, the build and the tests.
- `make e2e` and `deploy/dev/shots.sh` start a throwaway stack with real runners and run the end-to-end steps in
  `tests/e2e/e2e_full.py` (0 to 38). Steps 35 to 38 run real, unmodified actions from GitHub at pinned commits.
- The signed-out surface is a committed table per tier (`tests/access/anonymous.*.txt`), checked against every route the
  router registers (`crates/codehaven/tests/public_surface.rs`).
- Every other credential is a second committed table (`tests/access/matrix.*.txt`, `crates/codehaven/tests/access_matrix.rs`):
  every registered route, with every method, for each credential (none, session, basic, personal access token with read
  and with write scope, job token with contents read and write, a job token of another repository, runner, connected app),
  at each tier, with the owner's contribution limit on and off. A route that is not in `matrix.routes.txt` fails the
  build, so a new route cannot arrive unreviewed. See `docs/operations.md` for how to regenerate it.
- The flood test (`crates/codehaven/src/public_gate.rs`, module `flood`) holds the per-address and instance caps for
  heavy signed-out requests under a thousand addresses.
- The workspace is run in Chromium, Firefox and WebKit (`tests/e2e/workspace.mjs`, `monaco_worker.mjs`, `terminal.mjs`).
  The build host has Chrome only, so Firefox and WebKit run from a machine that has them (`docs/terminal.md`).

## What is left out, and why

The threat that matters here is a stranger with no account, and losing data. Invited people are trusted, so work that
only protects against an invited person is kept simple or left out.

| Left out | Why |
| --- | --- |
| A throwaway virtual machine for each CI job (ch-ctk.3) | The risk it removes is an invited person's. Jobs run in containers on a separate Docker daemon with no socket and no extra capabilities. It can be added later without changing the workflow format |
| Docker inside a job (`docker build`, buildx, `docker login`) | There is no Docker daemon in a job, by design. These fail at once with a reason. A `docker://image` step or a `run:` step works. The registry itself is tested with a real Docker client |
| OIDC, GraphQL, `workflow_run`, `repository_dispatch`, `pull_request_target`, `environment:` URLs, Windows, macOS and ARM runners | Not needed for a forge for people who know each other, and each is a large feature. Actions that need them fail with a named error and do not hang |
| Linux in the browser tab (ch-1ke.9) | Optional and large. The shell in the tab covers everyday commands |
| A wildcard certificate for Pages full mode (ch-lr5) | Needs a DNS challenge and a proxy build with a DNS provider module. The default mode, on one host, works without it |
| A sign-up page | There is none on purpose. The first account is made on the server and every other comes from an invite |

## Known limits worth knowing

- The `terminal` switch stored in the instance settings is the admin's choice. Turning it off does not close a terminal
  that is already open; that one runs until its page closes, and the admin page says so.
- Firefox and WebKit ignore an iframe's `allow` list for the clipboard, and WebKit allows a shared worker in a
  sandboxed frame. The terminal frame removes both itself, before any other script runs.
- Cache and artifact access follow the run: a job reads its own run's artifacts and the caches of its own branch, the
  default branch or, for a pull request, the base branch. A pull request from a fork reads and never writes.
