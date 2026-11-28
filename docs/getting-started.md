# Getting started

Codehaven is a self-hosted code forge written in Rust. It hosts git over HTTP and SSH, and has issues, pull requests with review, CI with registered runners, releases, packages, a container registry, static sites and a code editor in the browser. This page takes you from an empty Linux machine to a running instance with an admin account, a repository, a pull request, a first CI run and a backup. It is the page to start from; the others go deeper.

It is invite-only. There is no sign-up page. The first account is made on the server with one command, and every account after it comes from an invite. A repository starts private; its owner can widen it to every signed-in account on the server, or to anyone on the internet while the site admin allows public repositories. Contributing to any repository needs an account.

## Words used in these docs

- **Settings** in the page header is your account's settings: open the round avatar button at the top right, then Settings. It holds your profile, password, two-factor, sessions, SSH keys, access tokens, connected apps and notifications.
- **Settings** on a repository is the last tab of that repository's page: General, Collaborators, Branches, Webhooks, Secrets and variables, Environments, Runners and Pages.
- **Site admin** is in the same avatar menu, and only for site admins. It manages the whole instance.
- **The checkout** is the folder you cloned Codehaven into. Every `deploy/...` command runs from it.
- **sudo.** The setup, start, backup and restore scripts create files owned by the server's user, so they need root: run them with `sudo`. `deploy/compose.sh` needs only access to Docker, so it works without `sudo` if your user is in the `docker` group.
- **`deploy/compose.sh exec server codehaven ...`** runs the `codehaven` command inside the running server container. That is where the admin commands live, so only someone with access to the host can use them.

## What you need

| Need | Why |
| --- | --- |
| A Linux machine | The scripts use `/proc/meminfo`, `flock`, `ss` and systemd (for the backup timer). They are not written for macOS or Windows |
| Docker Engine with Compose 2.24 or newer and buildx | Everything runs and builds in containers. The host needs no Rust and no PostgreSQL |
| `git`, `openssl`, `curl`, `python3` and iproute2 (the `ip` command) | `setup.sh` checks for each and stops with its name if one is missing |
| Root, through `sudo` | The scripts make directories owned by the server's user (uid 10001) and write secrets |
| 4 GB of memory | `setup.sh` stops below about 3.8 GB, because the first build compiles the server |
| 20 GB free where the data lives | `CODEHAVEN_DATA_DIR`. `setup.sh` checks it |
| Room for Docker's images and build cache | They live under Docker's data root (`/var/lib/docker` by default). The first build makes a builder image and compiles the server, and the CI job image comes later. Plan for several more GB there; the figure was not measured. `docker system df` shows what is used. A watched disk at or over 85 percent full stops builds (`CODEHAVEN_DISK_LIMIT`) |
| Internet access while building | The builds pull base images, Debian packages, Rust crates and the Rust toolchain |

`make` is not needed for any command on this page. It is only for building and testing Codehaven itself.

### Docker on Debian or Ubuntu

Install Docker Engine from Docker's own apt repository (follow Docker's instructions for your distribution), with these packages: `docker-ce`, `docker-ce-cli`, `containerd.io`, `docker-buildx-plugin` and `docker-compose-plugin`. The `docker.io` package from a distribution's own repository may lack the buildx or Compose plugin, or have an old Compose; the version checks below tell you. Then check:

```sh
docker --version
docker compose version          # 2.24.0 or newer
docker buildx version
sudo docker info                # proves Docker is running and you can reach it
git --version; openssl version; curl --version | head -1; python3 --version; ip -V
```

## What works on a trial and what needs the real setup

You can try Codehaven on loopback with plain `http` and no domain. Some features need more.

| Feature | On a trial (`http://127.0.0.1:3200`) | Needs |
| --- | --- | --- |
| Sign in, repositories, issues, pull requests, review | Works | Nothing more |
| git over HTTPS and SSH from the same machine | Works | Nothing more |
| git over SSH from other machines | No | `CODEHAVEN_SSH_BIND` on a reachable address and `CODEHAVEN_SSH_EXTERNAL` set to match, with the port open |
| Other people reaching it | No | A reachable address, and for the internet a DNS name with TLS |
| CI jobs | The runner starts, but checkout fails: jobs get their code from `CODEHAVEN_EXTERNAL_URL`, and a job cannot reach a loopback or private address | A public `https` name on port 443 |
| Passkeys | No: an IP address cannot be a passkey's site | A domain name over `https` |
| Claude connector | Claude Code on the same machine only | An `https` address. Claude on the web, desktop and mobile needs a public one |
| Pages | No | `CODEHAVEN_PAGES_HOST`, a DNS name and a proxy site for it |
| Browser workspace and terminal | Works on `127.0.0.1` | `https` at a host name for other people. On plain `http` away from localhost, drafts cannot be encrypted |
| Email (password reset, invites, notifications) | Off | SMTP settings |

