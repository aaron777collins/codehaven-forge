# Using CI

Workflows are written in the syntax of GitHub Actions. A registered runner picks up each job and runs it in a container. This page covers setting CI up, writing workflows, and what is and is not supported. The inner workings are in [CI](ci.md) and [The runner and the CLI](runner.md).

## Turn CI on

If you want a guided first run, follow "Your first CI run" in [Getting started](getting-started.md). CI needs four things.

1. **A runner.** A site admin takes a one-use registration token at Site admin, Runners, then on the instance's host:

   ```sh
   HAVEN_RUNNER_TOKEN=<token> sudo -E bash deploy/prod/ci-up.sh register runner-1
   sudo bash deploy/prod/ci-up.sh
   ```

   The first command saves the runner's credential. The second starts the runner's own Docker daemon (`dind`), builds the job image `codehaven-job:trixie` and loads it into that daemon, and starts the runner. It is safe to run again. A token lasts 60 minutes and works once; a failed attempt needs a new one. After restoring a backup, register with `--replace`.

   The token can also come from the server: `deploy/compose.sh exec server codehaven admin runner-token`. Add `--repo owner/name` to make a runner that serves only that repository.

2. **Permission for the repository's owner to run CI.** Workflows run only when the account that owns the repository has the "Can run CI" role. The first admin has it. An invite grants it when you tick "Their pushes may run workflows on this server's runners". Otherwise a site admin chooses Allow CI at Site admin, Users. An organization starts with the permission its creator had at that moment. Until then the Actions tab says "Workflows cannot run here yet", pushes start nothing and Run workflow is refused.

3. **A server address that jobs can reach.** A job gets its code from `CODEHAVEN_EXTERNAL_URL`. Jobs run in a separate Docker daemon that may reach the internet and nothing else on the host, except tcp/443 on the host's own public addresses. So the address must be a public `https` name on port 443. A loopback or private address, or another port, makes every checkout fail.

4. **Actions on for the repository** (the default) and the repository not archived.

`haven-runner` is the runner program. On the instance's host it already runs in the `runner` container, so you never run it by hand there. A runner on another Linux machine needs Docker and a copy of the binary. Take it out of the runner image that `up.sh` built (the tag is `CODEHAVEN_TAG`, or the short commit id of the checkout), or build it from `crates/haven-runner` with Rust (`cargo build --release -p haven-runner`):

```sh
docker create --name hr codehaven-runner:<tag>
docker cp hr:/usr/local/bin/haven-runner ./haven-runner
docker rm hr
```

Then, on that machine:

```sh
HAVEN_RUNNER_TOKEN=<token> haven-runner register --url https://code.example.com --name runner-2
haven-runner run
```

`haven-runner check` reports problems with the credential, the settings file, Docker, the work directory, disk and the server.

## Write a workflow

Put a YAML file in `.codehaven/workflows/` or `.github/workflows/` in the repository, ending in `.yml` or `.yaml`. If both directories have a file of the same name, `.codehaven` wins. Files over 256 KiB are refused, and a file that does not parse shows as a failed run with the line and the reason.

```yaml
name: ci
on:
  push:
    branches: [main]
  pull_request:
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: make test
```

### Events

| Event | Notes |
| --- | --- |
| `push` | `branches`, `branches-ignore`, `tags`, `tags-ignore`, `paths`, `paths-ignore`. Patterns use `*`, `**`, `?`, `+`, `[]` and `!`. Path filters are skipped for pushes of more than 3000 files |
| `pull_request` | `types` (default `opened`, `synchronize`, `reopened`) and branch and path filters. It tests the pull request's head commit, not a merge into the base |
| `workflow_dispatch` | A manual run from the Actions tab, the API or `haven workflow run`, with typed `inputs` |
| `schedule` | Cron entries, in UTC |
| `release` | `types`: published, created, released, prereleased, unpublished, edited, deleted |
| `workflow_call` | Makes the workflow reusable (below) |

Any other event, including `workflow_run`, `repository_dispatch` and `pull_request_target`, is refused when the file is read, with a reason. They are never ignored silently.

### What the syntax covers

