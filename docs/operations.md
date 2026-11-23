# Running an instance

What an operator does after the first run (`docs/integration.md`): look after the settings, take
backups, restore, upgrade and roll back. Every command runs on the host, as root, from the
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

## The proxy

| `CODEHAVEN_PROXY` | What you get |
| --- | --- |
| `none` | The web server and ssh on the addresses in `CODEHAVEN_HTTP_BIND` and `CODEHAVEN_SSH_BIND`, loopback by default. No TLS. For a trial, or for a proxy that runs on the host |
| `network` | The server joins the existing Docker network `CODEHAVEN_PROXY_NETWORK` under the alias `CODEHAVEN_PROXY_ALIAS`. Your proxy sends to `<alias>:3200` and you add that site yourself. Codehaven changes nothing in your proxy |
| `caddy` | A Caddy in the same project gets the certificate for `CODEHAVEN_SITE_HOST` and sends to the server. It publishes ports 80 and 443, so the name must point at this machine and both ports must be free |

Set `CODEHAVEN_TRUSTED_PROXIES` to the network your proxy reaches the server from (for `caddy`,
`CODEHAVEN_EDGE_SUBNET`). Only a peer inside it may set `X-Forwarded-For` and
`X-Forwarded-Proto`; with `[]` the server trusts no one and sees the proxy's address as the
client. Your proxy must send every path to the server (including `/.well-known/`), remove any
`Remote-User` style header from the client, and not buffer responses, or live CI logs stall.

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

## Restoring

Restore into an empty location. Make a new env file (or edit a copy) with a new
`CODEHAVEN_PROJECT`, `CODEHAVEN_DATA_DIR` and `CODEHAVEN_SECRETS_DIR`, and ports that do not clash
with a running instance, then:

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
the server applies new database migrations as it starts. The image tag
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

Do not restore the database for a rollback of an additive migration; the old code ignores what it
does not know. Restore from backup only when data itself was damaged. Take the next upgrade from
a clean `up.sh` without the variable, so migrations apply again.

## When something is wrong

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