## 1. Get the code

```sh
git clone https://github.com/aaron777collins/codehaven-forge codehaven
cd codehaven
```

You get the `main` branch, the newest code. To run a fixed version instead, list the tags with `git tag --list` and check one out with `git checkout <tag>`; if there are none, `git rev-parse --short=12 HEAD` names the commit you run. Each commit builds into its own image tag, so an upgrade is `git pull` followed by `up.sh`, and an older commit stays available for rollback.

## 2. Make the settings file

```sh
cp .env.example .env
```

Edit `.env`. It holds no secrets, and [Configuration](configuration.md) explains every setting. The rules for the file: one `KEY=value` per line with an upper case name, `#` starts a comment only at the start of a line, and a value may be wrapped in one pair of single or double quotes, which are removed. Nothing else is interpreted, so a `#` later on a line is part of the value. A setting that is missing stops the scripts with its name.

The scripts create the directories you name, as root, so you do not make them yourself. A typical block:

```sh
CODEHAVEN_PROJECT=codehaven
CODEHAVEN_DATA_DIR=/srv/codehaven/data           # database, repositories, blobs, logs, master key
CODEHAVEN_SECRETS_DIR=/srv/codehaven/secrets     # database password, made by setup.sh
CODEHAVEN_BACKUP_DIR=/srv/codehaven/backups      # outside the checkout
CODEHAVEN_BUILD_ROOT=/srv/codehaven              # build locks and run marks
```

### A trial on one machine

```sh
CODEHAVEN_EXTERNAL_URL=http://127.0.0.1:3200
CODEHAVEN_PROXY=none
CODEHAVEN_TRUSTED_PROXIES='[]'
CODEHAVEN_HTTP_BIND=127.0.0.1:3200
CODEHAVEN_SSH_BIND=127.0.0.1:2222
CODEHAVEN_SSH_EXTERNAL=127.0.0.1:2222
```

Open `http://127.0.0.1:3200` on that machine, or through an `ssh -L 3200:127.0.0.1:3200` tunnel from yours. The address you open must match `CODEHAVEN_EXTERNAL_URL` exactly, or sign-in forms answer "Request blocked: the origin does not match this server."

### A real instance with Caddy

Point a DNS name at the machine, pick a free private /24 for the link between Caddy and the server, and set:

```sh
CODEHAVEN_EXTERNAL_URL=https://code.example.com
CODEHAVEN_PROXY=caddy
CODEHAVEN_SITE_HOST=code.example.com
CODEHAVEN_EDGE_SUBNET=10.20.16.0/24
CODEHAVEN_TRUSTED_PROXIES='["10.20.16.0/24"]'
```

To choose the subnet, list what the host already uses and take a range that is not there: `ip route` and `docker network inspect $(docker network ls -q) --format '{{.Name}} {{range .IPAM.Config}}{{.Subnet}} {{end}}'`. The same value goes into both `CODEHAVEN_EDGE_SUBNET` and `CODEHAVEN_TRUSTED_PROXIES`, which is a JSON list of network strings, so it keeps its double quotes inside single quotes. Only a peer in that list may tell the server a client's real address. Caddy gets the certificate for `CODEHAVEN_SITE_HOST` on its own. Ports 80 and 443 must be free.

For git over SSH from other machines also set `CODEHAVEN_SSH_BIND=0.0.0.0:2222` and `CODEHAVEN_SSH_EXTERNAL=code.example.com:2222` (the host people connect to, with the port), and open the port in your firewall. The proxy does not carry SSH. Behind your own proxy, see [Configuration](configuration.md), "The proxy".

Where settings live besides this file is explained at the top of [Configuration](configuration.md).

## 3. Set up and start

```sh
sudo bash deploy/prod/setup.sh
sudo bash deploy/prod/up.sh
```

`setup.sh` checks the host, makes the directories, and writes the database credentials into `CODEHAVEN_SECRETS_DIR`. It never prints them. It is safe to run again. `up.sh` builds the images, starts the instance, waits for the server to be healthy, runs a smoke check and prints the published ports. The first build compiles the server and takes a while. The server applies its own database migrations as it starts and makes its master key on first start.

### Check that it works

The server's port is published on `CODEHAVEN_HTTP_BIND` in all three proxy modes, so this works whichever you chose:

```sh
deploy/compose.sh ps
curl -fsS http://127.0.0.1:3200/healthz        # prints: ok
```

With `caddy`, ports 80 and 443 are also published, so check from outside too: `curl -fsS https://code.example.com/healthz`. With `network`, your own proxy sends to `<alias>:3200`, so check through the proxy's address. Then run the outside check against your public address:

```sh
bash deploy/dev/public-smoke.sh https://code.example.com
```

It makes read-only requests and prints one line per check. A good run looks like:

