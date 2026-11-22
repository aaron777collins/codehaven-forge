-- Connected apps: OAuth sign-in for MCP clients such as Claude. New tables only. Nothing here
-- alters or backfills an existing table, so an older binary keeps working on this schema.

CREATE TABLE oauth_grants (
 id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
 client_id TEXT NOT NULL, client_host TEXT NOT NULL, client_name TEXT NOT NULL,
 redirect_uri TEXT NOT NULL, resource TEXT NOT NULL,
 scopes TEXT[] NOT NULL CHECK (scopes <@ ARRAY['read','write','workflows']::text[] AND 'read' = ANY(scopes)),
 read_all BOOLEAN NOT NULL DEFAULT false,
 write_all BOOLEAN NOT NULL DEFAULT false,
 workflows_all BOOLEAN NOT NULL DEFAULT false,
 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 activated_at TIMESTAMPTZ, last_used_at TIMESTAMPTZ,
 expires_at TIMESTAMPTZ NOT NULL, revoked_at TIMESTAMPTZ, revoked_reason TEXT,
 CHECK (NOT write_all OR (read_all AND 'write' = ANY(scopes))),
 CHECK (NOT workflows_all OR (write_all AND 'workflows' = ANY(scopes))),
 CHECK (NOT ('workflows' = ANY(scopes)) OR 'write' = ANY(scopes)));
CREATE INDEX oauth_grants_user_idx ON oauth_grants(user_id) WHERE revoked_at IS NULL;

CREATE TABLE oauth_grant_repos (
 grant_id BIGINT NOT NULL REFERENCES oauth_grants(id) ON DELETE CASCADE,
 repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
 can_write BOOLEAN NOT NULL DEFAULT false,
 can_workflows BOOLEAN NOT NULL DEFAULT false,
 PRIMARY KEY (grant_id, repo_id), CHECK (NOT can_workflows OR can_write));
CREATE INDEX oauth_grant_repos_repo_idx ON oauth_grant_repos(repo_id);

CREATE TABLE oauth_requests (
 id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 request_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(request_hash)=32),
 session_hash BYTEA NOT NULL CHECK (octet_length(session_hash)=32),
 user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
 client_id TEXT NOT NULL, client_name TEXT NOT NULL, redirect_uri TEXT NOT NULL,
 state TEXT, scopes TEXT[] NOT NULL, resource TEXT NOT NULL, code_challenge TEXT NOT NULL,
 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), expires_at TIMESTAMPTZ NOT NULL, used_at TIMESTAMPTZ);
CREATE INDEX oauth_requests_user_idx ON oauth_requests(user_id, expires_at);

CREATE TABLE oauth_codes (
 id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 code_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(code_hash)=32),
 grant_id BIGINT NOT NULL REFERENCES oauth_grants(id) ON DELETE CASCADE,
 redirect_uri TEXT NOT NULL, code_challenge TEXT NOT NULL,
 expires_at TIMESTAMPTZ NOT NULL, used_at TIMESTAMPTZ);

CREATE TABLE oauth_tokens (
 id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 grant_id BIGINT NOT NULL REFERENCES oauth_grants(id) ON DELETE CASCADE,
 kind TEXT NOT NULL CHECK (kind IN ('access','refresh')),
 token_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash)=32),
 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 expires_at TIMESTAMPTZ NOT NULL, used_at TIMESTAMPTZ, revoked_at TIMESTAMPTZ);
CREATE INDEX oauth_tokens_grant_idx ON oauth_tokens(grant_id);
CREATE INDEX oauth_tokens_expiry_idx ON oauth_tokens(expires_at);
