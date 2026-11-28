# Going public and first run

How a checkout of this repository becomes a running, public, invite-only instance: the first run
on a host, the first admin, inviting people, runners, and what to check from outside. If this is
your first time, `docs/getting-started.md` is the gentler walk-through, and `docs/configuration.md`
explains every setting. The lower
half of the page keeps the record of what the six stream merges settled and how the end-to-end
run and the screenshots work. `ARCHITECTURE.md` section 2.4 has the design of the public surface.

> The first half of this page is the short first-run checklist; [Getting started](getting-started.md) is the full walk-through and the one kept up to date. The lower half is contributor history of how the work was merged and tested.

## The first run on a host

The host needs Docker, Compose 2.24 or newer, buildx, git, openssl, curl, python3 and iproute2,
with 4 GB of memory and 20 GB free where the data lives. Everything else runs in containers, so
the host needs no Rust and no PostgreSQL.

1. Copy `.env.example` to `.env` (or to `../codehaven.env`, or point `CH_ENV_FILE` at it) and
   fill it in. No script carries a built-in host, domain, path or project name: a setting that is
   missing stops with its name. The example install root used here is `/srv/codehaven`
   (`CODEHAVEN_DATA_DIR`, `CODEHAVEN_SECRETS_DIR`, `CODEHAVEN_BACKUP_DIR`,
   `CODEHAVEN_BUILD_ROOT`), the project is `codehaven`, and the address people open is
   `CODEHAVEN_EXTERNAL_URL=https://code.example.com`.
2. `sudo bash deploy/prod/setup.sh`, once. It checks the host, makes the data directories owned
   by the server's unprivileged user (uid 10001), and writes the database credentials into
   `CODEHAVEN_SECRETS_DIR`, where they stay; it never prints them.
3. `sudo bash deploy/prod/up.sh`. (`make up` runs the same script through make, on another host over ssh when `CH_HOST` is set; the `sudo bash` form is the documented one.) It builds the
   images when the tag has none, starts the instance, waits for the server to be healthy, checks
   that the published ports are only where `CODEHAVEN_HTTP_BIND` and `CODEHAVEN_SSH_BIND` say
   (loopback by default), and curls `/healthz`.

A proxy in front is a setting, not a step: `CODEHAVEN_PROXY=none` for a trial on one machine,
`network` for a proxy you already run, or `caddy` for the Caddy that comes with Codehaven and
gets the certificate itself (`CODEHAVEN_SITE_HOST`). Only the networks in
`CODEHAVEN_TRUSTED_PROXIES` may set `X-Forwarded-For` and `X-Forwarded-Proto`. An upgrade is
`git pull` and `up.sh` again; the tag is the one you pass, `CODEHAVEN_TAG`, or the short commit
id of the checkout, never a generic name another stack might already run.

## The first admin

No account exists until you make one. This is deliberately a command on the server, never a web
page, so nobody can race you to it:

```sh
sudo bash deploy/prod/first-admin.sh alice alice@example.com
```

It prints a one-time link, valid for 1 hour: open it, choose a password, turn on two-factor.
The script refuses to run when the instance already has an account, and when the server runs
with `webhooks.allow_private` on, which is for test stacks only. It never deletes or resets
accounts; if the first account is lost, its recovery codes are the way back in. There is no
script that empties an instance, and none is needed.

## Inviting people

Every account after the first comes from an invite: Site admin > Invites, or

```sh
bash deploy/compose.sh exec server codehaven admin invite create --email bob@example.com
```

A link works once. A used, revoked, expired or made-up link all show the same plain "not found"
page, so a link cannot be probed for. The invitee picks a username and a password, and that is
the only way to get an account.

## Runners

CI needs a runner. Take a registration token from Site admin > Runners (one use), then on the
instance's host:

```sh
HAVEN_RUNNER_TOKEN=<token> sudo -E bash deploy/prod/ci-up.sh register runner-1
sudo bash deploy/prod/ci-up.sh           # the dind daemon, the job image inside it, the runner
```

Jobs run in the runner's own Docker daemon (`dind`), never the host's, and cannot reach the
host's addresses or each other (`docs/runner.md`). A runner on another machine needs Docker and
the `haven-runner` binary: `haven-runner register --url https://code.example.com --name <name>`,
then `haven-runner run`. Whether a repository may run workflows at all is a permission on its
owner, which an admin sets.

## What public means

The instance is invite-only. There is no sign-up page and no sign-up route, and no proxy-header
login: Codehaven checks every request itself, and an identity header from a proxy in front signs
nobody in. A signed-out visitor gets the sign-in page, or a 401 from the API, git, the registry
and the runner API. The one thing readable without an account is a public repository, and only
while the site admin allows public repositories (`docs/operations.md`).

## Checking it from outside

`bash deploy/dev/public-smoke.sh <base-url>` makes read-only requests and checks that TLS is
valid, `/login` carries every security header, every user, API, git, registry and
runner URL and every repository that is not public refuses a signed-out caller, that no
sign-up route exists in any spelling, and that an
identity header signs nobody in. Against the server itself, with no proxy in front:
`deploy/dev/public-smoke.sh http://127.0.0.1:3200`.

