# The runner and the `haven` CLI

What the runner stream built, how to run it, how to test it, and the choices the other streams
need to know about. `ARCHITECTURE.md` sections 2.5, 9.4, 9.5 and 13 have the design.

## haven-runner

One binary, three commands.

```sh
haven-runner register --url http://server:3200 --name runner-1 [--labels ubuntu-latest,linux,x64]
                                    # the token comes from HAVEN_RUNNER_TOKEN (kept out of `ps`)
haven-runner run [--config config.toml] [--concurrency 2] [--work-dir /work]
haven-runner check                  # credential, settings, Docker, work dir, disk, server
```

Without `--labels`, `register` offers the labels of its settings file's `[labels]` table (the
stack's `deploy/runner.toml`: `ubuntu-latest`, `ubuntu-24.04` and `linux`) plus the architecture
(`x64` or `arm64`), so the server offers it exactly the jobs it has an image for.

`register` saves `.runner` (mode 0600, written atomically) next to where it runs, or under
`$HAVEN_RUNNER_HOME`. A second `register` is refused without `--replace`. The registration token
is single use, so a failed attempt needs a new one from Site admin > Runners.

`run` needs Docker. It reads `DOCKER_HOST` (and `DOCKER_TLS_VERIFY`, `DOCKER_CERT_PATH`), which in
the compose stack point at the separate `dind` daemon. The host's socket is never involved.
`deploy/runner.example.toml` documents every setting. Unknown keys are an error.

### In the compose stack

```sh
CH_STREAM=src bash deploy/dev/release.sh            # builds codehaven-runner and the other images
HAVEN_RUNNER_TOKEN=<token> sudo -E bash deploy/prod/ci-up.sh register runner-1
sudo bash deploy/prod/ci-up.sh                      # dind, the job image inside it, the runner
```

`ci-up.sh` builds `codehaven-job:trixie` (`deploy/images/job.Dockerfile`: Debian trixie with git,
a C toolchain, Node, Python and Rust through rustup with stable, clippy and rustfmt; no Docker
CLI, since jobs cannot reach a daemon) with the host's buildx
builder and loads it into `dind` with `docker save | docker load`. Nothing is started on the host
by these scripts until you run them; the runner name defaults to `CODEHAVEN_RUNNER_NAME`
(`runner-1` in `.env.example`).

### What happens to a job

1. `POST /jobs/request` long-polls (30 s). A slot is taken first, so a runner with 2 slots never
   holds a job it cannot start. The disk guard runs before each poll.
2. A directory `<work_dir>/job-<id>-<n>/` is created and bind-mounted into the job container at the
   same path. The container is `tail -f /dev/null` with limits (`--cpus 4`, 8 GiB, 4096 pids, no
   swap), never privileged, no devices, no added capabilities, no host network, no socket.
3. Each step: evaluate `if:` against the job status so far, evaluate `env`, `with` and `run`, run
   it, read `$GITHUB_OUTPUT`, `$GITHUB_ENV`, `$GITHUB_PATH` and `$GITHUB_STEP_SUMMARY`, report.
   `run:` steps are `docker exec` of a script file under `_runner/scripts`.
4. Cancel (from a heartbeat answer) stops the current step, kills its whole process tree, and
   still runs `always()` and `cancelled()` steps. A timeout does the same and the job ends as
   `timed_out`. Shutdown works the same way after a grace period, and a second signal stops
   everything at once.
5. Always: remove the container, take back ownership of what root made, delete the directory.
   A job container that died on its own is handled with a throwaway container. A runner that was
   killed leaves containers labelled `org.codehaven.runner=<id>` and `job-*` directories, and the
   next start removes both.

