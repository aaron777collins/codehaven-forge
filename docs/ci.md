# CI: what `ch-workflow` and `ch-ci` provide

The design is in `ARCHITECTURE.md` section 9. This file is the hand-off: what is built, how the
pieces fit, and what the other streams must do to meet it.

## ch-workflow (pure, no IO, no async)

| Module | What it does |
| --- | --- |
| `parse` | YAML to a typed `Workflow`, every problem at once with its line (`line 14: ...`). Strict about unknown keys, specific about unsupported features |
| `expr` | `${{ }}` engine with Actions semantics: loose comparison, operand-returning `&&`/`||`, `.*` filters, `format`, `join`, `toJSON`, `fromJSON`, `contains`, `startsWith`, `endsWith`, `hashFiles` (supplied by the caller), `success()`, `failure()`, `always()`, `cancelled()`. Depth and length limits |
| `plan` | Matrix expansion (`include`, `exclude`, cap 256), job names, `runs-on` labels, the `needs` graph, cycles, matrices waiting on `needs` |
| `trigger` | `Workflow::matches(&EventInfo)` for push, pull_request, workflow_dispatch, schedule, release, with branch, tag and path filters |
| `glob` | Actions' pattern rules (`*`, `**`, `?`, `+`, `[]`, `!`) |
| `options` | The allow list for `container.options`. The server checks it when it reads a file and after expressions are filled in, and **`haven-runner` must call `options::validate` again** before it starts a container |
| `actions` | `uses:` classification: checkout, cache, upload-artifact, download-artifact, `docker://` |
| `inputs` | Validates and defaults `workflow_dispatch` inputs |
| `refs` | Which secrets and variables a job reads |
| `schedule` | Cron validation and the next fire time (UTC) |
| `discover` | Which files are workflows, `.codehaven` winning over `.github` |

The runner uses `expr` for `if:` and `${{ }}`, with `Context::with_hash_files` for `hashFiles`.
`expr::should_run` is the complete rule for a step or job `if:` (no condition is `success()`, a
condition with no status function is `success() && condition`).

## ch-ci

```
outbox event ──ci-triggers──> triggers::fire ──> runs::create_run ──> workflow_runs, workflow_jobs, check_runs
                                                       │
                          runs::advance_run  <─────────┘  (every state change goes through it, under a run row lock)
                                 │  promotes jobs whose needs are done, skips on `if:`, fail-fast,
                                 │  max-parallel, concurrency groups, cancel, matrix expansion, run roll-up
runner ──/api/runner/v1/jobs/request──> try_claim (FOR UPDATE SKIP LOCKED) ──> payload::build_payload
runner ──heartbeat / logs / steps / complete──> runner_api ──> logs, runs::finish_job
```

Workers: `ci-triggers` (event consumer, retries an event 3 times then skips it with an error in the
log), `ci-scheduler` (cron, plus a sweep that advances half-finished runs), `ci-reaper` (jobs whose
lease ran out fail with "The runner stopped responding", and a backstop for jobs far past their
timeout), `ci-janitor` (artifacts, caches, logs, spent registration tokens).

Runs are created only when the repository's owner has `can_run_ci`, `has_actions` is on and the
repository is not archived. `Ci::availability` says which, for the Actions tab.

### Credentials

`ch_db::ci_auth` is the one place that knows where the two CI credentials live. **`ch-auth` must call
it from `Authn::from_bearer`**:

- `runner_from_credential(pool, token)` for `chr_...` gives `Actor::Runner`. A revoked runner gives `None`.
- `job_from_token(pool, token)` for `chj_...` gives `Actor::Job`. It only works while the job runs.
  Jobs of a fork pull request get `write_checks: false`.

Registration tokens (`chrt_...`) are checked by `POST /api/runner/v1/register` itself, which is
the one public CI route. Nothing in `ch-ci` reads an identity header.

### What other streams rely on

- **git** must emit `Push`, `PrOpened` and `PrSynchronized`, and for a pull request the objects at its
  head sha must be readable in the base repository (`refs/pull/<n>/head`) before `PrOpened` is emitted,
  because the workflow files are read from there.
