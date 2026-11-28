# Troubleshooting

Real failures, what they mean and the command to run. Commands run on the host, from the checkout, as root, unless they say otherwise. When you do not know where to start, run these three and read what they say:

```sh
deploy/compose.sh ps                                      # which containers run, and their health
deploy/compose.sh logs --tail 100 server                  # why the server stopped or refused something
deploy/compose.sh exec server codehaven doctor            # checks the installation, one line each
```

## Installing and starting

### A script stops with "X is not set"

That key is missing from the env file. `.env.example` has a comment for it, and [Configuration](configuration.md) explains each one. Nothing falls back to a guess. A setting that is correct in your shell but missing from the file also fails: set it in the file.

### "no env file" or "two env files found"

Copy `.env.example` to `.env` in the checkout. If both `.env` and `../codehaven.env` exist, keep one, or choose with `CH_ENV_FILE=/path/to/file`.

### "cannot read this line (expected KEY=value)"

The env file is parsed, not run. A line must be `KEY=value` with an upper case name. `export KEY=value`, lower case names and spaces before the name are refused.

### `setup.sh` stops on the host

| Message | Fix |
| --- | --- |
| `'<tool>' is not installed` | Install it: docker, git, openssl, curl, python3, or iproute2 for `ip` |
| `cannot talk to docker` | Start Docker, and run as root or a member of the `docker` group |
| `Docker Compose X is too old` | Install Compose 2.24 or newer: `docker compose version` |
| `docker buildx is not installed` | Install the Docker buildx plugin |
| `this host has less than 4 GB of memory` | Add memory or swap. The first build compiles the server |
| `less than 20 GB free under <dir>` | Free space, or point `CODEHAVEN_DATA_DIR` at a bigger disk |
| `run this as root (sudo)` | Run it with `sudo`. It makes directories owned by the server's user |
| `CODEHAVEN_BACKUP_DIR is inside the checkout` | Move backups elsewhere, such as `/srv/codehaven/backups` |
| `the docker network '<name>' does not exist` | With `CODEHAVEN_PROXY=network`, create it or fix `CODEHAVEN_PROXY_NETWORK` |
| `another container already has the alias` | Pick another `CODEHAVEN_PROXY_ALIAS` |

### "diskcheck: / is at 87% (limit 85%)"

Builds and `setup.sh` stop when a watched disk is nearly full. Free space, or change `CODEHAVEN_DISK_LIMIT` or `CODEHAVEN_DISK_PATHS`. `bash deploy/dev/clean.sh` removes dangling Codehaven images and build cache older than a week. Every upgrade keeps the earlier images for rollback, so over time list them and remove ones you will not roll back to: `docker image ls 'codehaven-*'`, then `docker image rm codehaven-server:<old tag> codehaven-runner:<old tag>`. Keep the images tagged `<project>-previous`.

### "address already in use" when starting

Something else holds port 3200 or 2222 (or 80 and 443 with Caddy). Find it with `ss -tlnp | grep -E ':(3200|2222|80|443) '`, then change `CODEHAVEN_HTTP_BIND` or `CODEHAVEN_SSH_BIND`, or stop the other service.

### "Docker says all predefined address pools have been fully subnetted"

The host has no free range for the two networks the instance makes. Give them fixed subnets in an extra compose file and point `CODEHAVEN_COMPOSE_EXTRA` at it:

```yaml
# /srv/codehaven/compose.extra.yml
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

Use two ranges nothing else on the host uses. If the networks were half made, remove them first: `docker network rm <project>_backend <project>_runners`. Then run `sudo bash deploy/prod/up.sh` again.

### `up.sh` says "the server did not become healthy"

Read the server's log: `deploy/compose.sh logs --tail 100 server`. The server checks its configuration at start and names the setting it dislikes. The common ones:

- `server.external_url must be an origin with no path and no trailing slash`: fix `CODEHAVEN_EXTERNAL_URL`.
- `mcp.enabled needs an https server.external_url`: turn `CODEHAVEN_MCP_ENABLED` off, or use an `https` address.
- `pages.host ... neither the app's host nor a parent of it`: choose another `CODEHAVEN_PAGES_HOST`.
- `http.trusted_proxies must not contain 0.0.0.0/0`: list the real proxy network.
- A database or permissions error: run `deploy/compose.sh exec server codehaven doctor`, and check that `CODEHAVEN_DATA_DIR` still belongs to uid 10001 (`sudo bash deploy/prod/setup.sh` repairs it).