Environment: every `GITHUB_*` variable has a `CODEHAVEN_*` twin, plus `CI=true`, `RUNNER_*`, and
`GIT_CONFIG_*` for `safe.directory` on the workspace (it belongs to the runner's user, not the
container's). `GITHUB_TOKEN` is not in the environment; `${{ secrets.GITHUB_TOKEN }}` and
`${{ github.token }}` give the job token.

### Built-in actions

`checkout`, `cache` (and `cache/restore`, `cache/save`), `upload-artifact`, `download-artifact`,
`docker://image`, each with or without an `actions/` or `codehaven/` prefix and a version.
Every other `uses:` is fetched and run, as the next section says. They run inside the runner process, so they work on the
workspace from outside the container. The rules for what they may touch:

- File patterns never leave the workspace: no `..`, absolute paths only inside it, and symbolic
  links are never followed (otherwise a job could upload the runner's own `.runner`).
- Cache archives are unpacked with only regular files and directories, no setuid bits, and
  nothing outside the workspace. Zips are unpacked with `enclosed_name` and a byte count.
- `checkout` sends the job token only to the server's own origin (`http.<origin>/.extraheader`),
  and masks both the token and its base64 form.
- `checkout` never runs anything the job left behind or writes while it works. A process from an
  earlier step can still be running, so git never touches the workspace: it fetches and checks
  out into a new repository in the runner's private directory, with `--git-dir` and
  `--work-tree` on every call. The finished tree, `.git` included, is then moved into the
  workspace one directory at a time without following a link, replacing whatever the job left at
  those names (an old `.git` too). After that the `.git` is the job's, and the runner never runs
  git on it again. Every git call also runs with `core.hooksPath=/dev/null`,
  `core.fsmonitor=false`, `protocol.ext.allow=never`, no credential helper, and no global or
  system config.
- The runner's own files in the job directory (step scripts, `$GITHUB_OUTPUT`, `$GITHUB_ENV`,
  `$GITHUB_PATH`, `$GITHUB_STEP_SUMMARY`, `event.json`) are written and read one directory at a
  time without following a symbolic link. A link the job put at one of those names is replaced, a
  link on the way to them stops the step, and only regular files are read back. Archives the
  runner builds or downloads live in a private directory beside the job's, which the job cannot
  reach.

These rules keep one job's files from reaching the runner. They are not a sandbox: a job still
runs in a container on a shared Docker daemon. Jobs in virtual machines (ch-ctk.3) are the
stronger option if Codehaven ever runs code from people it does not trust.

### Other actions

Any other `uses:` is fetched (ch-ctk.4, ch-ctk.5):

- `owner/repo[/path]@ref` is a public GitHub action. `haven_owner/repo[/path]@ref` is a
  repository on this instance, served when it is the job's own, public, or the same owner's and
  as visible, and readable by the person who started the run. A scheduled run has no such person,
  so it uses only its own repository's actions and public ones. `./path` is an action in the
  job's own workspace.
- The runner asks the server (`POST /jobs/{id}/actions/resolve`, with the job token). The server
  checks the site's policy (Site admin > Actions: none, selected or all, an allow list, a block
  list that always wins, optionally full SHAs only), resolves the ref to a commit that a branch
  or tag of that repository reaches (a tag before a branch; never a fork's commit that GitHub
  serves under the parent's name, never `refs/pull/*`), keeps a tarball of that commit and pins
  the answer to the run. Later jobs and re-runs of failed jobs get the same commit, and the run
  page lists every action with its commit.
- A repository here is used when it is the job's own, public, or has the same owner and is at
  least as visible as the job's repository. A pull request from a fork uses public ones only.
  Any other looks exactly like a repository that does not exist.
- The runner downloads the archive with the job token, checks its sha256, unpacks regular files
  and folders only under `_runner/actions/<archive>` and reads `action.yml`.
- JavaScript actions (`node12` to `node24`) run with the job image's `node`: inputs as
  `INPUT_*`, `GITHUB_ACTION_PATH`, `GITHUB_STATE`, a `pre` script before the main one, and a
  `post` script as a step of its own after the workflow's steps, last first, when its
  `post-if` (default `always()`) holds. A post step shows as `Post <name>`.
- Composite actions run their steps inside the step that used them, with their own `inputs` and
  `steps` contexts and `github.action_path`; their `outputs` are their `value` expressions.
  Nesting stops at 9 levels.
- Docker container actions (`using: docker`) need a Docker daemon, which jobs do not have, so
  they fail with a message that says to use a `docker://image` step or a `run:` step.
- JavaScript actions also get `ACTIONS_RUNTIME_TOKEN` and `ACTIONS_CACHE_URL`, so the cache
  built into `setup-node`, `setup-python`, `setup-go`, `setup-java` and the rest works
  (ch-ctk.6). The token is a JWT the runner signs with the sha256 of the job token, which the
  server checks against the hash it keeps, and dies with the job. Like GitHub's, it reaches the
  cache service and nothing else: it carries none of the job token's scopes, so an action that
  was never given the job token cannot write a status, an issue or a release with it. Cache
  uploads that are never finished are dropped after an hour, or when the job ends, and the
  server holds at most 4 GiB of them at once. Caches follow the built-in cache's rules: a job reads
  its own ref, then a pull request's base, then the default branch, and writes only its own.
  The toolkit downloads an archive with no credential, so the lookup answers with a link this
  server signed for that one entry, which stops working after an hour.
- The same token opens the artifact service, so an action that bundles `@actions/artifact` (v4
  artifacts) works: `ACTIONS_RESULTS_URL` is the server's address, and the toolkit calls
  `/twirp/github.actions.results.api.v1.ArtifactService/` there (CreateArtifact,
  FinalizeArtifact, ListArtifacts, GetSignedArtifactURL, DeleteArtifact). The upload is a signed
  link the toolkit writes blocks to and the download is another, both made by this server for one
  upload or one artifact and good for an hour, because the toolkit sends no credential with them
  (the one public route, `/api/runner/v1/toolkit/blobs/{grant}`, now also takes PUT; anything
  without a good link is a plain 404). A job reads the artifacts of its own run and writes only
  its own; a pull request from a fork reads and never writes. As on GitHub, a name is written once
  per run: delete it to write it again. Artifacts stay the built-in action's 2 GiB, 100 per run,
  90 days at most. Blocks that are never joined go with the upload, after an hour or when the job
  ends.

### Services

`services:` (ch-ctk.7) start before the first step, each in the job container's network, so the
job reaches a service at `localhost:<port>` and by its name (`postgres:5432`). The job waits up
to `limits.service_wait_seconds` (120) for each port the service lists under `ports:`; health
check options are accepted and the port wait stands in for them. Options are the same safe list
as a job container's. `job.services.<id>.id` and `.ports` are in the `job` context. Services go
when the job ends.

### The job token

What `GITHUB_TOKEN` may do follows the workflow's `permissions:` (ch-ctk.8). With none it reads
contents and packages. `contents: write` pushes, tags and makes releases; `statuses: write`
posts commit statuses; `issues` or `pull-requests: write` opens and comments on issues and pull
requests (one scope here); `packages: write` pushes images linked to the job's own repository.
Naming any scope sets the others to none, and a job's block replaces the workflow's. Writes are
made as the `codehaven-actions` account, which cannot sign in, go through branch protection with
no bypass, never change workflow files and start no workflow. A repository admin can cap every
token at read (Settings > Secrets, or `PUT /repos/{o}/{r}/actions/permissions/workflow`). A pull
request from a fork reads contents and nothing else.

### Logs

Lines are masked first (every secret value, every line of a multi-line secret, `::add-mask::`
values, the job token, the runner's own credential), then batched every 500 ms or 64 KB. Step `0`
is the job's own log: image pulls, cap notices, post-job cache saves. Markers follow GitHub's raw
log format so the web stream can fold them: `##[group]`, `##[endgroup]`, `##[error]`,
`##[warning]`, `##[notice]`, `##[debug]`. A step's header, `##[group]Run <first line>`, echoes the
script. A job log stops growing at `limits.max_log_megabytes`; the job carries on.

## Contract choices the ci stream should match

The runner follows `ch_api_types::runner` and `ARCHITECTURE.md` 9.4. Where those leave room, it
chose as follows. If `ch-ci` chooses otherwise, change one side in the same pull request.

| Topic | What the runner does |
| --- | --- |
| Credentials | `Bearer <runner credential>` for register-less calls. `Bearer <job token>` for artifacts and cache. Git gets `Basic x-access-token:<job token>` |
| Log `seq` | One counter per job across all steps, from 0. A batch is for one step. `next_seq` in the answer is honoured when it is ahead |
| Lost lease | `404`, `410` or `409` on a heartbeat or step update stops the job and nothing is reported. `401` ends the runner with an error |
| Retries | `5xx`, `429` (with `Retry-After`) and network errors retry with backoff and jitter. Logs 8 times, steps 6, complete 14 (about 5 minutes), heartbeat 2. Other `4xx` never retry |
| Step updates | `in_progress`, then `completed` with a conclusion and outputs. A skipped step gets only `completed` with `skipped`. Steps that never started (job aborted) are not reported |
| Job conclusion | `success`, `failure`, `cancelled`, `timed_out` |
| Artifact upload | `PUT /jobs/{id}/artifacts/{name}` with a zip body, `Content-Length` set, optional `?retention_days=N` (not in the DTOs) |
| Cache | `POST /cache/lookup`, `PUT /cache/{key}?version=<v>` with a tar.gz body (`409` means it exists and is fine), `GET /cache/{id}`. `version` is a hash of the paths |
| Checkout by sha | If the run's ref has moved, the runner fetches the commit by sha, which needs `uploadpack.allowAnySHA1InWant` (or protocol v2 behaviour) on the git side |
| Server URL in jobs | `github.server_url` and `GITHUB_SERVER_URL` are `context.server_url` unchanged. A job container in `dind` cannot resolve compose names such as `server`, so give it the public URL |

## Expression engine

`src/expr.rs` is a full evaluator for `${{ }}` (contexts, `&&` and `||` returning operands, loose
equality, `a.*.b` filters, `format`, `join`, `toJSON`, `fromJSON`, `contains`, `startsWith`,
`endsWith`, the status functions, `hashFiles`). `ARCHITECTURE.md` 9.2 gives the engine to
`ch-workflow`, which the ci stream builds in parallel. The runner needed it before that crate had
one, so it has its own. The surface the runner uses is three functions (`eval_condition`,
`interpolate`, `eval_template`). When `ch-workflow`'s engine lands, replace the module body with
an adapter over those three and keep the tests, which pin GitHub's semantics.

## Tests

```sh
make STREAM=runner test P=haven-runner       # unit and integration tests, in the builder
make STREAM=runner test P=haven-cli
make STREAM=runner docker-test               # the executor against a real, throwaway Docker daemon
```

- `tests/job_engine.rs`, `actions.rs`, `runner_loop.rs`, `client.rs` run the real client and engine
  against an in-memory server over HTTP (`haven_runner::testing::FakeServer`). The host stands in
  for the container (`LocalBackend`). They cover step logic, masking, cancel and timeout (including
  process trees), artifacts, cache, checkout against a real git repository, retries, concurrency,
  shutdown, the disk guard and startup cleanup.
- `tests/docker.rs` needs a daemon. `deploy/dev/docker-tests.sh` starts `dind-test` (profile
  `docker-tests` in `deploy/compose.dev.yml`, its own daemon on the private `dev` network), builds
  the job image into it, and runs the file with `HAVEN_REQUIRE_DOCKER=1`. Without that variable and
  a daemon the tests skip with a note. The `nonroot_*` tests re-run themselves as uid 10001 under
  `setpriv`, which is how the runner runs in its container, to prove files root made inside the
  container can be taken back and removed.
- `deploy/dev/clean.sh` removes the test daemon's volumes.

## haven

`haven` is a REST v3 client built on `ch-api-types`. Commands: `auth login|logout|status|token|
git-credential|setup-git`, `repo create|clone|view|fork|list`, `issue list|view|create|close|
comment`, `pr create|list|view|checkout|diff|review|merge`, `run list|view|watch|logs|rerun|cancel`,
`workflow list|run`, `release create|list|upload`, `api`.

- The token is stored in the OS keyring (service `haven`), or in `~/.config/haven/tokens.toml`
  with mode 0600 when there is no keyring. `HAVEN_TOKEN` and `HAVEN_HOST` override both.
- It sends the token only to the host it was issued for, never to a link that points elsewhere.
- `-R owner/name` picks the repository, otherwise the `origin` remote of the current directory.
- `--json` on list and view commands prints the DTOs. `haven api` takes `-f` (strings), `-F`
  (typed), `-H`, `--input`, `--include` and `--paginate`.
- `pr merge --auto` turns on auto-merge (with `--squash`, `--rebase` or `--merge`), `--disable-auto` turns it off.
- `tests/cli.rs` drives every command against a fake server whose bodies are the committed
  `ch-api-types` snapshots.

## What a job can reach

A job is code that anyone with `can_run_ci` wrote, so it gets the internet and nothing else on the
host. `deploy/dind/guard.sh` is the `dind` service's entry point. Before it hands over to
`dockerd` it keeps three rules in place inside the daemon's own network namespace, so the host's
firewall is never touched:

- A job container may not open a connection to a private range (10/8, 172.16/12, 192.168/16,
  169.254/16, 100.64/10) through the `runners` network. Before this, a job could connect to ssh,
  postgres, redis and the local-only helpers on every host bridge address.
- A job container may not talk to the daemon's own address.
- Nothing inside the daemon, host-network jobs included, may open a connection to a private
  range. The runner's own connection and its replies are already established, so they pass.
- The same two rules apply to the host's own public addresses (`deploy/prod/host-ips.sh` lists
  them and `up.sh` and `ci-up.sh` pass them in as `HOST_PUBLIC_IPS`), with one exception: tcp/443,
  the public https site. The host listens on ssh, postgres, redis and more at its public address,
  and a job's packets to it used to be delivered, so every one of those answered.

Every container the runner starts also gets a network of its own, so one job cannot connect to
another job's listeners or read its secrets, and the `NET_RAW` capability is dropped, so a job
cannot open a raw socket to sniff or spoof. Both are checked by a real Docker test
(`jobs_cannot_reach_each_other_and_have_no_raw_sockets`) and by an e2e step that runs two jobs at
the same time.

Jobs reach the server over its public name, which is not a private address, so checkout,
artifacts and logs are unaffected. Jobs cannot remove the rules: `container.options` has no
`--privileged`, `--network` or `--cap-add`. The rules are checked again every 30 seconds. The e2e
run has a workflow step that tries every bridge gateway and public address the host has and fails if one answers.
