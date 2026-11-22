-- Issues and pull requests. A PR is an issue row (is_pull) plus a pull_requests row,
-- so they share numbers, labels, comments and the timeline.

CREATE TABLE milestones (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id     BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    number      BIGINT NOT NULL,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    open        BOOLEAN NOT NULL DEFAULT true,
    due_on      TIMESTAMPTZ,
    closed_at   TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repo_id, number),
    UNIQUE (repo_id, title)
);

CREATE TABLE labels (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id     BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    name        CITEXT NOT NULL,
    color       TEXT NOT NULL DEFAULT '0f766e' CHECK (color ~ '^[0-9a-fA-F]{6}$'),
    description TEXT NOT NULL DEFAULT '',
    UNIQUE (repo_id, name)
);

CREATE TABLE issues (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id        BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    number         BIGINT NOT NULL,
    is_pull        BOOLEAN NOT NULL DEFAULT false,
    author_id      BIGINT NOT NULL REFERENCES users (id),
    title          TEXT NOT NULL,
    body           TEXT NOT NULL DEFAULT '',
    open           BOOLEAN NOT NULL DEFAULT true,
    -- completed, not_planned, reopened
    state_reason   TEXT,
    locked         BOOLEAN NOT NULL DEFAULT false,
    lock_reason    TEXT,
    pinned         BOOLEAN NOT NULL DEFAULT false,
    milestone_id   BIGINT REFERENCES milestones (id) ON DELETE SET NULL,
    comments_count INTEGER NOT NULL DEFAULT 0,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at      TIMESTAMPTZ,
    closed_by      BIGINT REFERENCES users (id) ON DELETE SET NULL,
    search         TSVECTOR GENERATED ALWAYS AS (
        to_tsvector('simple', coalesce(title, '') || ' ' || coalesce(body, ''))
    ) STORED,
    UNIQUE (repo_id, number)
);
CREATE INDEX issues_repo_state_idx ON issues (repo_id, open, updated_at DESC);
CREATE INDEX issues_author_idx ON issues (author_id);
CREATE INDEX issues_search_idx ON issues USING gin (search);

CREATE TABLE issue_labels (
    issue_id BIGINT NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    label_id BIGINT NOT NULL REFERENCES labels (id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, label_id)
);

CREATE TABLE issue_assignees (
    issue_id BIGINT NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    user_id  BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, user_id)
);
CREATE INDEX issue_assignees_user_idx ON issue_assignees (user_id);

CREATE TABLE issue_comments (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    issue_id   BIGINT NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    author_id  BIGINT NOT NULL REFERENCES users (id),
    body       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX issue_comments_issue_idx ON issue_comments (issue_id, created_at);

CREATE TABLE reactions (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    subject_type TEXT NOT NULL CHECK (subject_type IN ('issue', 'issue_comment', 'review_comment')),
    subject_id   BIGINT NOT NULL,
    content      TEXT NOT NULL CHECK (content IN ('+1', '-1', 'laugh', 'confused', 'heart', 'hooray', 'rocket', 'eyes')),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, subject_type, subject_id, content)
);
CREATE INDEX reactions_subject_idx ON reactions (subject_type, subject_id);

