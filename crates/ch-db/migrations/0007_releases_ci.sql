-- Releases, and everything CI.

CREATE TABLE releases (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id          BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    tag_name         TEXT NOT NULL,
    target_commitish TEXT NOT NULL DEFAULT '',
    name             TEXT NOT NULL DEFAULT '',
    body             TEXT NOT NULL DEFAULT '',
    draft            BOOLEAN NOT NULL DEFAULT false,
    prerelease       BOOLEAN NOT NULL DEFAULT false,
    author_id        BIGINT NOT NULL REFERENCES users (id),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    published_at     TIMESTAMPTZ,
    UNIQUE (repo_id, tag_name)
);

CREATE TABLE release_assets (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    release_id     BIGINT NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    label          TEXT NOT NULL DEFAULT '',
    content_type   TEXT NOT NULL DEFAULT 'application/octet-stream',
    size           BIGINT NOT NULL,
    blob_sha256    TEXT NOT NULL,
    download_count BIGINT NOT NULL DEFAULT 0,
    uploader_id    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (release_id, name)
);

CREATE TABLE workflows (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id    BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    path       TEXT NOT NULL,
    name       TEXT NOT NULL,
    state      TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'disabled')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repo_id, path)
);

CREATE TABLE workflow_runs (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id           BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    workflow_id       BIGINT REFERENCES workflows (id) ON DELETE SET NULL,
    run_number        BIGINT NOT NULL,
    attempt           INTEGER NOT NULL DEFAULT 1,
    event             TEXT NOT NULL,
    head_sha          TEXT NOT NULL,
    head_branch       TEXT,
    git_ref           TEXT,
    title             TEXT NOT NULL DEFAULT '',
    actor_id          BIGINT REFERENCES users (id) ON DELETE SET NULL,
    -- `waiting` is a fork PR run that needs a maintainer to press "Approve and run".
    status            TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('waiting', 'queued', 'in_progress', 'completed')),
    conclusion        TEXT CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped', 'timed_out')),
    concurrency_group TEXT,
    event_payload     JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at        TIMESTAMPTZ,
    completed_at      TIMESTAMPTZ,
    UNIQUE (repo_id, run_number, attempt)
);
CREATE INDEX workflow_runs_repo_idx ON workflow_runs (repo_id, created_at DESC);
CREATE INDEX workflow_runs_sha_idx ON workflow_runs (repo_id, head_sha);

CREATE TABLE runners (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name            TEXT NOT NULL,
    labels          TEXT[] NOT NULL DEFAULT '{}',
    -- SHA-256 of the credential issued at registration.
    credential_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(credential_hash) = 32),
    version         TEXT,
    os              TEXT,
    arch            TEXT,
    -- NULL means the runner serves every repo that may run CI.
    repo_id         BIGINT REFERENCES repositories (id) ON DELETE CASCADE,
    registered_by   BIGINT REFERENCES users (id) ON DELETE SET NULL,
    last_seen_at    TIMESTAMPTZ,
    revoked_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Issued by a site admin. Single use, one hour.
