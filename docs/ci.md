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

### Limits and things to know

- `POST /register` is public, so failed attempts are throttled per client address (10 per 10
  minutes, then `429`), in memory. The gate's own rate limiting (identity stream) still applies
  on top once it exists.
- Hostile workflow files: files over 512 KiB and YAML nested more than 50 levels are refused before
  the parser runs (a stack overflow would abort the whole server). Expressions have depth and length
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
