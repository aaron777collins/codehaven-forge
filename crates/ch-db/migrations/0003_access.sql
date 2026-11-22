-- Invite-only access (ARCHITECTURE.md 7.3 to 7.6). There is no sign-up table because
-- there is no sign-up route.

CREATE TABLE invites (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- SHA-256 of the full `chi_...` token. The token itself is never stored.
    token_hash    BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    -- NULL when made by the CLI.
    created_by    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_via   TEXT NOT NULL DEFAULT 'web' CHECK (created_via IN ('web', 'cli')),
    -- If set, the account must use this address.
    email         CITEXT,
    grants_admin  BOOLEAN NOT NULL DEFAULT false,
    grants_ci     BOOLEAN NOT NULL DEFAULT false,
    org_id        BIGINT REFERENCES users (id) ON DELETE SET NULL,
    team_id       BIGINT REFERENCES teams (id) ON DELETE SET NULL,
    -- A note to self, shown only in the admin list.
    note          TEXT,
    expires_at    TIMESTAMPTZ NOT NULL,
    used_at       TIMESTAMPTZ,
    used_by       BIGINT REFERENCES users (id) ON DELETE SET NULL,
    revoked_at    TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (expires_at > created_at),
    -- Used implies a user (unless that user was deleted later, which nulls used_by).
    CHECK (used_by IS NULL OR used_at IS NOT NULL)
);
CREATE INDEX invites_created_idx ON invites (created_at DESC);

-- Set-password and reset-password links, minted by the CLI or an admin.
CREATE TABLE one_time_links (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    token_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose    TEXT NOT NULL CHECK (purpose IN ('set_password', 'reset_password')),
    expires_at TIMESTAMPTZ NOT NULL,
    used_at    TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX one_time_links_user_idx ON one_time_links (user_id);

-- Password sign-in lockout. Keyed by account (`acct:<id>`) or address (`ip:<addr>`).
-- 10 failures in 15 minutes lock for 15 minutes, doubling per repeat up to 24 hours.
-- Tokens and SSH keys ignore this table.
CREATE TABLE login_failures (
    key             TEXT PRIMARY KEY,
    fail_count      INTEGER NOT NULL DEFAULT 0,
    window_start    TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_until    TIMESTAMPTZ,
    lock_level      INTEGER NOT NULL DEFAULT 0,
    last_failure_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