CREATE TABLE runner_registration_tokens (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    token_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    created_by BIGINT REFERENCES users (id) ON DELETE SET NULL,
    repo_id    BIGINT REFERENCES repositories (id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    used_at    TIMESTAMPTZ,
    runner_id  BIGINT REFERENCES runners (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE workflow_jobs (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id           BIGINT NOT NULL REFERENCES workflow_runs (id) ON DELETE CASCADE,
    repo_id          BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    job_key          TEXT NOT NULL,
    name             TEXT NOT NULL,
    matrix           JSONB,
    runs_on          TEXT[] NOT NULL DEFAULT '{}',
    needs            TEXT[] NOT NULL DEFAULT '{}',
    status           TEXT NOT NULL DEFAULT 'waiting' CHECK (status IN ('waiting', 'queued', 'in_progress', 'completed')),
    conclusion       TEXT CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped', 'timed_out')),
    runner_id        BIGINT REFERENCES runners (id) ON DELETE SET NULL,
    -- 2 minute lease, extended by heartbeats. If it expires the job fails.
    lease_expires_at TIMESTAMPTZ,
    spec             JSONB NOT NULL DEFAULT '{}'::jsonb,
    outputs          JSONB NOT NULL DEFAULT '{}'::jsonb,
    token_hash       BYTEA,
    log_path         TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at       TIMESTAMPTZ,
    completed_at     TIMESTAMPTZ
);
CREATE INDEX workflow_jobs_run_idx ON workflow_jobs (run_id);
CREATE INDEX workflow_jobs_queue_idx ON workflow_jobs (created_at) WHERE status = 'queued';
CREATE INDEX workflow_jobs_lease_idx ON workflow_jobs (lease_expires_at) WHERE status = 'in_progress';

CREATE TABLE workflow_steps (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_id       BIGINT NOT NULL REFERENCES workflow_jobs (id) ON DELETE CASCADE,
    number       INTEGER NOT NULL,
    name         TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'in_progress', 'completed')),
    conclusion   TEXT CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped')),
    started_at   TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    -- Byte range of this step in the job log file.
    log_start    BIGINT,
    log_end      BIGINT,
    UNIQUE (job_id, number)
);

CREATE TABLE secrets (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    scope        TEXT NOT NULL CHECK (scope IN ('repo', 'org')),
    repo_id      BIGINT REFERENCES repositories (id) ON DELETE CASCADE,
    org_id       BIGINT REFERENCES users (id) ON DELETE CASCADE,
    name         TEXT NOT NULL CHECK (name ~ '^[A-Z_][A-Z0-9_]*$'),
    value_sealed BYTEA NOT NULL,
    created_by   BIGINT REFERENCES users (id) ON DELETE SET NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((scope = 'repo' AND repo_id IS NOT NULL AND org_id IS NULL) OR (scope = 'org' AND org_id IS NOT NULL AND repo_id IS NULL))
);
CREATE UNIQUE INDEX secrets_repo_uq ON secrets (repo_id, name) WHERE scope = 'repo';
CREATE UNIQUE INDEX secrets_org_uq ON secrets (org_id, name) WHERE scope = 'org';

CREATE TABLE variables (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    scope      TEXT NOT NULL CHECK (scope IN ('repo', 'org')),
    repo_id    BIGINT REFERENCES repositories (id) ON DELETE CASCADE,
    org_id     BIGINT REFERENCES users (id) ON DELETE CASCADE,
    name       TEXT NOT NULL CHECK (name ~ '^[A-Z_][A-Z0-9_]*$'),
    value      TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((scope = 'repo' AND repo_id IS NOT NULL AND org_id IS NULL) OR (scope = 'org' AND org_id IS NOT NULL AND repo_id IS NULL))
);
CREATE UNIQUE INDEX variables_repo_uq ON variables (repo_id, name) WHERE scope = 'repo';
CREATE UNIQUE INDEX variables_org_uq ON variables (org_id, name) WHERE scope = 'org';

CREATE TABLE artifacts (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id      BIGINT NOT NULL REFERENCES workflow_runs (id) ON DELETE CASCADE,
    job_id      BIGINT REFERENCES workflow_jobs (id) ON DELETE SET NULL,
    name        TEXT NOT NULL,
    blob_sha256 TEXT NOT NULL,
    size        BIGINT NOT NULL,
    expires_at  TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (run_id, name)
);

CREATE TABLE caches (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id      BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    key          TEXT NOT NULL,
    version      TEXT NOT NULL DEFAULT '',
    git_ref      TEXT,
    blob_sha256  TEXT NOT NULL,
    size         BIGINT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repo_id, key, version)
);

CREATE TABLE check_runs (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id      BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    head_sha     TEXT NOT NULL,
    -- `<workflow> / <job>`, matrix values in brackets.
    name         TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'in_progress', 'completed')),
    conclusion   TEXT CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped', 'timed_out', 'neutral')),
    details_url  TEXT,
    output       JSONB NOT NULL DEFAULT '{}'::jsonb,
    job_id       BIGINT REFERENCES workflow_jobs (id) ON DELETE CASCADE,
    external_id  TEXT,
    started_at   TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX check_runs_sha_idx ON check_runs (repo_id, head_sha);
CREATE UNIQUE INDEX check_runs_job_uq ON check_runs (job_id) WHERE job_id IS NOT NULL;

CREATE TABLE commit_statuses (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id     BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    sha         TEXT NOT NULL,
    context     TEXT NOT NULL DEFAULT 'default',
    state       TEXT NOT NULL CHECK (state IN ('pending', 'success', 'failure', 'error')),
    description TEXT,
    target_url  TEXT,
    creator_id  BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX commit_statuses_sha_idx ON commit_statuses (repo_id, sha, context, created_at DESC);

CREATE TABLE schedules (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workflow_id BIGINT NOT NULL REFERENCES workflows (id) ON DELETE CASCADE,
    repo_id     BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    cron        TEXT NOT NULL,
    next_run_at TIMESTAMPTZ NOT NULL,
    last_run_at TIMESTAMPTZ,
    UNIQUE (workflow_id, cron)
);
CREATE INDEX schedules_due_idx ON schedules (next_run_at);
