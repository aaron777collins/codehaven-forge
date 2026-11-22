-- Notifications, webhooks, the domain event outbox and the job queue.

CREATE TABLE notifications (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    repo_id      BIGINT REFERENCES repositories (id) ON DELETE CASCADE,
    reason       TEXT NOT NULL CHECK (reason IN ('author', 'assigned', 'mention', 'team_mention', 'review_requested', 'comment', 'state_change', 'subscribed', 'ci_activity')),
    subject_type TEXT NOT NULL CHECK (subject_type IN ('issue', 'pull', 'commit', 'release', 'workflow_run')),
    subject_id   BIGINT NOT NULL,
    title        TEXT NOT NULL,
    unread       BOOLEAN NOT NULL DEFAULT true,
    done         BOOLEAN NOT NULL DEFAULT false,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_read_at TIMESTAMPTZ,
    -- One inbox row per thread per person.
    UNIQUE (user_id, subject_type, subject_id)
);
CREATE INDEX notifications_inbox_idx ON notifications (user_id, done, updated_at DESC);

CREATE TABLE webhooks (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    scope         TEXT NOT NULL CHECK (scope IN ('repo', 'org')),
    repo_id       BIGINT REFERENCES repositories (id) ON DELETE CASCADE,
    org_id        BIGINT REFERENCES users (id) ON DELETE CASCADE,
    url           TEXT NOT NULL,
    secret_sealed BYTEA,
    content_type  TEXT NOT NULL DEFAULT 'json' CHECK (content_type IN ('json', 'form')),
    events        TEXT[] NOT NULL DEFAULT '{push}',
    active        BOOLEAN NOT NULL DEFAULT true,
    created_by    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((scope = 'repo' AND repo_id IS NOT NULL AND org_id IS NULL) OR (scope = 'org' AND org_id IS NOT NULL AND repo_id IS NULL))
);
CREATE INDEX webhooks_repo_idx ON webhooks (repo_id);
CREATE INDEX webhooks_org_idx ON webhooks (org_id);

CREATE TABLE webhook_deliveries (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    webhook_id       BIGINT NOT NULL REFERENCES webhooks (id) ON DELETE CASCADE,
    guid             UUID NOT NULL DEFAULT gen_random_uuid(),
    event            TEXT NOT NULL,
    action           TEXT,
    payload          JSONB NOT NULL,
    attempt          INTEGER NOT NULL DEFAULT 0,
    -- Retries at 1 min, 5 min, 30 min, 2 h, 6 h.
    next_attempt_at  TIMESTAMPTZ,
    delivered_at     TIMESTAMPTZ,
    success          BOOLEAN,
    request_headers  JSONB,
    response_status  INTEGER,
    response_headers JSONB,
    response_body    TEXT,
    duration_ms      INTEGER,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX webhook_deliveries_hook_idx ON webhook_deliveries (webhook_id, created_at DESC);
CREATE INDEX webhook_deliveries_due_idx ON webhook_deliveries (next_attempt_at) WHERE delivered_at IS NULL AND next_attempt_at IS NOT NULL;

-- Domain events. Written in the same transaction as the change. See ch-db outbox.
CREATE TABLE outbox_events (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind       TEXT NOT NULL,
    payload    JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE event_cursors (
    consumer   TEXT PRIMARY KEY,
    last_id    BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Postgres job queue claimed with FOR UPDATE SKIP LOCKED.
CREATE TABLE background_jobs (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind         TEXT NOT NULL,
    payload      JSONB NOT NULL DEFAULT '{}'::jsonb,
    state        TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'running', 'done', 'failed')),
    run_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempts     INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 5,
    locked_by    TEXT,
    locked_until TIMESTAMPTZ,
    last_error   TEXT,
    -- At most one queued job per key, so a burst of pushes queues one gc.
    dedupe_key   TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at  TIMESTAMPTZ
);
CREATE INDEX background_jobs_claim_idx ON background_jobs (run_at) WHERE state = 'queued';
CREATE UNIQUE INDEX background_jobs_dedupe_uq ON background_jobs (kind, dedupe_key) WHERE state = 'queued' AND dedupe_key IS NOT NULL;