Workflow keys: `name`, `run-name`, `on`, `env`, `defaults`, `concurrency`, `permissions` and `jobs`. Job keys: `name`, `runs-on`, `needs`, `if`, `env`, `defaults`, `outputs`, `steps`, `strategy`, `container`, `services`, `environment`, `timeout-minutes`, `continue-on-error`, `concurrency`, `permissions`, and `uses` with `with` and `secrets` for a reusable workflow. Step keys: `id`, `name`, `if`, `uses`, `with`, `run`, `shell`, `working-directory`, `env`, `continue-on-error` and `timeout-minutes`. An unknown key is an error that names the nearest valid one.

Expressions (`${{ }}`) follow the Actions rules, with the `github`, `env`, `vars`, `secrets`, `needs`, `matrix`, `steps`, `job`, `runner`, `inputs` and `strategy` contexts, and the functions `format`, `join`, `toJSON`, `fromJSON`, `contains`, `startsWith`, `endsWith`, `hashFiles`, `success`, `failure`, `always` and `cancelled`. A matrix may have `include` and `exclude` and at most 256 cells. YAML anchors and aliases work; merge keys (`<<: *base`) are refused, as GitHub refuses them.

`runs-on` takes a label. The runner stack offers `ubuntu-latest`, `ubuntu-24.04` and `linux`, and all three run the job image: Debian trixie with git, a C toolchain, Node, Python and Rust (stable, clippy and rustfmt). It identifies itself as Ubuntu 24.04 so tools such as `setup-python` fetch the builds they publish. A queued job whose labels no registered runner has fails after 15 minutes, and its page lists the labels the runners offer. Any other job fails as timed out after a day in the queue.

### Actions

`uses:` names an action:

- **Built in:** `actions/checkout`, `actions/cache` (with `restore` and `save`), `actions/upload-artifact`, `actions/download-artifact` and `docker://image`.
- **From GitHub:** `owner/repo@ref`, fetched and run, pinned to a commit for the whole run. JavaScript actions (`node12` to `node24`) and composite actions work. Real, unmodified actions run, such as `setup-node`, `setup-python`, `setup-go`, `setup-java`, `setup-dotnet`, `ruby/setup-ruby`, `dtolnay/rust-toolchain`, `actions/github-script`, `softprops/action-gh-release` and `ncipollo/release-action`.
- **From this server:** `haven_owner/repo@ref` for a repository here, used when it is the job's own, public, or the same owner's and at least as visible, and readable by whoever started the run.
- **In the job's workspace:** `./path/to/action`.

A site admin controls which actions may run, at Site admin, Actions: none, selected actions (an allow list of `owner/*` or `owner/repo`), or all public actions, a block list that always wins, an option to require full commit SHAs, and whether repositories on this instance may be used. Actions in the job's own repository are always allowed. A failed run offers to widen the list in one click.

### Reusable workflows and composite actions

A workflow with `on: workflow_call` (with `inputs`, `secrets` and `outputs`) can be called by a job of another: `uses: ./.codehaven/workflows/build.yml` for a file in the same repository, or `uses: haven_owner/repo/.codehaven/workflows/build.yml@ref` for another repository here. `.github/workflows` works in both. Workflows are never fetched from GitHub. A calling job takes only `name`, `needs`, `if`, `uses`, `with`, `secrets` and `permissions`. Put `strategy` and `concurrency` on a job of the called workflow instead. Calls nest 4 deep. Called jobs see only what the caller passed: `secrets: inherit` passes every secret, and the called token is never wider than the caller's.

An action with `runs.using: composite` (a folder with `action.yml`) runs its steps inside the step that used it. It cannot read `secrets`, `vars`, `needs` or `matrix` itself: pass what it needs in `with:`.

### Services and containers

`services:` start before the first step, in the job's network, so the job reaches a service at `localhost:<port>` and by its name (`postgres:5432`). The job waits up to 120 seconds for each port a service lists. `container:` sets the job's image. Options are limited to a safe list: no `--privileged`, `--network` or `--cap-add`.

## Secrets, variables and tokens

At Settings, Secrets and variables (needs Admin):