```
ok    TLS is valid and /healthz is 200
ok    /healthz says ok and nothing else
ok    header content-security-policy
...
public smoke: all checks passed
```

Any line that starts with `FAIL` ends the run with `public smoke: FAILED`. [Troubleshooting](troubleshooting.md) explains the common ones.

## 4. Make the first admin

```sh
sudo bash deploy/prod/first-admin.sh alice alice@example.com
```

This makes the account `alice` at once, an admin who may run CI, with no password yet, and prints a link. The link starts with `CODEHAVEN_EXTERNAL_URL`, so if that setting is wrong the link is too. It works once and lasts one hour. Open it and choose a password: 12 to 128 characters, not easy to guess, and without your username or email name in it. You are then signed in.

**Two-factor is not part of that page.** It is turned on afterwards, in Settings, Two-factor and security. Site admins are reminded: until two-factor is on, a banner says "Turn on two-factor sign-in to use the admin pages." and Site admin answers "Turn on two-factor sign-in in Settings before using the admin pages." Connecting Claude needs it as well. Choose "Set up an authenticator app", scan the QR code (or type the key) into an authenticator app, enter the code, and keep the ten recovery codes somewhere safe. They are shown once and are the way back in if you lose the app.

If the link expires before you use it, the account already exists, so `first-admin.sh` refuses to run again. Make a new link:

```sh
deploy/compose.sh exec server codehaven admin reset-password alice
```

The same command is the way back in if you, as the only admin, forget your password. It signs the account out everywhere and prints a new one-hour link. It does not turn two-factor off, so keep the recovery codes.

## 5. Back up now

```sh
sudo bash deploy/prod/backup.sh
```

It prints the path of one archive under `CODEHAVEN_BACKUP_DIR`. The archive holds the database, the repositories and the server's master key, so it is as sensitive as the instance. It sits on the same disk as the instance, so it does not survive losing that disk: copy it to another machine, encrypted. Then set up a daily schedule with `sudo bash deploy/prod/backup-timer.sh install`, and try a restore once. [Running an instance](operations.md) has the details, including an encrypted copy sent off the host for you.

## 6. Your first repository

Choose New repository (the plus menu, or `/new`). Name it `demo`, leave it private, and leave "Add a README file" unticked to see the empty-repository case, or tick it to start with a commit.

### Push over HTTPS

Make an access token: avatar menu, Settings, Access tokens. Name it, tick the **Repositories** scope, and also tick **Workflows** if you will push a file under `.codehaven/workflows/` (step 8 does). Copy the token when it is shown, because it is shown once. Use it as the password; your username stays the same.

An empty repository shows the commands. With a new local folder:

```sh
git init -b main
echo "# demo" > README.md
git add README.md
git commit -m "First commit"
git remote add origin https://code.example.com/alice/demo.git
git push -u origin main          # username: alice   password: the token
```

The first branch pushed becomes the default branch (`main`, then `master`, then the first by name). If you ticked "Add a README file", clone it instead: `git clone https://code.example.com/alice/demo.git`.

To stop typing the token, store it with a credential helper, for example `git config --global credential.helper store` (plain text on disk) or your system's keychain helper. A token expires after the time you chose, 90 days by default. The symptom is a push that fails with 401 or "Authentication failed" on something that worked before. Make a new token at the same page and push again; git may need the old stored one removed (`git credential reject`, or edit your credential store). An account with two-factor on cannot use its password for git, only a token.

### Push over SSH

Add a public key: avatar menu, Settings, SSH keys, paste the contents of `~/.ssh/id_ed25519.pub` (make one with `ssh-keygen -t ed25519`). The repository page shows the SSH address. On a trial, or with any port other than 22, it has this exact form, user `git` always:

```
ssh://git@127.0.0.1:2222/alice/demo.git
```

On port 22 it is `git@host:alice/demo.git`. The host and port come from `CODEHAVEN_SSH_EXTERNAL`, with or without a port (no port means 22). Test the connection before cloning:

```sh
ssh -T -p 2222 git@127.0.0.1
```

The first time, ssh asks whether to trust the server: "The authenticity of host '[127.0.0.1]:2222' can't be established. ED25519 key fingerprint is SHA256:... Are you sure you want to continue connecting (yes/no)?" Answer `yes`. The server makes that host key on first start and keeps it with its other secrets, so it does not change on restart or after a restore. A good key then prints "Hi alice! You have signed in, but Codehaven does not give shell access. Use git over SSH." Then `git clone ssh://git@127.0.0.1:2222/alice/demo.git` works.

## 7. Your first issue and pull request

1. In the repository, open Issues, then New issue. Give it a title and a description and submit it. It is `#1`.
2. Make a change on a branch and push it:

   ```sh
   git switch -c add-greeting
   echo "hello" > greeting.txt
   git add greeting.txt
   git commit -m "Add a greeting"
   git push -u origin add-greeting
   ```