### `up.sh` says "the smoke check failed"

The server is healthy but an outside check failed against `CODEHAVEN_SMOKE_URL` (by default `http://` plus `CODEHAVEN_HTTP_BIND`). The failing lines are printed, and the whole log is `$CODEHAVEN_BUILD_ROOT/run/up-smoke.log`. Run it by hand to see them all: `bash deploy/dev/public-smoke.sh http://127.0.0.1:3200`.

### "the deploy failed and the server was rolled back"

`up.sh` started the server again on the previous image, without migrating. The database was not changed back, and the dump taken before the deploy is named in the message (`pre-deploy-<project>-<time>-<tag>.dump` in `CODEHAVEN_BACKUP_DIR`). Until a later `up.sh` succeeds, every `deploy/compose.sh` call keeps the rolled back images and says so. The mark is the file `$CODEHAVEN_BUILD_ROOT/run/<project>.rolled-back`. Fix the cause, then run `sudo bash deploy/prod/up.sh` to deploy again and clear it.

### "port N is bound beyond loopback but 127.0.0.1:N was asked for"

A published port is reachable from outside although the setting says loopback. Look at what else changed the Docker or firewall setup, and `ss -tln`.

### Caddy does not get a certificate

`deploy/compose.sh logs caddy`. The name in `CODEHAVEN_SITE_HOST` must point at this machine, and ports 80 and 443 must be free and open in your firewall. The certificate is requested when Caddy first starts.

## Signing in

### "Request blocked: the origin does not match this server."

`CODEHAVEN_EXTERNAL_URL` must be exactly what people type in the browser: the scheme, the host and the port, no path, no trailing slash. A trial on `http://127.0.0.1:3200` is a different origin from `http://localhost:3200`. Fix the setting and run `sudo bash deploy/prod/up.sh`.

### Sign-in works on one address and not another, or on `http` after you set `https`

With an `https` external URL the session cookie is a `__Host-` cookie, which browsers do not send over plain `http`. Open the site at the address in `CODEHAVEN_EXTERNAL_URL`.

### The first admin's link expired before it was used

`first-admin.sh` refuses to run again, because the account exists. Make a new link for it: `deploy/compose.sh exec server codehaven admin reset-password alice`.

### Everyone appears to come from one address, or one person's lockout locks everyone

`CODEHAVEN_TRUSTED_PROXIES` does not include the network your proxy reaches the server from, so the server sees the proxy's address as every client's. Set it to the proxy's network (for Caddy, `CODEHAVEN_EDGE_SUBNET`) and run `up.sh` again. Check what the server sees in `deploy/compose.sh logs server`.

### "Too many attempts. Try again in N minutes."

The account or address is locked. A site admin clears an account's lockout at Site admin, Users, Unlock, or: `deploy/compose.sh exec server codehaven admin unlock <username>`.

### Passkeys say "Passkeys need this site to be opened by a domain name over https"

A passkey belongs to a host name. An IP address cannot be one. Set `CODEHAVEN_EXTERNAL_URL` to an `https` name.

### The admin pages say "Turn on two-factor sign-in"

Site admins need two-factor before the admin pages open. Settings, Two-factor and security.

### "Forgot your password" says the server does not send email

No mail is set up. Ask a site admin for a reset link, or set up mail ([Configuration](configuration.md), "Settings the env file does not carry"). If mail is set up and nothing arrives, read `deploy/compose.sh logs server` for `mail` lines. A reset link goes only to the account's primary email.

### "This invite link doesn't work any more"

It was used, revoked or expired, or it was never valid. All four look the same on purpose. Make a new invite.

## Git

### Push says "use a personal access token as the password"

The account has two-factor on, so its password does not work for git. Make a token (Settings, Access tokens, with the Repositories scope) and use it as the password.

### Push says a token needs the workflow scope

