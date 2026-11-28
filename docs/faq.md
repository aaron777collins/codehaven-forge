# Frequently asked questions

Short answers, with a link to where the detail is.

## The basics

### What is Codehaven for?

Running your own code forge: git over HTTP and SSH, issues, pull requests with review, CI, releases, packages, a container registry, static sites and a browser editor, on a machine you control. It is built for people who know each other: invited people are trusted, and the effort goes into keeping strangers out and never losing data.

### Can anyone sign up?

No. There is no sign-up page and no sign-up route. The first account is made on the server with `first-admin.sh`, and every other account comes from an invite a site admin makes. See [Accounts and sign-in](accounts.md).

### Can I make a repository public?

Yes, once the site admin allows public repositories, and only the account that owns the repository, an owner of its organization or a site admin with two-factor can do it. A repository is private by default. Its owner can widen it to every signed-in account on the server, or to anyone on the internet while the site admin allows public repositories. Reading a public repository needs no account. Contributing needs an account. See [Repositories](repositories.md).

### What do I need to run it?

A Linux machine with Docker (Compose 2.24 or newer, and buildx), git, openssl, curl, python3 and iproute2, 4 GB of memory and 20 GB of free disk. You do not need Rust or PostgreSQL on the host. See [Getting started](getting-started.md).

### Does it run on macOS, Windows or a Raspberry Pi?

The deploy scripts are written for Linux, and the docs cover only that. Other platforms are not documented here.

### Is it free?

Codehaven is MIT licensed.

## Installing and running

### Do I need a domain name?

For a trial, no: run it on loopback with no proxy. For real use, yes. TLS needs a name, passkeys need a name, the Claude connector needs a public `https` name, CI jobs reach the server by its public name, and the browser workspace works best over `https` at a name.

### Can I use my own reverse proxy?

Yes. Set `CODEHAVEN_PROXY=network` and join the server to the Docker network your proxy is on, or `none` and send to the published loopback port. The proxy must send every path to the server, drop `Remote-User` style headers, not buffer responses, and be listed in `CODEHAVEN_TRUSTED_PROXIES`. See [Configuration](configuration.md).

### Where is my data?

Under `CODEHAVEN_DATA_DIR`: `postgres` (the database), `git` (the repositories), `blobs` (artifacts, caches, release files, image layers), `logs` and `secrets` (the server's master key). The database password is in `CODEHAVEN_SECRETS_DIR`. Back all of it up with `sudo bash deploy/prod/backup.sh`. See [Running an instance](operations.md).

### How do I upgrade?

Back up, `git pull`, then `sudo bash deploy/prod/up.sh`. It dumps the database first, builds, starts, checks the server and rolls back to the previous image by itself if the new one is not healthy.

### How do I roll back?

If `up.sh` fails it rolls back for you. To go back by hand, check out the older commit and run `CODEHAVEN_NO_MIGRATE=1 sudo -E bash deploy/prod/up.sh`. You rarely need the database dump: newer migrations only add things the older server ignores. See [Running an instance](operations.md).

### How do I move to another machine?

Take a backup on the old one, copy the archive across, and on the new one install Codehaven, then run `sudo bash deploy/prod/restore.sh <archive>` into an empty location and `up.sh`. The archive holds the master key, so keep it private in transit.

### Is any data sent to another service?

The server has no telemetry and no update check, and the workspace loads nothing from another site. It contacts other hosts only because you set something up: it fetches actions from GitHub for workflows that use them, imports a repository from an address you give it, delivers your webhooks, reads a connecting app's client document from a trusted host when the Claude connector is on, sends mail through the server you configure, and sends the off-host backup you configure.

### Does Codehaven send email?

Only if you set up SMTP. Then it mails password reset links, invites made for an address, and notifications for people who turn that on. See [Configuration](configuration.md).

## Using it

### Can I sign in with GitHub, Google or my company login?

No. Accounts are local: a password, with optional two-factor by authenticator app or passkey. A proxy cannot sign anyone in either: Codehaven ignores identity headers.

### I lost my authenticator and my recovery codes. What now?

There is no admin command that turns two-factor off for another account, by design. Keep recovery codes safe and add a passkey as a second way in. See [Accounts and sign-in](accounts.md).

### Why can't I push with my password?

Git over HTTPS needs a personal access token as the password, if the account has two-factor on. Make one at Settings, Access tokens. SSH keys work as usual.

### Can I use GitHub Actions workflows?

Mostly. Workflows use GitHub Actions syntax, `.github/workflows` is read, and real actions from GitHub run, such as `actions/checkout`, `setup-node` and `cache`. Things that need a Docker daemon in the job, OIDC, GraphQL, `workflow_run`, `pull_request_target`, or Windows, macOS and ARM runners do not work, and say so instead of hanging. See [Using CI](using-ci.md).

### Why does my pull request from a fork not run CI?

Someone with write access has to approve that commit first, because the code comes from a fork. Open the pull request and choose Approve and run.

### Can I move a repository over from GitHub?

Yes, with New repository, Import a repository: any git repository anyone can clone over `https`, up to 2 GB. Branches and tags come across. Issues and pull requests do not. The REST API at `/api/v3` is shaped like GitHub's, so many tools work with a different base address.

### Is there a command line tool?

`haven`, a client for the REST API. It is built from `crates/haven-cli`. See [Repositories](repositories.md).

### How do I let Claude work on my repositories?

Turn on the connector (`CODEHAVEN_MCP_ENABLED=true`), then add `https://<your address>/mcp` as a connector in Claude. You choose what it may read, write and run. See [The Claude connector](connector.md).

## What is not here

Not in this version, by design or for later:

- GraphQL, a wiki, project boards, Git LFS, pull mirrors and more than one server instance.
- Docker inside CI jobs, a virtual machine for each job, OIDC for jobs, `workflow_run`, `repository_dispatch` and `pull_request_target`, and Windows, macOS and ARM runners.
- A sign-up page, sign-in through another provider, and deploy keys (the SSH server accepts them but no page creates one).
- A wildcard certificate for Pages full mode set up for you.
- Linux in the browser tab. The shell there covers everyday commands.

[State of the project](state.md) lists what is built, how it is checked, and what is left out and why.