- **git and collab** mark a commit the server writes on top of a branch for someone (a file edit,
  an applied suggestion, Update branch): `Push.server_built` and `PrSynchronized.built_on`. That
  person vouches for their own change only, so CI counts them only when the commit under it is in
  `pull_cleared_commits` (CI ran it at once) or was approved. Otherwise the new commit waits like
  any commit nobody can be named for (ch-x12).
- **collab** calls `Checks::required_satisfied` for the merge gate, and may read `Checks::combined`.
  A commit with no checks and no statuses is `Pending`, never `Success`.
- **web** uses the `Ci` trait (new default-bodied methods: `availability`, `workflows`, `jobs`, `job`,
  `artifacts`, `approve_run`, `runners`) and for live logs opens
  `GET /api/v3/repos/{o}/{r}/actions/jobs/{job}/logs/stream` (SSE, events `log` and `done`, ids are
  byte offsets, resume with `Last-Event-ID`). `Ci::log_range` is in **bytes**.
- **foundation** (small change, made here): `reqinfo::request_info` also inserts `ClientInfo` as its own
  request extension, because domain crates cannot name the binary's `RequestMeta`. Handlers take
  `Option<Extension<ClientInfo>>`.

### REST

`/api/v3`: statuses (`POST /repos/{o}/{r}/statuses/{sha}`, `GET .../commits/{ref}/status`, `.../statuses`),
check runs, `actions/workflows` (list, get, enable, disable, dispatches, runs), `actions/runs` (list,
get, cancel, rerun, rerun-failed-jobs, approve, jobs, logs, artifacts), `actions/jobs` (get, logs,
logs/stream), `actions/artifacts` (list, get, zip, delete), `actions/caches`, secrets and variables at
repo and owner scope, and runners. Site-admin only: `POST /admin/actions/runners/registration-token`,
`GET /admin/actions/runners`, `DELETE /admin/actions/runners/{id}`.

Secrets and variables are written with a plain `value` (GitHub's libsodium step is not needed here,
the transport is TLS). Values are never returned.

### Tests

`cargo nextest run -p ch-workflow -p ch-ci` (`make STREAM=ci test P=ch-ci`). The `ch-ci` tests run a
real Postgres, real `git` repositories and the real router, with a fake runner that speaks the wire
protocol. `crates/ch-ci/tests/support/mod.rs` is the harness. `ch-ci` uses runtime-checked
`sqlx::query` rather than `query!`, so it adds nothing to `.sqlx/`; the tests run every query against
the real schema.

### Reusable workflows