git prints, after `remote:`, "This changes .codehaven/workflows, which runs as CI. A token needs the workflow scope to do that." (the folder name is the one you changed). Make a new token at Settings, Access tokens with the Workflows scope as well as Repositories, and push with it.

### Push to a branch is refused

A protection rule covers it: direct pushes, force pushes and deletion are refused on a protected branch. Push to a new branch and open a pull request, or change the rule at Settings, Branches. The default branch can never be deleted.

### Push is refused because the disk is low

Uploads and pushes are refused when they would leave the data disk with less than 10 GiB free (`storage.min_free_gib`). Free space under `CODEHAVEN_DATA_DIR`.

### A push that worked before now fails with 401 or "Authentication failed"

The access token expired (90 days by default) or was deleted. Make a new one at Settings, Access tokens and use it as the password. Git may keep sending the old one from its credential store: remove it with `git credential reject` (give it `protocol=https`, `host=code.example.com` and a blank line), or edit the store.

### SSH says "Permission denied (publickey)"

The key is not added to the account, or the client offers another key. Add the public key at Settings, SSH keys, then test:

```sh
ssh -T -p 2222 git@code.example.com
```

A good key prints "Hi <name>! You have signed in, but Codehaven does not give shell access. Use git over SSH." If the connection never opens, check `CODEHAVEN_SSH_BIND` (loopback only means other machines cannot connect), your firewall, and that you use the port in the clone address.

### `public-smoke.sh` prints FAIL

Each `FAIL` line says what was wrong. The common ones: "https /healthz returned N (TLS or routing problem)" means the proxy, the certificate or DNS is not right, so check `deploy/compose.sh logs caddy` or your proxy; "missing header ..." means a proxy removed a security header the server sends; "Remote-User gave N" or "/signup" lines mean something in front of Codehaven is handling identity or sign-up, which it must not; "/login is not 200" means the server is not answering that address. Run it against the server directly (`bash deploy/dev/public-smoke.sh http://127.0.0.1:3200`) to tell a server problem from a proxy problem.

### The SSH clone address is wrong

Set `CODEHAVEN_SSH_EXTERNAL` to the host and port people really connect to, and run `up.sh` again.

## CI

### The Actions tab says "Workflows cannot run here yet"

The account that owns the repository may not run CI. A site admin chooses Allow CI at Site admin, Users. Check with `deploy/compose.sh exec server codehaven admin users`.

### A job stays queued

No runner takes it. Check, in order:

```sh
deploy/compose.sh exec server codehaven admin runners   # is one online, and is it recent?
deploy/compose.sh --profile ci ps                       # are dind and runner up?
deploy/compose.sh --profile ci logs --tail 50 runner
```

If no runner is registered, register one ([Using CI](using-ci.md)). If the job's `runs-on` label is not one the runner offers, the job fails after 15 minutes and its page lists the labels that exist. After a deploy, recreate the runner on the new image with `sudo bash deploy/prod/ci-up.sh`, and remove a stale registration with `codehaven admin revoke-runner <id>`.

### A job fails at the checkout step

The job cannot reach the server. Jobs reach it at `CODEHAVEN_EXTERNAL_URL`, from a daemon that may reach the internet and tcp/443 on the host's public addresses and nothing else. A loopback address, a private address or a port other than 443 does not work. Use a public `https` name. The name must also resolve, from the host, to an address that is not in a private range (10/8, 172.16/12, 192.168/16, 169.254/16, 100.64/10), because jobs are blocked from those. Whether a job can reach the host's own public name depends on your network (some clouds do not loop traffic back to your own public address); the checkout step in [Getting started](getting-started.md) is the test, and this was not verified against any particular cloud.

### "runner register" fails

A registration token works once and lasts 60 minutes, so a failed or repeated attempt needs a new one (Site admin, Runners). A runner that is already registered needs `--replace`: `HAVEN_RUNNER_TOKEN=<token> sudo -E bash deploy/prod/ci-up.sh register runner-1 --replace`. The message "set HAVEN_RUNNER_TOKEN to the registration token" means the variable did not reach the script: use `sudo -E`.

### Live logs arrive all at once