3. Open Pull requests, then New pull request. Choose base `main` and compare `add-greeting`, write a title, and create it.
4. Open the pull request and choose Merge pull request (choose a method if asked).

A new repository has no branch protection, so nothing blocks you: as the owner you can merge your own pull request, with no approvals needed. The rules come from Settings, Branches on the repository (for example, "Required approvals"); once a rule requires approvals, your own approval never counts. See [Issues and pull requests](pull-requests.md).

## 8. Your first CI run

CI needs a runner. This takes some minutes the first time, because the job image is built once.

1. In the browser, open Site admin, Runners, and choose New registration token. Copy the token; it is shown once, works once and lasts 60 minutes. (Or: `deploy/compose.sh exec server codehaven admin runner-token`.)
2. On the host, from the checkout, register the runner:

   ```sh
   HAVEN_RUNNER_TOKEN=<token> sudo -E bash deploy/prod/ci-up.sh register runner-1
   ```

   `sudo` normally drops your environment, so `-E` is what carries `HAVEN_RUNNER_TOKEN` through; the token stays out of the process list. This saves the runner's credential.
3. Start CI:

   ```sh
   sudo bash deploy/prod/ci-up.sh
   ```

   This starts **dind**, a Docker daemon in its own container ("Docker in Docker") where only CI jobs run, so the host's Docker is never touched. It builds the job image `codehaven-job:trixie` (Debian with git, a C toolchain, Node, Python and Rust) if it is missing, which is the slow part on a first run and needs the internet, then loads the image into dind and starts the runner. It is safe to run again.
4. Confirm it is **Online**: Site admin, Runners shows `runner-1` with the status Online, or run `deploy/compose.sh exec server codehaven admin runners`, which prints the status `online`.
5. In your `demo` clone, make the file `.codehaven/workflows/hello.yml`:

   ```yaml
   name: hello
   on: [push, workflow_dispatch]
   jobs:
     hello:
       runs-on: ubuntu-latest
       steps:
         - run: echo "hello from CI"
   ```

6. Commit and push it (`git add .codehaven && git commit -m "Add a workflow" && git push`). Pushing a workflow file needs the Workflows scope on your token. Without it the push is refused: "This changes .codehaven/workflows, which runs as CI. A token needs the workflow scope to do that." Make a new token with the scope.
7. Open the repository's Actions tab. The run `hello` appears, then turns green with a check mark. Open it, then the job, to see `hello from CI` in the log.

Your account needs the "Can run CI" role. The first admin has it, so there is nothing to do here.

To prove that CI can fetch your code, add `- uses: actions/checkout@v4` as the first step and `- run: ls` after it, and push. If checkout fails, the job cannot reach `CODEHAVEN_EXTERNAL_URL`: see [Troubleshooting](troubleshooting.md).

What CI needs from the network: the built-in actions (`checkout`, `cache`, `upload-artifact`, `download-artifact`) and `run:` steps need only the server's public address. Any other action is fetched by the server from `github.com`, so the server needs outbound internet for those, and a step that installs packages needs the internet from the job. Images for `services:` and `container:` are pulled by the job daemon from the registry they name. Jobs cannot reach private networks or the host's own addresses other than tcp/443. [Using CI](using-ci.md) has the rest.

## 9. Invite people

Every account after the first comes from an invite. In the browser: Site admin, Invites, Create an invite. Or on the server:

```sh
deploy/compose.sh exec server codehaven admin invite create --email bob@example.com
```

The options are: an email address (the account must then use it), "They can manage the whole server" (site admin), "Their pushes may run workflows on this server's runners" (CI permission), an organization and team to join, a note for yourself, and how long the link works (1 hour to 30 days, 7 days by default). You do not need mail set up: Codehaven shows or prints the link and you send it yourself, by chat or email. The link works once. The invitee opens it, picks a username and a password, and has an account.

## 10. Public repositories

They are off until a site admin turns them on. Either Site admin, Settings, "Allow public repositories", or:

```sh
deploy/compose.sh exec server codehaven admin public-repos on
```

Turning it on makes nothing public. The owner of a repository then chooses Public at Settings, General, Visibility. Turning it off lowers every public repository to internal. See [Repositories](repositories.md).

## Where to go next

| You want | Read |
| --- | --- |
| Backups on a schedule, upgrades, rollback | [Running an instance](operations.md) |
| Every setting explained | [Configuration](configuration.md) |
| Workflows, runners, secrets | [Using CI](using-ci.md) |
| Mail for password resets and invites | [Configuration](configuration.md), "Settings the env file does not carry" |
| A static site from a branch | [Pages](pages.md) |
| Claude as a connected app | [The Claude connector](connector.md) |
| Something went wrong | [Troubleshooting](troubleshooting.md) |
