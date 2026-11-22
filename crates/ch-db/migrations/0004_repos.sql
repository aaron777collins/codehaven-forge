-- Repositories and everything attached to the repository itself.

CREATE FUNCTION ch_join_text(parts TEXT[], sep TEXT) RETURNS TEXT
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT array_to_string(parts, sep) $$;

CREATE TABLE repositories (
    id                    BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_id              BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name                  CITEXT NOT NULL CHECK (name ~ '^[A-Za-z0-9._-]{1,100}$' AND name NOT IN ('.', '..')),
    description           TEXT NOT NULL DEFAULT '',
    website               TEXT NOT NULL DEFAULT '',
    -- `public` means every signed-in account on this instance. Anonymous callers get nothing.
    visibility            TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('public', 'private')),
    default_branch        TEXT NOT NULL DEFAULT 'main',
    fork_parent_id        BIGINT REFERENCES repositories (id) ON DELETE SET NULL,
    -- The root of the fork network, whose objects forks borrow through alternates.
    fork_root_id          BIGINT REFERENCES repositories (id) ON DELETE SET NULL,
    archived              BOOLEAN NOT NULL DEFAULT false,
    is_empty              BOOLEAN NOT NULL DEFAULT true,
    topics                TEXT[] NOT NULL DEFAULT '{}',
    has_issues            BOOLEAN NOT NULL DEFAULT true,
    has_pulls             BOOLEAN NOT NULL DEFAULT true,
    has_actions           BOOLEAN NOT NULL DEFAULT true,
    -- Also read workflows from .github/workflows.
    read_github_workflows BOOLEAN NOT NULL DEFAULT true,
    allow_merge_commit    BOOLEAN NOT NULL DEFAULT true,
    allow_squash_merge    BOOLEAN NOT NULL DEFAULT true,
    allow_rebase_merge    BOOLEAN NOT NULL DEFAULT true,
    delete_branch_on_merge BOOLEAN NOT NULL DEFAULT false,
    -- Issues and PRs share one number sequence per repo, taken with UPDATE ... RETURNING.
    next_number           BIGINT NOT NULL DEFAULT 1,
    size_kb               BIGINT NOT NULL DEFAULT 0,
    flags                 JSONB NOT NULL DEFAULT '{}'::jsonb,
    pushed_at             TIMESTAMPTZ,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    search                TSVECTOR GENERATED ALWAYS AS (
        to_tsvector('simple', coalesce(name::text, '') || ' ' || coalesce(description, '') || ' ' || coalesce(ch_join_text(topics, ' '), ''))
    ) STORED,
    UNIQUE (owner_id, name)
);
CREATE INDEX repositories_search_idx ON repositories USING gin (search);
CREATE INDEX repositories_name_trgm ON repositories USING gin ((name::text) gin_trgm_ops);
CREATE INDEX repositories_fork_root_idx ON repositories (fork_root_id);

-- Rename and transfer leave a row here so old URLs still land.
CREATE TABLE repo_redirects (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    old_owner  CITEXT NOT NULL,
    old_name   CITEXT NOT NULL,
    repo_id    BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (old_owner, old_name)
);

CREATE TABLE repo_collaborators (
    repo_id    BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    permission TEXT NOT NULL CHECK (permission IN ('read', 'triage', 'write', 'maintain', 'admin')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repo_id, user_id)
);
CREATE INDEX repo_collaborators_user_idx ON repo_collaborators (user_id);

-- Declared here because it references repositories. Teams were created earlier.
CREATE TABLE team_repos (
    team_id    BIGINT NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
    repo_id    BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    permission TEXT NOT NULL CHECK (permission IN ('read', 'triage', 'write', 'maintain', 'admin')),
    PRIMARY KEY (team_id, repo_id)
);
CREATE INDEX team_repos_repo_idx ON team_repos (repo_id);

CREATE TABLE branch_protections (
    id                       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id                  BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    pattern                  TEXT NOT NULL,
    required_approvals       INTEGER NOT NULL DEFAULT 0 CHECK (required_approvals >= 0),
    dismiss_stale_reviews    BOOLEAN NOT NULL DEFAULT false,
    require_code_owner_review BOOLEAN NOT NULL DEFAULT false,
    required_checks          TEXT[] NOT NULL DEFAULT '{}',
    strict                   BOOLEAN NOT NULL DEFAULT false,
    require_resolved_threads BOOLEAN NOT NULL DEFAULT false,
    block_force_push         BOOLEAN NOT NULL DEFAULT true,
    block_deletion           BOOLEAN NOT NULL DEFAULT true,
    allow_direct_push        BOOLEAN NOT NULL DEFAULT false,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repo_id, pattern)
);

CREATE TABLE stars (
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    repo_id    BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, repo_id)
);

CREATE TABLE watches (
    user_id BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    level   TEXT NOT NULL CHECK (level IN ('all', 'participating', 'ignore')),
    PRIMARY KEY (user_id, repo_id)
);

CREATE TABLE deploy_keys (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id     BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    title       TEXT NOT NULL,
    public_key  TEXT NOT NULL,
    fingerprint TEXT NOT NULL UNIQUE,
    read_only   BOOLEAN NOT NULL DEFAULT true,
    created_by  BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX deploy_keys_repo_idx ON deploy_keys (repo_id);