- **Secrets** are write only. Workflows read them as `${{ secrets.NAME }}`, people cannot. The name is saved in upper case, and saving again replaces the value. The runner masks every secret value in logs before sending them.
- **Variables** are readable and editable, as `${{ vars.NAME }}`.
- **Workflow permissions** cap what a job's own token may do: read only, or read and write when the workflow asks.

Secrets and variables can also be set for a whole organization through the REST API.

`GITHUB_TOKEN` (`${{ secrets.GITHUB_TOKEN }}` or `${{ github.token }}`) is a short-lived token for one job. With no `permissions:` it reads contents and packages. `contents: write` pushes, tags and makes releases; `statuses: write` posts commit statuses; `issues` or `pull-requests: write` opens and comments on issues and pull requests; `packages: write` pushes packages of the job's own repository. Naming any scope sets the others to none. Its writes go through branch protection with no bypass, never change workflow files and start no workflow. A pull request from a fork gets read access to contents and nothing else.

### Environments

A job can name where it deploys with `environment: prod`. A repository admin sets what that environment asks for at Settings, Environments:

- **Required reviewers**: up to 6 people with write access. One of them must approve each job that reaches it, on the run page.
- **Wait timer**: 0 to 43,200 minutes, from when the job reaches the environment.
- **Branches**: patterns for the branches that may deploy (`main`, `release/*`). A tag is `tags/v*`. A run from any other ref fails at once. A pull request from a fork never matches.
- **Secrets**: only that environment's jobs read them, only after the reviewers and timer allow it, and they win over a repository secret of the same name.

An environment that does not exist deploys with no gate. `environment:` URLs are not supported.

## Runs, logs, artifacts and caches

The Actions tab lists workflows and runs. Open a run to see its jobs, and a job to follow its log live. From there you can run a workflow by hand (Run workflow), re-run all jobs or only failed ones, cancel, and approve a run that waits. `GET /runs/{id}/logs` is a zip with a text file per job.

- **Logs** stop growing at 64 MiB per job, with a note. They are kept 90 days.
- **Artifacts** from `upload-artifact`: up to 2 GiB each, 100 per run, kept at most 90 days. A job reads its own run's artifacts. The v4 toolkit works too.
- **Caches** from `actions/cache` and the setup actions' built in caches: up to 2 GiB each and 10 GiB per repository (the least recently used go first), dropped after 7 days unused. A job restores caches saved on its own ref, the default branch or, for a pull request, its base branch, never another branch's. A pull request from a fork reads and never writes.
- **Status badges** at `/{owner}/{repo}/actions/workflows/<file>/badge.svg`, for signed-in viewers.
- **Who is told:** only failed runs notify the person who started them.

A pull request from a fork waits for someone with write access to approve that commit before anything runs. A push or pull request from a connected app that lacks the workflows permission also waits for approval.

## What is not supported

Jobs run in containers on a Docker daemon of their own, with no socket and no extra capabilities. By design:

- **Docker inside a job** (`docker build`, buildx, `docker login`, Docker container actions) fails at once with a reason. Use a `docker://image` step or a `run:` step. The container registry itself works from outside, with a real Docker client.
- **OIDC and cloud login** (`id-token: write` gives nothing), GraphQL (so most of `gh pr` and `gh issue`), the Pages deploy action, `environment:` URLs, `workflow_run`, `repository_dispatch` and `pull_request_target` fail with a named error and do not hang.
- **Windows, macOS and ARM runners.** Runners are Linux containers.
- **`strategy` or `concurrency` on a calling job** of a reusable workflow.
- **A virtual machine for each job.** Jobs share the runner's daemon. Invited people are trusted, so this is a limit for now, not a gap.

See [State of the project](state.md) for the reasons.

## Run Codehaven's own CI on itself

`.codehaven/workflows/ci.yml` runs `make ci CH_NATIVE=1` in the builder image. To use it on your instance, load the builder image once with `CODEHAVEN_CI_BUILDER=1 sudo -E bash deploy/prod/ci-up.sh` (a few GiB), make sure the owner may run CI, and push the source to a private repository. [Running an instance](operations.md) has the steps.
