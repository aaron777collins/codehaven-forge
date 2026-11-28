# Site admin

What a site admin can do, in the browser and on the server. A site admin manages the whole instance, not any one repository.

## Getting in

Site admin pages are at `/admin`. They need an account with the admin role that has two-factor sign-in on, and a browser session. Until two-factor is on the pages say "Turn on two-factor sign-in before using the admin pages." An access token never carries site admin powers, whatever its scopes. A site admin has no extra power over a repository unless they have two-factor on and are in a browser session.

The first admin is made on the server (see [Getting started](getting-started.md)). To make another, make an invite with the admin option ("They can manage the whole server"), or on the server run `codehaven admin create-user --username <name> --email <address> --admin`.

## The pages

| Page | What it does |
| --- | --- |
| Overview | People, organizations, repositories, runners online, queued jobs, the server version and the data disk |
| Invites | Make, list and revoke invites. An invite can name an email, an organization and team, grant admin, grant the right to run CI, and carry a note. It works once and lasts 1 hour to 30 days |
| Users | Search people. See each one's roles (Admin, Can run CI, Two-factor, Suspended). Suspend or unsuspend, Unlock a locked account, Sign out everywhere, Reset password (prints a one-time link), and Allow CI or Stop CI |
| Runners | Registered runners with labels, status, version, last seen and scope. Make a one-use registration token. Remove a runner, which stops its credential working at once |
| Actions | The policy for which actions workflows may use: none, selected or all public actions, an allow list, a block list that always wins, full SHAs only, and whether repositories on this instance may be used |
| Audit log | Every privileged action, with who, what, the target, the address and the outcome. Filter by actor id, action and address |
| Settings | Allow public repositories |
| Workspace | Turn the browser editor and the terminal on or off |
| Pages | Turn full mode on or off (needs `CODEHAVEN_PAGES_HOST` and a wildcard certificate) |

### Users

- **Suspend** ends the account's sessions, disables its tokens and stops its SSH keys. Nothing is deleted, and **Unsuspend** lifts it.
- **Reset password** prints a link that works once for an hour, signs the person out everywhere and revokes their access tokens. It does not turn two-factor off.
- **Allow CI** lets workflows of repositories the account owns run. Without it, pushes start nothing.
- There is no page or command to delete an account or to turn two-factor off for someone else. Both are deliberate.

### Public repositories

Public repositories are off until you allow them at Settings. Turning the switch on makes nothing public. Each owner widens a repository on its own settings page. Turning it off lowers every public repository to internal at once. Anyone on the internet can read a public repository, so decide who may make one: the account owner, an organization owner or a site admin with two-factor. See [Repositories](repositories.md).

### Runners

A registration token works once and lasts 60 minutes. A revoked runner stays in the table with a revoked time, as the record that it was removed. After a deploy, recreate the runner on the new image and check that one runner is online and there is no second registration with the same name that stopped calling in. See [Using CI](using-ci.md).

### The audit log

Security relevant actions write a row with the time, who did it, the action, its target, the client address and the outcome. Examples are two-factor changes, deployment approvals, issue deletes and transfers, package changes, repository imports, fork re-parenting, pull request merges, review dismissals, and every call a connected app makes. Filter by actor id, action and address.

## On the server

These commands run inside the server container, so only someone with root on the host can use them:

```sh
deploy/compose.sh exec server codehaven admin <command>
```

| Command | What it does |
| --- | --- |
| `users` | List accounts and their flags |
| `create-user --username U --email E [--admin] [--can-run-ci]` | Make an account with no password and print a one-time set-password link (1 hour) |
| `invite create [--email E] [--admin] [--can-run-ci] [--org O [--team T]] [--expires 7d] [--note N]` | Make an invite link. `--expires` is `1h` to `30d` |
| `invite list`, `invite revoke <id>` | See and end invites |
| `reset-password <user>` | Print a fresh one-time link, sign the account out everywhere and revoke its tokens |
| `unlock <user>` | Clear a sign-in lockout |
| `suspend <user>`, `unsuspend <user>` | Suspend or restore an account |
| `runner-token [--repo owner/name]` | Print a one-use runner registration token. With `--repo`, the runner serves only that repository |
| `runners` | List runners: id, name, status and when each last called in |
| `revoke-runner <id>` | Remove a runner. Jobs it holds fail when their lease runs out |
| `run-ci <owner/name> <workflow> [--branch B]` | Start a workflow by hand, as the owner would from the Actions tab. It needs a `workflow_dispatch` trigger. For checking that a runner still takes a job after a deploy |
| `repo-import --owner O --name N --from <path> [--run-ci]` | Make a private repository for an existing account from a bare git repository at a path inside the container. Never overwrites, and is audited |
| `public-repos status`, `on`, `off` | Whether repositories may be public. `off` lowers every public repository to internal |

Two other commands help:

```sh
deploy/compose.sh exec server codehaven doctor
deploy/compose.sh exec server codehaven migrate
```

`doctor` checks the installation and prints a line per check, marked `ok`, `warn` or `FAIL`: the git version (2.38 or newer), the data directories and whether they are writable, the git hooks, disk use (a warning at 85 percent, a failure at 90), the external URL and trusted proxies, the connector, the signed-out routes, the database, whether migrations are applied, and whether an account or organization is named `mcp` or `oauth`. `migrate` applies pending migrations and exits; `up.sh` does this for you as the server starts.

## Keeping an eye on it

- `deploy/compose.sh ps` shows the containers and their health. `deploy/compose.sh logs server` shows the server's log.
- `/healthz` on the public port answers `ok`. The server also has an internal listener on `127.0.0.1:9300` inside its container that serves `/readyz` and `/metrics`. It is loopback only and is not published.
- Watch the data disk. Uploads and pushes are refused below 10 GiB free by default, and builds and `setup.sh` stop when a watched disk is 85 percent full.
- Back up on a schedule and try a restore now and then (see [Running an instance](operations.md)).
