# Running an instance

What an operator does after the first run (`docs/integration.md`): look after the settings, take
backups, restore, upgrade and roll back. Every setting is explained in `docs/configuration.md`, and
`docs/troubleshooting.md` lists the failures you are likely to meet with the command to run for each. Every command runs on the host, as root, from the
checkout. The scripts read one env file; copy `.env.example` to `.env` to make it, or keep it
next to the checkout as `../codehaven.env`, or point `CH_ENV_FILE` at it. Use one place only: with
two candidates the scripts stop and say so.

## The settings

`.env.example` is the list, with a comment on each setting. The scripts parse the file and never
run it, so a value cannot hold a command, and a variable set in the shell wins over the file. A
required setting that is missing stops with its name. The file holds no secrets: the database
password lives in `CODEHAVEN_SECRETS_DIR`, written once by `setup.sh`, and the server's master key
lives in `CODEHAVEN_DATA_DIR/secrets`, made by the server on first start.

Always go through `deploy/compose.sh` to talk to the instance (`deploy/compose.sh ps`,
`deploy/compose.sh logs server`, `deploy/compose.sh exec server codehaven doctor`). It loads the
env file, works out the host's public addresses, adds the proxy file for `CODEHAVEN_PROXY` and
passes the project name. Calling `docker compose` on `deploy/compose.yml` directly skips all that.

## Public repositories

Public repositories are off until a site admin turns them on, at `/admin/settings` or with:

```sh
deploy/compose.sh exec server codehaven admin public-repos status
deploy/compose.sh exec server codehaven admin public-repos on
deploy/compose.sh exec server codehaven admin public-repos off
```

Turning the switch off lowers every public repository to internal: readable by the signed-in
accounts of this server and nobody else. Turning it on makes nothing public by itself; each
repository is widened by its owner on the repository's visibility settings page. When the
three tiers arrived, repositories that used to be called public became internal; nothing
became public on its own.

Before any widening the server rebuilds the repository's git store from its refs, so deleted
history can no longer be fetched. The old store is kept unreachable for 7 days under
`data/git/_prepublish` and then removed.

## Email

Codehaven sends no mail until `[mail.smtp]` is set in `codehaven.toml` (see
`deploy/codehaven.example.toml`), or the same keys as `CODEHAVEN__MAIL__SMTP__HOST` and so on.
The stock compose file passes neither to the server, so on a `deploy/prod` install you set them
with an extra compose file (`docs/configuration.md`, "Settings the env file does not carry").
With it set, three things are mailed: a reset link when someone uses "Forgot your password", an
invite a site admin makes for an email address, and notifications for the people who turn email
on at Settings, Notifications (off until chosen; each mail holds a title and a link, never the
text of a comment, and a failed send is tried again up to five times).

- TLS is required. `tls = "starttls"` is the default, `tls = "tls"` is for port 465, and
  `tls = "none"` is accepted only when `host` is `localhost`, `127.0.0.1` or `::1`.
- The password lives in the file `password_file` names, read once at start. A username without a
  password file stops the server at start with a message.
- A reset link goes only to the account's primary email, works for an hour and once, and at most
  one is sent per account every ten minutes. Asking for one signs nobody out. Using it signs the
  account out everywhere.
- The answer on the page is the same whether or not an account matched, and it is given before
  the mail goes out.

## The proxy

| `CODEHAVEN_PROXY` | What you get |
| --- | --- |
| `none` | The web server and ssh on the addresses in `CODEHAVEN_HTTP_BIND` and `CODEHAVEN_SSH_BIND`, loopback by default. No TLS. For a trial, or for a proxy that runs on the host |
| `network` | The server joins the existing Docker network `CODEHAVEN_PROXY_NETWORK` under the alias `CODEHAVEN_PROXY_ALIAS`. Your proxy sends to `<alias>:3200` and you add that site yourself. Codehaven changes nothing in your proxy |
| `caddy` | A Caddy in the same project gets the certificate for `CODEHAVEN_SITE_HOST` and sends to the server. It publishes ports 80 and 443, so the name must point at this machine and both ports must be free |

