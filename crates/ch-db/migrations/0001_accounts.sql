-- Accounts: users (people, orgs and bots share one table), emails, sessions,
-- tokens, keys, two-factor recovery codes, settings.
-- ARCHITECTURE.md section 5.3. All ids are BIGINT identity, names people type are citext.

CREATE EXTENSION IF NOT EXISTS citext;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Settings that belong to the instance, not to a user.
CREATE TABLE instance_settings (
    key        TEXT PRIMARY KEY,
    value      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind                TEXT NOT NULL DEFAULT 'user' CHECK (kind IN ('user', 'org', 'bot')),
    username            CITEXT NOT NULL UNIQUE
                        CHECK (username ~ '^[A-Za-z0-9]([A-Za-z0-9-]{0,37}[A-Za-z0-9])?$'),
    display_name        TEXT NOT NULL DEFAULT '',
    description         TEXT NOT NULL DEFAULT '',
    -- NULL until the person opens their set-password link. No password, no password sign-in.
    password_hash       TEXT,
    is_admin            BOOLEAN NOT NULL DEFAULT false,
    -- Workflows run only for repos owned by an account with this on (ARCHITECTURE.md 2.5).
    can_run_ci          BOOLEAN NOT NULL DEFAULT false,
    suspended_at        TIMESTAMPTZ,
    -- AES-256-GCM sealed, see ch-db sealing.
    totp_secret_sealed  BYTEA,
    totp_enabled_at     TIMESTAMPTZ,
    -- For orgs: the permission every member gets on the org's repos.
    org_base_permission TEXT NOT NULL DEFAULT 'read' CHECK (org_base_permission IN ('none', 'read', 'triage', 'write', 'maintain', 'admin')),
    -- NULL for the first admin, who is made by the CLI.
    invited_by          BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_login_at       TIMESTAMPTZ
);
CREATE INDEX users_username_trgm ON users USING gin ((username::text) gin_trgm_ops);
CREATE INDEX users_kind_idx ON users (kind);

CREATE TABLE user_emails (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id     BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    email       CITEXT NOT NULL UNIQUE,
    is_primary  BOOLEAN NOT NULL DEFAULT false,
    verified_at TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX user_emails_one_primary ON user_emails (user_id) WHERE is_primary;

-- A browser session. Only the SHA-256 of the cookie value is stored.
CREATE TABLE sessions (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    token_hash   BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Per-session CSRF secret, random.
    csrf_secret  BYTEA NOT NULL,
    -- True between the password step and the two-factor step.
    pending_2fa  BOOLEAN NOT NULL DEFAULT false,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- 7 days idle and 30 days absolute (ARCHITECTURE.md 7.1).
    expires_at   TIMESTAMPTZ NOT NULL,
    ip           INET,
    user_agent   TEXT
);
CREATE INDEX sessions_user_idx ON sessions (user_id);
CREATE INDEX sessions_expires_idx ON sessions (expires_at);

CREATE TABLE oidc_identities (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    provider   TEXT NOT NULL,
    subject    TEXT NOT NULL,
    email      CITEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (provider, subject)
);

-- Personal access tokens: `chp_` plus 40 base62 characters. Only the hash is stored.
CREATE TABLE personal_access_tokens (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    token_hash   BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    prefix       TEXT NOT NULL,
    scopes       TEXT[] NOT NULL,
    -- Required. 90 days by default, one year at most.
    expires_at   TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ,
    revoked_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX pat_user_idx ON personal_access_tokens (user_id);

CREATE TABLE ssh_keys (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    title        TEXT NOT NULL,
    public_key   TEXT NOT NULL,
    key_type     TEXT NOT NULL,
    -- `SHA256:` plus base64, unique across the instance.
    fingerprint  TEXT NOT NULL UNIQUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ
);
CREATE INDEX ssh_keys_user_idx ON ssh_keys (user_id);

CREATE TABLE totp_recovery_codes (
    id        BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id   BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash BYTEA NOT NULL,
    used_at   TIMESTAMPTZ,
    UNIQUE (user_id, code_hash)
);

CREATE TABLE user_settings (
    user_id            BIGINT PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    theme              TEXT NOT NULL DEFAULT 'system' CHECK (theme IN ('system', 'light', 'dark')),
    notification_prefs JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
