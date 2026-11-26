-- Deployment environments (ch-bj5): a job that names `environment:` waits for what the
-- environment asks before it starts. Required reviewers must approve, a wait timer must run
-- out, and only certain branches and tags may deploy. The environment's secrets go to the job
-- only after that, and never to a pull request from a fork.
CREATE TABLE environments (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id      BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    name         CITEXT NOT NULL CHECK (name ~ '^[A-Za-z0-9][A-Za-z0-9._ -]{0,99}$'),
    wait_minutes INTEGER NOT NULL DEFAULT 0 CHECK (wait_minutes BETWEEN 0 AND 43200),
    -- Branch and tag name patterns (`main`, `release/*`). Empty means any.
    branches     TEXT[] NOT NULL DEFAULT '{}',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repo_id, name)
);

CREATE TABLE environment_reviewers (
    environment_id BIGINT NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    user_id        BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    PRIMARY KEY (environment_id, user_id)
);

-- Sealed like repository secrets, and tied to the environment and the name.
CREATE TABLE environment_secrets (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    environment_id BIGINT NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    name           TEXT NOT NULL CHECK (name ~ '^[A-Z_][A-Z0-9_]*$'),
    value_sealed   BYTEA NOT NULL,
    created_by     BIGINT REFERENCES users (id) ON DELETE SET NULL,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (environment_id, name)
);

-- One row per job held at an environment. `release_at` is when the wait timer runs out.
CREATE TABLE deployment_gates (
    job_id         BIGINT PRIMARY KEY REFERENCES workflow_jobs (id) ON DELETE CASCADE,
    environment_id BIGINT NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    state          TEXT NOT NULL CHECK (state IN ('pending', 'approved', 'rejected')),
    requested_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    release_at     TIMESTAMPTZ NOT NULL,
    decided_by     BIGINT REFERENCES users (id) ON DELETE SET NULL,
    decided_at     TIMESTAMPTZ,
    comment        TEXT NOT NULL DEFAULT ''
);
CREATE INDEX deployment_gates_due_idx ON deployment_gates (release_at) WHERE state = 'approved';