Set `CODEHAVEN_TRUSTED_PROXIES` to the network your proxy reaches the server from (for `caddy`,
`CODEHAVEN_EDGE_SUBNET`). Only a peer inside it may set `X-Forwarded-For` and
`X-Forwarded-Proto`; with `[]` the server trusts no one and sees the proxy's address as the
client. On a Docker network shared with other containers, name the proxy instead: set
`CODEHAVEN_TRUSTED_PROXY_HOSTS` to its container name, for example `'["caddy"]'`, and
`CODEHAVEN_TRUSTED_PROXIES` to `[]`. Then only that container's address is trusted, looked up
again every 30 seconds, and a neighbour on the network cannot pick its own address for the rate
limits or the audit log. Your proxy must send every path to the server (including `/.well-known/`), remove any
`Remote-User` style header from the client, and not buffer responses, or live CI logs stall.

## Pages

Codehaven Pages (`docs/design/pages.md`) is off until `CODEHAVEN_PAGES_HOST` names a host, for
example `pages.example.com`. Point that name at the server, then give the proxy a site for it that
drops `Cookie`, `Authorization` and identity headers on the way in and `Set-Cookie` on the way out
(`deploy/Caddyfile.example`). The server does the same, and answers every request for that host
itself, before any app route. The host must not be the app's host or a parent of it; the server
refuses to start otherwise.