CREATE TABLE timeline_events (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    issue_id   BIGINT NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    actor_id   BIGINT REFERENCES users (id) ON DELETE SET NULL,
    event      TEXT NOT NULL,
    data       JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX timeline_events_issue_idx ON timeline_events (issue_id, created_at);

CREATE TABLE issue_subscriptions (
    issue_id   BIGINT NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    subscribed BOOLEAN NOT NULL DEFAULT true,
    reason     TEXT NOT NULL DEFAULT 'subscribed',
    PRIMARY KEY (issue_id, user_id)
);

-- One row per PR, keyed by its issue row. `repo_id` is the base repository.
CREATE TABLE pull_requests (
    issue_id          BIGINT PRIMARY KEY REFERENCES issues (id) ON DELETE CASCADE,
    repo_id           BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    head_repo_id      BIGINT REFERENCES repositories (id) ON DELETE SET NULL,
    head_ref          TEXT NOT NULL,
    head_sha          TEXT NOT NULL,
    base_ref          TEXT NOT NULL,
    base_sha          TEXT NOT NULL,
    merge_base        TEXT,
    mergeable_state   TEXT NOT NULL DEFAULT 'unknown' CHECK (mergeable_state IN ('clean', 'conflict', 'behind', 'blocked', 'unknown')),
    merge_commit_sha  TEXT,
    merge_method      TEXT CHECK (merge_method IN ('merge', 'squash', 'rebase')),
    merged_at         TIMESTAMPTZ,
    merged_by         BIGINT REFERENCES users (id) ON DELETE SET NULL,
    draft             BOOLEAN NOT NULL DEFAULT false,
    auto_merge_method TEXT CHECK (auto_merge_method IN ('merge', 'squash', 'rebase')),
    maintainer_can_modify BOOLEAN NOT NULL DEFAULT true
);
CREATE INDEX pull_requests_repo_idx ON pull_requests (repo_id);
CREATE INDEX pull_requests_head_idx ON pull_requests (head_repo_id, head_ref);

CREATE TABLE review_requests (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pull_id      BIGINT NOT NULL REFERENCES pull_requests (issue_id) ON DELETE CASCADE,
    user_id      BIGINT REFERENCES users (id) ON DELETE CASCADE,
    team_id      BIGINT REFERENCES teams (id) ON DELETE CASCADE,
    requested_by BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((user_id IS NULL) <> (team_id IS NULL))
);
CREATE UNIQUE INDEX review_requests_user_uq ON review_requests (pull_id, user_id) WHERE user_id IS NOT NULL;
CREATE UNIQUE INDEX review_requests_team_uq ON review_requests (pull_id, team_id) WHERE team_id IS NOT NULL;

CREATE TABLE reviews (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pull_id      BIGINT NOT NULL REFERENCES pull_requests (issue_id) ON DELETE CASCADE,
    author_id    BIGINT NOT NULL REFERENCES users (id),
    state        TEXT NOT NULL CHECK (state IN ('pending', 'commented', 'approved', 'changes_requested', 'dismissed')),
    body         TEXT NOT NULL DEFAULT '',
    commit_sha   TEXT NOT NULL,
    submitted_at TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX reviews_pull_idx ON reviews (pull_id, submitted_at);

CREATE TABLE review_threads (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pull_id     BIGINT NOT NULL REFERENCES pull_requests (issue_id) ON DELETE CASCADE,
    path        TEXT NOT NULL,
    resolved    BOOLEAN NOT NULL DEFAULT false,
    resolved_by BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX review_threads_pull_idx ON review_threads (pull_id);

CREATE TABLE review_comments (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pull_id             BIGINT NOT NULL REFERENCES pull_requests (issue_id) ON DELETE CASCADE,
    review_id           BIGINT REFERENCES reviews (id) ON DELETE CASCADE,
    thread_id           BIGINT REFERENCES review_threads (id) ON DELETE CASCADE,
    author_id           BIGINT NOT NULL REFERENCES users (id),
    in_reply_to         BIGINT REFERENCES review_comments (id) ON DELETE SET NULL,
    path                TEXT NOT NULL,
    side                TEXT NOT NULL CHECK (side IN ('left', 'right')),
    line                INTEGER,
    start_line          INTEGER,
    original_line       INTEGER,
    commit_sha          TEXT NOT NULL,
    original_commit_sha TEXT NOT NULL,
    diff_hunk           TEXT NOT NULL DEFAULT '',
    body                TEXT NOT NULL,
    -- True once later commits changed the line. The comment keeps its original hunk.
    outdated            BOOLEAN NOT NULL DEFAULT false,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX review_comments_pull_idx ON review_comments (pull_id, created_at);