A workflow that says `on: workflow_call` (with `inputs`, `secrets` and `outputs`, as on GitHub) can be
called by a job of another workflow: `uses: ./.codehaven/workflows/build.yml` for a file in the same
repository (read at the run's own commit), or `uses: haven_owner/repo/.codehaven/workflows/build.yml@ref`
for one in another repository on this instance (`.github/workflows` works in both). The `haven_` marks
a repository here; workflows are never fetched from GitHub. Such a job takes only `name`, `needs`,
`if`, `uses`, `with`, `secrets` and `permissions`.

A call is inlined when the run is made (ch-ctk.7). The called workflow's jobs take the calling job's
place in the same run, named `caller / called` on the run page, and a re-run builds exactly what ran.
Two jobs the server finishes itself sit around them: `caller (start)`, which stands for the caller's
`needs` and `if`, and `caller`, which waits for every called job and gives back the call's result and
`outputs`, so `needs.caller.result` and `needs.caller.outputs.x` mean what they mean on GitHub. They
have no runner and no check run.

What a called job may see is decided when the run is made, so it never sees what it was not given:

- `inputs.x` becomes what the caller passed (or the input's default). `with:` values and `secrets:`
  values may read `github`, `inputs` and `vars` only (not `needs`, `matrix`, `env`, `steps`), because they
  are pasted into jobs that are not the caller. `with:` may not read `secrets`.
- `secrets.X` becomes what the caller passed under that name, or an empty string. `secrets: inherit`
  passes every secret. `GITHUB_TOKEN` is always the job's own. A called job that reads the whole
  `secrets` context is refused unless the caller used `inherit`.
- Its token may only be narrower than the caller's `permissions:`: each scope is the lower of the two.
- The called workflow's `env` and `defaults` apply to its jobs under the job's own. Its workflow-level
  `concurrency` is ignored. The caller's workflow-level `env` and `defaults` do not reach a called job
  (on GitHub they do not either), so a secret the caller keeps in its own `env` is not passed on.

A file in another repository is used only when the run may use that repository: it is the caller's own,
or public, or has the same owner and is at least as visible and the person who started the run can read
it. A pull request from a fork and a scheduled run (nobody behind it) get public repositories only. A
repository the run may not use looks exactly like one that does not exist. Only a branch, a tag or a
commit one of them reaches is followed (never `refs/pull`), and the site's rule for `haven_` repositories
in the Actions policy applies. Calls nest 4 deep, a run may call 20 workflows and make 200 jobs this way
(counted before anything is built, so a call that fans out is refused at once), the text the calls paste
in is capped (64 KB for one value, 8 MB for a run), and a workflow that calls itself is refused. A call that cannot be made shows as a failed run with the
reason, like a workflow file that does not parse.

Not supported yet: `strategy` on a calling job (one call per matrix cell), `needs` outputs inside `with:`,
`concurrency` on a calling job, and `workflow_run`.

### Composite actions

An action with `runs.using: composite` (a folder with an `action.yml`) runs its steps inside the step
that used it. `uses: ./path/to/action` reads the folder in the job's own workspace, and `owner/repo@ref`
or `haven_owner/repo@ref` fetches it as any other action does. The step's `with:` values become the
action's `inputs`, an input the step leaves out takes its `default`, and the action's `outputs` are the
`value` expressions it declares, read after its steps ran. Its steps see `steps` (their own), `inputs`,
`github`, `env` and `runner`; `github.action_path` is the action's folder. `run:` steps default to
`bash`, and `if:`, `env:`, `working-directory:` and `continue-on-error:` work as in a workflow. A
composite action may use another one, up to 9 deep; the next level fails the step with a plain message.

A composite action reads **only what it is given**. Its steps cannot read `secrets`, `vars`, `needs` or
`matrix`: those are empty inside it, as on GitHub. A secret it needs goes in through `with:` (`with:
token: ${{ secrets.DEPLOY_KEY }}`, evaluated in the calling step, and masked in the log as usual). The same
goes for an input's `default:`: it is the action author's text, so it cannot read the job's secrets or
variables either. That keeps an action from another repository from reading a secret it was not handed.

### Deployment environments

A job names a place it deploys to with `environment: prod` (or `environment: {name: prod, url: ...}`; the
name is plain text, not an expression). Repository admins set up what that environment asks, at
`/{owner}/{repo}/settings/environments`:

- **Required reviewers**: up to 6 people with write access. One of them has to approve each job that
  reaches the environment, or reject it, on the run page. Only they can, the repository's owner and
  admins included. A reviewer's decision is audited (`deployment.approved`, `deployment.rejected`).
  Nobody is stopped from approving a run they started; a repository that wants a second pair of eyes
  lists someone else.
- **Wait timer**: 0 to 43,200 minutes counted from when the job reaches the environment. It runs
  beside the review, and both have to be done. The timer is checked every 15 seconds.
- **Branch filter**: patterns for the branches that may deploy (`main`, `release/*`). A plain pattern
  matches branches only, never a tag of the same name. A tag is allowed by a pattern that starts with
  `tags/` (`tags/v*`), which matches tags only (so a branch named `tags/x` is never matched). A run
  from any other ref fails the job at once, with the reason in its log. A pull request from a fork never
  matches a filter. With no filter, any ref may deploy (a fork's pull request still waits for a reviewer, if the
  environment has one).
- **Secrets**: sealed like repository secrets and never read back. A job in the environment gets the
  secrets it names, and an environment secret wins over a repository secret with the same name. They are
  released only once the gate is past: while a review is pending or rejected, and while the wait timer
  runs, the job has none. A pull request from a fork never gets any.

A job whose environment does not exist deploys with no gate and no environment secrets. A pull request
from a fork follows the fork rule first (nothing is created until a writer approves that commit) and
the environment's rules after. A job that waits shows as waiting on the run page; cancelling the run
cancels it. If everyone on the reviewers list has lost write access, nothing can approve the job, so
cancel the run and change the list.

### A private NuGet feed

`/nuget/{owner}/index.json` is a NuGet v3 feed for one account (ch-ctk.13): push, restore, search and
unlist. It is for people and tokens signed in here only. A signed-out request gets a Basic challenge,
the same for a feed that exists and one that does not, and there is no public feed.

```sh
dotnet nuget add source https://HOST/nuget/OWNER/index.json -n codehaven -u USER -p TOKEN --store-password-in-clear-text
dotnet nuget push pkg.1.0.0.nupkg -s codehaven -k TOKEN      # the key goes in X-NuGet-ApiKey
dotnet nuget delete Pkg 1.0.0 -s codehaven -k TOKEN          # unlists, never removes
```

A package is an `owner/id` in the same table as images and follows the same rules. A token needs
`read:packages` to restore and `write:packages` to push. A package with no repository belongs to its
owner (for an organization, members read and owners write). A package linked to a repository follows
that repository: read to restore, write to push or unlist. A workflow job pushes with `GITHUB_TOKEN` as
the key when its `packages: write` allows it, to a package of its own repository, and never creates
one for a public repository (the first push would publish it); a job with `packages: read` restores
those of its own repository. A person's first push makes the package the owner's alone.

A version is written once. `1.0` and `1.0.0` are the same version, and a second push of it is a `409`.
The server reads the zip's directory and the one top-level `.nuspec` (512 KiB at most, no DOCTYPE or
entity) and unpacks nothing. A package is at most 250 MiB and a package holds at most 5000 versions.
Unlisting hides a version from search; asking for it by version still restores it.

### Limits and things to know

- `POST /register` is public, so failed attempts are throttled per client address (10 per 10
  minutes, then `429`), in memory. The gate's own rate limiting (identity stream) still applies
  on top once it exists.
- Hostile workflow files (the same guard covers `action.yml` and YAML issue forms): files over
  256 KiB (issue forms: 64 KiB) are refused with a plain error. Every line break YAML knows (CRLF,
  a lone CR, NEL, LS, PS) is turned into LF once, and the nesting scan and the YAML parser are given
  that same text, so they cannot disagree about where a line ends. YAML nested more than 50 levels
  is refused before the parser runs. As a last line of defence the parse runs on its own thread with
  a stack of 5 KiB per byte of the cap (1.25 GiB of address space for a workflow, about 4 times what
  the deepest file at the cap needs, measured), so even a file the scan let through cannot overflow
  the stack that would abort the whole server. Expressions have depth and length
  limits. Path filters are skipped for pushes of more than 3000 files, which lets the run go ahead.
- A job's log stops at 64 MiB with a note. Artifacts: 2 GiB each, 100 per run, 90 days at most.
  Caches: 2 GiB each, 10 GiB per repository (least recently used go first), 7 days unused.
- The server does not mask secrets in logs. The runner masks them before sending (ARCHITECTURE.md 9.5).
- A queued job that no registered runner has the labels for fails after 15 minutes
  (`ci.no_runner_minutes`), and its page says which labels the runners here offer. Any other job
  fails as timed out after a day in the queue (`ci.queue_timeout_minutes`).
- A pushed workflow file that does not parse, or a job refused while planning (container options
  that expressions turn into `--privileged`, say), shows as a failed run with the reason, once
  per push that changed the file. A manual run reports the problem on the form instead.
- Caches: a job restores caches saved on its own ref, the default branch or, for a pull request,
  its base branch. Never another branch's.
- Status badges: `/{owner}/{repo}/actions/workflows/{file}/badge.svg` (`?branch=`, `?event=`), for
  signed-in viewers only, like every page here. `GET /runs/{id}/logs` is a zip with a text file per
  job. Only failed runs notify the person who started them.