## Test stacks are throwaway

The end-to-end run and the screenshot run each build a stack of their own, in a throwaway compose
project with a throwaway image tag and a free subnet (`CODEHAVEN_E2E_SUBNET`):

```sh
make e2e E2E_TAG=test-x        # steps 0-1 by default, STEPS=0-19 for the whole run
```

The tag and the project name must be throwaway names (`test-`, `e2e-`, `shots-` or `tmp-` plus a
word), a project that already holds a container not started as a throwaway one is left alone,
and the harness itself refuses a stack that already holds an account. There is no flag that
relaxes any of this. The run tears its own project down at the end and never touches the
instance's project, volumes or data, so there is no reset step after it and no reset script.

## Backups

`sudo bash deploy/prod/backup.sh` stops the server just long enough to take a consistent copy
(the database, the repositories, blobs and logs, the master key, the database credentials and
the env file) into a 0600 archive under `CODEHAVEN_BACKUP_DIR`, which must be outside the
checkout, then starts the server again. The archive is as sensitive as the instance: keep it
where only you can read it, and encrypt it before it leaves the host. `deploy/prod/restore.sh`
brings an instance back from one.

## What the merges settled

The foundation, identity, git, web, collab, ci and runner streams were built apart and merged
into `main` in the order of ROADMAP.md section 5, each with `--no-ff`, with the whole workspace
test suite run in the builder after every merge (816, 1044, 1229 and 1510 tests green). This
section records what the merge itself had to settle, so the next person does not rediscover it.

| Clash | Settled how |
| --- | --- |
| Two `Directory` traits (identity's admin and email directory, the web's people lists) | The web's became `People` (`state.people`). Identity's stays `Directory` |
| Two `blame` methods on `GitService` | The git stream's `Blame` stays. The blame page turns it into the lines it draws |
| The web's `Repos::update` and the git stream's `update_settings` | `GitCore` implements both. `update` maps onto the settings update |
| Four traits the web called that no stream implemented (`Sessions`, `People`, `Tracker`, `Pipelines`) | Each is implemented beside the data it reads: `ch-auth/src/web.rs`, `ch-collab/src/web.rs`, `ch-ci/src/web.rs`. They call the real services, so permissions, audit records and events are the same ones the REST edge gets |
| A privilege change (turning two-factor on or off, a new password) ends every session and issues a new cookie | The web trait now hands the new cookie back and the pages set it in the same response. Before, the person was signed out |
| Two CSRF schemes (the request guard's and the web crate's own) | The guard checks every unsafe request, so pages embed the guard's token. The web crate keeps its own only when there is no guard (the demo and the page tests) |
| A right password on an account with two-factor, over git | The gate says "use a personal access token as the password" in the 401 body. Git prints it after `remote:` |
| `root` is a reserved username | The e2e admin is `ops`. `avatar` and `preview` are now reserved too, because `/{owner}` is a route |
| The dind daemon's certificate was only valid for its container id | `DOCKER_TLS_SAN: DNS:dind` in `deploy/compose.yml`, so the runner can connect by name |

## The end-to-end test

`tests/e2e/e2e_full.py` runs steps 0 to 19 of ROADMAP.md section 6 against a throwaway stack,
and step 20 leaves session cookies for the screenshots. It is standard library Python, so it
runs on the build host with no install. Its HTTP calls go to the server over the stack's own
network, where its address is a trusted proxy, so it can send `X-Forwarded-For` and act as many
client addresses (the rate limit and lockout steps need that). Real `git` over HTTP and SSH,
real `ssh-keygen`, TOTP codes computed in the script, HTML forms with CSRF, a local listener for
the webhook.

The Rust harness in `tests/e2e/src` runs steps 0 and 1 inside the same throwaway project. Steps
2 and later live in the Python script because they need `git`, `ssh` and a runner, and the e2e
compose project has neither a runner nor a Docker daemon for jobs.

Step 14 checks that a runner registration token works once and is refused the second time. The
OCI registry with a real `docker login` is covered by `deploy/tests/registry-conformance.sh`. The live job log is covered by step 21,
which runs `tests/e2e/livelog.mjs` in a real browser.

## Screenshots

`bash deploy/dev/shots.sh --tag shots-mine` builds a seeded throwaway stack of its own, runs the
end-to-end script through step 20 for a signed-in session, then shoots the pages with
`tests/e2e/shoot.mjs`: `playwright-core` with the system Chrome on the build host, 36 pages at
390 and 1440 pixels wide, light and dark, 144 images. The session cookie goes in as a header
because the server expects the `__Host-` cookie, which a browser will not send over plain http.
The env file needs `CODEHAVEN_E2E_SUBNET`, `CODEHAVEN_SHOTS_RUNNERS_SUBNET` and
`CODEHAVEN_SHOTS_NODE_MODULES` (a directory that holds `playwright-core`), and the images are
built first (`bash deploy/dev/release.sh shots-mine`).