Full mode (a site admin's switch at `/admin/pages`) also serves `*.pages.example.com`. Turn it on
only after the proxy has a wildcard certificate for that name, which needs a DNS challenge.

## Runners

After a deploy, recreate the runner on the new image and check `codehaven admin runners`: one online runner,
and no second registration with the same name that has stopped calling in (revoke it with `revoke-runner`; the
web page at Site admin > Runners does the same). A revoked runner stays in the table with a revoked time, which is
the record that it was removed.

`codehaven admin runners` lists the registered runners and when each last called in.
`codehaven admin revoke-runner <id>` removes one: its credential stops working at once. Both run
inside the server container, like the other admin commands:

```sh
docker compose -p <project> exec server codehaven admin runners
docker compose -p <project> exec server codehaven admin revoke-runner 1
```

## Codehaven's own repository and CI

Codehaven can build itself (ch-d9s). `.codehaven/workflows/ci.yml` runs `make ci CH_NATIVE=1`
in the builder image with a Postgres service beside it, which is the same gate `make ci` runs
on a build host. Once, on the instance that should run it:

1. Load the builder image into the runner's Docker daemon: `CODEHAVEN_CI_BUILDER=1 bash
   deploy/prod/ci-up.sh`. It is a few GiB in that daemon's storage.
2. Make sure the account that will own the repository may run CI (`can_run_ci`; Site admin >
   Users, or `codehaven admin create-user --can-run-ci` for a new one).
3. Create an empty private repository for it in the browser (New repository, private, no
   README), and a personal access token with the `repo` and `workflow` scopes (Settings >
   Tokens). The `workflow` scope is what lets a push carry `.codehaven/workflows/`.
4. From a checkout of the source: `git remote add haven https://<host>/<owner>/codehaven.git`
   and `git push haven main`, with the token as the password. Nothing is copied from anywhere
   else.
5. The push starts the `ci` workflow on the instance's runner. Later pushes and pull requests
   run it again.

Without a password or a token: a person with root on the host can do steps 3 and 4 inside the
server container with the admin command, which makes a private repository for an existing
account (it never overwrites, and is audited as `repo.imported_from_server_path`) from a bare
git repository at a path in the container, and with `--run-ci` starts the workflows once:

```sh
git clone --bare <the source checkout> /tmp/codehaven-src.git          # on the host
docker cp /tmp/codehaven-src.git <server container>:/tmp/codehaven-src.git
docker exec <server container> codehaven admin repo-import --owner <account> --name codehaven \
  --from /tmp/codehaven-src.git --run-ci
docker exec <server container> rm -rf /tmp/codehaven-src.git
```

The job takes the source from the instance, so its address must be one a job may reach: the
public https name does (the runner's Docker daemon lets jobs open tcp/443 to it and nothing else on
the host). The Postgres service image is pulled from Docker Hub by that daemon the first time.

Proof, on a throwaway stack (no account, repository or token of a real instance was used): the e2e
harness's step 26 creates a repository, pushes the whole source of a checkout, and waits for this
workflow to finish on the stack's own runner. On 2026-11-25 it took 13.7 minutes and `make ci`
passed, 2135 of 2135 tests:

```sh
bash deploy/dev/host.sh <stream> 'CH_STREAM=<stream> bash deploy/dev/release.sh shots-mine'
bash deploy/dev/host.sh <stream> 'SHOTS_STEPS=0-4,26 SHOTS_DOGFOOD=1 SHOTS_NO_SHOTS=1 bash deploy/dev/shots.sh --tag shots-mine'
```

`SHOTS_JOBS_REACH_SERVER=1` is the same fixed address and guard exception without the builder and Postgres
images. The real-actions steps (35 and 36) need it, and the internet, because the server fetches each action from
GitHub: `SHOTS_STEPS=0-4,35,36 SHOTS_JOBS_REACH_SERVER=1 SHOTS_NO_SHOTS=1 bash deploy/dev/shots.sh --tag shots-mine`.

Step 38 (the acceptance rows step 35 does not run: setup-java, setup-go, setup-ruby, setup-dotnet and
rust-toolchain, a 1 GiB artifact, a Postgres service reached by port and by name with a `nuget push` using the job
token, the cache read order including a fork's pull request, ncipollo/release-action and
peter-evans/create-pull-request) runs the same way and needs the same two things: `SHOTS_STEPS=0-4,38`. The
throwaway server gets 5 GiB of memory for it, because its data directory is memory there.

`SHOTS_DOGFOOD=1` loads the builder and Postgres images into the stack's daemon, gives the server a
fixed address on the runners network (`deploy/compose.shots-dogfood.yml`) and tells the daemon's
guard (`GUARD_ALLOW`, never set by the production compose file) to let jobs reach that one address
and port, because a throwaway server has no public https name.

## The access matrix

`tests/access/` holds what each credential reaches, as committed tables. The signed-out tables
(`anonymous.*.txt`) are checked by `public_surface.rs`. Every other credential has one
file (`matrix.<credential>.txt`), checked by `access_matrix.rs`: one row per route and method with six
cells, for a private, an internal and a public repository, each with the owner's contribution limit off and on.
`.` is refused, `v` is answered but the request was not valid, `r` is answered and no row or ref changed, `w` is
answered and a row or ref changed. `matrix.routes.txt` lists every route the router registers, and a route
that is not in it fails the build, so a new route cannot arrive without someone reading what each credential does
with it.

When a route changes on purpose, regenerate the tables and read the diff before committing it:

```sh
CH_FETCH=tests/access/.gen bash deploy/dev/remote.sh <stream> \
  'CH_MATRIX_OUT=/work/tests/access/.gen cargo nextest run -p codehaven --test access_matrix --no-fail-fast'
python3 tests/access/merge_matrix.py tests/access/.gen
```

Besides the tables, the test fails whatever they say when a request answers 5xx, when a GET changes a row, when an
account with no grant changes anything but issues, comments, reviews, stars and watches, when the contribution limit
does not hold, when a read-only credential writes, and when a private repository answers a credential with no right
to it differently from one that does not exist.

## Backups

```sh
sudo bash deploy/prod/backup.sh
```

It prints the path of one archive under `CODEHAVEN_BACKUP_DIR` (which must be outside the
checkout, mode 0600). The archive holds a `pg_dump` of the database, the git repositories, blobs
and logs, the server's master key, the database credentials and the env file. That makes it as
sensitive as the instance: keep it where only you can read it, and encrypt it before it leaves
the machine. Without the master key, stored secrets cannot be read, which is why it is in the
archive.

The server is stopped while the files are copied, so the database and the repositories match,
and it is started again whatever happens. Expect a pause of seconds to minutes, depending on how
much data there is. Nothing else is stopped. If anything fails the archive is removed and the
script says so. Take backups on a schedule with cron or a systemd timer, and test a restore now
and then: a backup nobody has restored is a hope.

### A daily schedule

```sh
sudo bash deploy/prod/backup-timer.sh install
```

That installs a systemd timer that runs `deploy/prod/backup-daily.sh` once a day at 04:17
local time, up to ten minutes later, and catches up after the host was off. Each run takes one
backup and then prunes: the newest archive of each of the last 7 days and of each of the last
4 ISO weeks is kept, the rest is deleted, and nothing else in `CODEHAVEN_BACKUP_DIR` is ever
touched. The counts are settings: `CODEHAVEN_BACKUP_KEEP_DAILY` (default 7) and
`CODEHAVEN_BACKUP_KEEP_WEEKLY` (default 4).

Before the backup the run checks the free space on the disk behind `CODEHAVEN_BACKUP_DIR`.
When less than `CODEHAVEN_BACKUP_MIN_FREE_MB` (default 2048) is free, or less than three
times the newest archive, or less than twice the data it copies, the run is skipped and writes `SKIPPED` to `backup-daily.log` in
the backup directory, so a nearly full disk does not eat its own backups. Every line of the
run lands in that log.

```sh
sudo bash deploy/prod/backup-timer.sh status
```

shows when the timer fires next and the last 20 lines of the log.

The timer runs a copy of `deploy/` taken at install time, kept under
`CODEHAVEN_BUILD_ROOT/backup-tool`, so a checkout that is being synced or edited is never
what runs at night. Run `install` again after updating the checkout to refresh the copy.
`remove` uninstalls the timer and keeps every archive already taken.

### A copy off the host (optional)

Off by default: nothing is sent anywhere until `CODEHAVEN_BACKUP_OFFSITE` in the env file
names a destination. When it does, the daily run encrypts each archive with openssl
AES-256 and sends only the encrypted `.enc` file, because the archive holds the instance's
master key. Two forms:

- `rsync:<destination>`, for example `rsync:backup@example.com:/srv/codehaven-backups/`.
  Root's ssh key must already be trusted by the destination: the copy goes over BatchMode
  ssh, which never prompts for a password.
- `s3://<bucket>/<prefix>`, for any S3-compatible store. The `aws` CLI needs its own
  credentials for root; Codehaven never reads or stores them. When the store is not AWS's
  own S3, set `CODEHAVEN_BACKUP_OFFSITE_ENDPOINT` to its endpoint URL.

The passphrase lives in the file named by `CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE`:
root-only, mode 600, one passphrase. Write it once and keep a copy of it somewhere other
than this host, because without it the off-site copies cannot be opened. To open one on
another machine:

```sh
CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE=/path/to/passphrase \
  bash deploy/prod/backup-open.sh <name>.tar.gz.enc <name>.tar.gz
```

Each copy is encrypted and then authenticated: a 32 byte HMAC-SHA256 tag (made with a key
of its own, from the same passphrase) follows the encrypted data. `backup-open.sh` checks
the tag first and refuses a copy that was changed, cut short or sealed with another
passphrase, writing nothing. `restore.sh` takes a `.enc` copy directly and does the same
check before it touches anything. It needs only openssl and python3.

Codehaven never prunes the remote side: what the destination keeps is the operator's
retention policy. When a send fails, the run writes `OFFSITE FAILED` to `backup-daily.log`
and exits non-zero, while the local backup and its prune have already happened.

These archives otherwise stay on the same disk as the instance, so they do not survive
losing that disk, and a copy off the host is the way to change that.

## Restoring

Restore into an empty location. On a new host, use your env file as it is: install the checkout
and the env file first (the archive holds a copy as `codehaven.env.restored`). To restore beside a
running instance, make an env file with a new `CODEHAVEN_PROJECT`, `CODEHAVEN_DATA_DIR` and
`CODEHAVEN_SECRETS_DIR`, and ports that do not clash. The script refuses only when the project
already has containers or a directory is not empty. Then:

```sh
sudo bash deploy/prod/restore.sh <archive>
sudo bash deploy/prod/up.sh
```

The script refuses when the project already has containers, or when any data directory, the
secrets directory or the database directory holds a file. It never overwrites and never deletes.
The env file saved in the archive is unpacked next to the archive as `codehaven.env.restored` for
you to compare with yours; it is not applied. To replace a broken instance in place, move its
directories aside first (do not delete them until the restored one is checked).

## Upgrading

```sh
sudo bash deploy/prod/backup.sh
git pull
sudo bash deploy/prod/up.sh
```

`up.sh` builds images for the new commit when it has none, then starts the instance on them, and
the server applies new database migrations as it starts.

Before it changes anything, `up.sh` dumps the database to `CODEHAVEN_BACKUP_DIR` as
`pre-deploy-<project>-<time>-<tag>.dump` (the newest 5 are kept) and tags the image the server runs
now as `codehaven-server:<project>-previous`. After the deploy the server has to be healthy within
`CODEHAVEN_DEPLOY_WAIT` seconds (default 300) and `deploy/dev/public-smoke.sh` has to pass against
`CODEHAVEN_SMOKE_URL` (default `http://CODEHAVEN_HTTP_BIND`). If either fails, `up.sh` starts the
server again on the previous image, without migrating, and exits with an error that says so. The
database is never rolled back automatically: the old server runs on it as the new one left it, and
the dump is there if the data itself needs to go back (see Restoring). Until a later `up.sh`
succeeds, every `deploy/compose.sh` call (a restart, `ci-up.sh`) keeps running the rolled back
images without migrating, and says so: the mark is `<CODEHAVEN_BUILD_ROOT>/run/<project>.rolled-back`.
Deploy a fixed version with `up.sh` to clear it. The image tag
is `CODEHAVEN_TAG` when you set it, otherwise the short id of the checked-out commit, so every
version has its own tag and the previous images stay on the host.

## Rolling back

A newer version may have added tables that an older one does not know. The older server then
refuses `serve --migrate`, because the database holds migrations it cannot find. It runs fine
on that database when it is not asked to migrate, which `CODEHAVEN_NO_MIGRATE=1` does:

```sh
git checkout <the older commit>
CODEHAVEN_NO_MIGRATE=1 sudo -E bash deploy/prod/up.sh
```

Migration 0139 added `issues.pin_position` with a CHECK that ties it to `pinned`. The image from
before it pins and unpins with the flag alone, so migration 0140 adds a trigger that fills the
position in (last) when an older writer pins, and clears it when it unpins. Rolling back to the
C9 image therefore leaves pin and unpin working. Deploy 0140 before relying on a rollback to a
version older than C10: on a database that has 0139 but not 0140, pin and unpin on that older image
answer 500 until 0140 is applied.

Migrations 0141 (environments) and 0142 (merge queue) are additive too. On the C10 image a workflow
with `environment:` is refused as before, so a job that waited at an environment never starts there, and
a repository that turned its merge queue on merges the ordinary way again (the older code does not know
the queue exists), with whatever waited in it left in place until the queue code returns.

Migrations 0143 to 0146 (repository access holds, a package orphan reason, passkeys, and the latest-release mark) only add
tables and columns too. An image from before 0145 does not know passkeys, so passkeys people registered are kept in the database
but that image does not offer them.

Do not restore the database for a rollback of an additive migration; the old code ignores what it
does not know. Restore from backup only when data itself was damaged. Take the next upgrade from
a clean `up.sh` without the variable, so migrations apply again.

## When something is wrong

`docs/troubleshooting.md` has the long list. The short one:

| Symptom | Look at |
| --- | --- |
| A script stops with a setting's name | That key is missing from the env file. `.env.example` has a comment for it |
| `up.sh` says the server is not healthy | `deploy/compose.sh logs server`, then `deploy/compose.sh exec server codehaven doctor` |
| Sign-in forms say the origin is wrong | `CODEHAVEN_EXTERNAL_URL` must be exactly what people type, with no path and no trailing slash |
| Everyone appears to come from one address | `CODEHAVEN_TRUSTED_PROXIES` does not include the network your proxy uses |
| Live CI logs arrive all at once | The proxy buffers. Turn buffering off for the server |
| A build stops on disk | `CODEHAVEN_DISK_LIMIT` was reached. `bash deploy/dev/clean.sh` removes what builds made |
| Docker says "all predefined address pools have been fully subnetted" | The host has no free range for the two networks the instance makes. Give them fixed subnets in an extra compose file and point `CODEHAVEN_COMPOSE_EXTRA` at it (the example below) |

### Fixed subnets for the instance's networks

Write a file such as `/srv/codehaven/compose.extra.yml`, using two ranges nothing else on the host uses:

```yaml
networks:
  backend:
    ipam:
      config:
        - subnet: 10.20.40.0/24
  runners:
    ipam:
      config:
        - subnet: 10.20.41.0/24
```

Then set `CODEHAVEN_COMPOSE_EXTRA=/srv/codehaven/compose.extra.yml` in the env file and run `sudo bash deploy/prod/up.sh` again. If the networks were half made, remove them first with `docker network rm <project>_backend <project>_runners`.

If the first admin is lost, sign in with its recovery codes. There is no script that resets
accounts or empties an instance, and none is needed.