The proxy buffers. Turn buffering off for the server. With Caddy that is `flush_interval -1` on the `reverse_proxy`, as in `deploy/Caddyfile.example`.

### A run says "Waiting for approval"

The pull request comes from a fork, or a connected app started the run without the workflows permission. Someone with write access opens the run and chooses Approve and run, or cancels it. Each new commit needs its own approval.

### A step fails with an action that is not allowed

The site's action policy blocks it. A site admin changes it at Site admin, Actions, or accepts the one-click offer on the failed run.

### A step fails on `docker build` or `docker login`

Jobs have no Docker daemon by design. Use a `docker://image` step or a `run:` step. See [Using CI](using-ci.md), "What is not supported".

### The runner stops taking jobs

Its disk guard tripped: a watched disk is over `disk.max_percent` (85) and pruning images was not enough. Free space on the disk that holds `CODEHAVEN_DATA_DIR`. The runner already prunes unused images older than 7 days and never prunes the job image. If you remove the job image by hand, `sudo bash deploy/prod/ci-up.sh` loads it again.

## Features

### The connector cannot be reached or will not finish connecting

`deploy/compose.sh exec server codehaven doctor` shows whether it is on and its address. The address must be public `https` and `/mcp` must not redirect to another host. The troubleshooting table in [The Claude connector](connector.md) lists the rest.

### Pages shows no tab, or a site answers 404

No Pages tab means `CODEHAVEN_PAGES_HOST` is empty. A 404 on the site means the proxy has no site for the pages host, DNS does not point at the machine, or the repository has not published a copy yet (Settings, Pages). See [Pages](pages.md).

### The workspace editor or terminal does not start

Open Codehaven over `https` at a host name. A proxy that rewrites `Content-Security-Policy` or adds a `frame-ancestors` rule breaks the terminal. A site admin may have turned the editor or terminal off at Site admin, Workspace. See [The browser workspace](workspace.md).

### `docker login` or `dotnet nuget` answers 401

Use an access token with the Packages scope as the password, and the host name only for Docker (`code.example.com`, no `https://`). A package you may not read looks like one that does not exist.

### A webhook never arrives

The address must be public. Deliveries to private, loopback and link-local addresses are refused unless the operator allowed them. A failed delivery is tried six times in all, over roughly eight and a half hours.

## Backups and restore

### `backup.sh` fails

| Message | Fix |
| --- | --- |
| `run this as root (sudo)` | Use `sudo`. The repositories belong to the server's user |
| `CODEHAVEN_BACKUP_DIR is inside the checkout` | Put it outside |
| `postgres is not running, so there is nothing to dump` | Start the instance: `sudo bash deploy/prod/up.sh` |
| `the archive does not match its own checksums` | The disk may be failing or full. Nothing was kept. Check `df -h` and try again |

The server is started again whatever happens.

### The daily backup says SKIPPED or OFFSITE FAILED

`sudo bash deploy/prod/backup-timer.sh status` shows the schedule and the last lines of `backup-daily.log`. SKIPPED means the backup disk is too full: free space, or lower `CODEHAVEN_BACKUP_KEEP_DAILY`. OFFSITE FAILED means the encrypted copy did not go: the local backup is fine. Check the destination, root's ssh key (`rsync:`) or `aws` credentials (`s3://`), and that the passphrase file exists with mode 600 or 400.

### `restore.sh` refuses

| Message | Fix |
| --- | --- |
| `project '<name>' already has containers` | Restore into a new project name, or on a host where it does not run |
| `<dir> is not empty` | Restore into an empty location. Move the old directories aside, do not delete them until the restored instance works |
| `the archive does not match its checksums` | The archive is damaged. Use another one |
| `the off-host copy was refused` | Wrong passphrase, or the copy was changed or cut short. Open it by hand with `backup-open.sh` to see the reason |

## Asking for help

Include the output of `deploy/compose.sh ps`, the last 100 lines of `deploy/compose.sh logs server`, the output of `codehaven doctor`, the commit you run (`git rev-parse --short=12 HEAD`) and the exact message. Remove addresses and names you do not want public first. Report bugs on the project's issue tracker.
