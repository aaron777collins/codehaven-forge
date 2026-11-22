-- Container registry, audit log and blob reference counts.

CREATE TABLE packages (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_id   BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    repo_id    BIGINT REFERENCES repositories (id) ON DELETE SET NULL,
    kind       TEXT NOT NULL DEFAULT 'container' CHECK (kind IN ('container')),
    name       CITEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_id, kind, name)
);

CREATE TABLE oci_blobs (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    package_id BIGINT NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    digest     TEXT NOT NULL CHECK (digest ~ '^sha256:[0-9a-f]{64}$'),
    size       BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (package_id, digest)
);

CREATE TABLE oci_manifests (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    package_id BIGINT NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    digest     TEXT NOT NULL CHECK (digest ~ '^sha256:[0-9a-f]{64}$'),
    media_type TEXT NOT NULL,
    size       BIGINT NOT NULL,
    content    BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (package_id, digest)
);

CREATE TABLE oci_tags (
    package_id  BIGINT NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    manifest_id BIGINT NOT NULL REFERENCES oci_manifests (id) ON DELETE CASCADE,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (package_id, name)
);

CREATE TABLE oci_uploads (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    package_id   BIGINT NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    started_by   BIGINT REFERENCES users (id) ON DELETE SET NULL,
    offset_bytes BIGINT NOT NULL DEFAULT 0,
    tmp_path     TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Who did what. Rows are kept for a year. No foreign keys, so deleting an account
-- never deletes its history.
CREATE TABLE audit_events (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor_kind  TEXT NOT NULL,
    actor_id    BIGINT,
    actor_label TEXT NOT NULL,
    client_ip   INET,
    user_agent  TEXT,
    action      TEXT NOT NULL,
    target_type TEXT,
    target_id   TEXT,
    outcome     TEXT NOT NULL CHECK (outcome IN ('success', 'failure')),
    details     JSONB NOT NULL DEFAULT 'null'::jsonb
);
CREATE INDEX audit_events_at_idx ON audit_events (at DESC);
CREATE INDEX audit_events_actor_idx ON audit_events (actor_id, at DESC);
CREATE INDEX audit_events_action_idx ON audit_events (action, at DESC);
CREATE INDEX audit_events_ip_idx ON audit_events (client_ip, at DESC);

-- Reference counts for the content-addressed blob store. A daily job removes blobs
-- that have had no references for more than 24 hours.
CREATE TABLE blobs (
    sha256             TEXT PRIMARY KEY CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    size               BIGINT NOT NULL,
    refcount           INTEGER NOT NULL DEFAULT 0 CHECK (refcount >= 0),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    unreferenced_since TIMESTAMPTZ
);
CREATE INDEX blobs_unreferenced_idx ON blobs (unreferenced_since) WHERE refcount = 0;
