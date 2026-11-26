-- WebAuthn passkeys as a second factor beside TOTP (ch-wf0). A passkey is one of an account's
-- second factors: with one registered, the account has two-factor on, exactly as with an
-- authenticator app, and recovery codes still work.
CREATE TABLE webauthn_credentials (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id       BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- What the authenticator calls the credential. Unique across the site: a credential belongs
    -- to one account.
    credential_id BYTEA NOT NULL UNIQUE,
    name          TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 60),
    -- The public key, sign count and flags, as the library keeps them (never a private key).
    passkey       JSONB NOT NULL,
    sign_count    BIGINT NOT NULL DEFAULT 0,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at  TIMESTAMPTZ
);
CREATE INDEX webauthn_credentials_user_idx ON webauthn_credentials (user_id);

-- The state of a ceremony in flight: one row per registration (key `reg:<user>`) or per half
-- signed-in session (key `auth:<session>`). It is read once and deleted, and it expires.
CREATE TABLE webauthn_challenges (
    key        TEXT PRIMARY KEY,
    state      JSONB NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX webauthn_challenges_expiry_idx ON webauthn_challenges (expires_at);

-- Whether an account has a second factor: an authenticator app or a passkey. One definition, used
-- wherever the answer decides something (the sign-in step, the admin gate, Basic auth, apps).
CREATE FUNCTION ch_has_second_factor(uid BIGINT) RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
    SELECT EXISTS (SELECT 1 FROM users WHERE id = uid AND totp_enabled_at IS NOT NULL)
        OR EXISTS (SELECT 1 FROM webauthn_credentials WHERE user_id = uid)
$$;
