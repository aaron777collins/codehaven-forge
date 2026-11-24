-- Codehaven Pages (epic ch-125, docs/design/pages.md). A site per repository, published copies
-- of one folder at one commit, and the files of each copy as blobs. Off for every repository
-- until its admin turns it on, and full mode off for the instance.

CREATE TABLE pages_settings (
    only_row  BOOLEAN PRIMARY KEY DEFAULT true CHECK (only_row),
    full_mode BOOLEAN NOT NULL DEFAULT false
);
INSERT INTO pages_settings DEFAULT VALUES;

CREATE TABLE pages_sites (
    repo_id    BIGINT PRIMARY KEY REFERENCES repositories (id) ON DELETE CASCADE,
    branch     TEXT NOT NULL CHECK (length(branch) BETWEEN 1 AND 255),
    folder     TEXT NOT NULL CHECK (folder IN ('', 'docs')),
    -- The widest audience the admin allows. Served at the lowest of this, the live copy's tier
    -- and the repository's tier now.
    audience   TEXT NOT NULL CHECK (audience IN ('private', 'internal', 'public')),
    live_id    BIGINT,
    created_by BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE pages_deployments (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id     BIGINT NOT NULL REFERENCES pages_sites (repo_id) ON DELETE CASCADE,
    commit_sha  TEXT NOT NULL CHECK (commit_sha ~ '^[0-9a-f]{40}([0-9a-f]{24})?$'),
    branch      TEXT NOT NULL,
    folder      TEXT NOT NULL CHECK (folder IN ('', 'docs')),
    -- The repository's tier when the copy was queued.
    tier        TEXT NOT NULL CHECK (tier IN ('private', 'internal', 'public')),
    state       TEXT NOT NULL CHECK (state IN ('queued', 'building', 'ready', 'failed')),
    error       TEXT,
    file_count  BIGINT NOT NULL DEFAULT 0,
    total_bytes BIGINT NOT NULL DEFAULT 0,
    created_by  BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ
);
CREATE INDEX pages_deployments_repo ON pages_deployments (repo_id, id DESC);
CREATE INDEX pages_deployments_queued ON pages_deployments (id) WHERE state = 'queued';

ALTER TABLE pages_sites
    ADD CONSTRAINT pages_sites_live_fk FOREIGN KEY (live_id) REFERENCES pages_deployments (id) ON DELETE SET NULL;

-- Paths are relative to the published folder, UTF-8, never starting with a slash and never
-- holding a `.` or `..` segment. The server looks them up exactly; nothing builds a file system
-- path from a request.
CREATE TABLE pages_files (
    deployment_id BIGINT NOT NULL REFERENCES pages_deployments (id) ON DELETE CASCADE,
    path          TEXT NOT NULL CHECK (length(path) BETWEEN 1 AND 1024 AND left(path, 1) <> '/'),
    blob_sha256   TEXT NOT NULL CHECK (blob_sha256 ~ '^[0-9a-f]{64}$'),
    size          BIGINT NOT NULL,
    PRIMARY KEY (deployment_id, path)
);

-- Every row holds one reference on its blob. Releasing it here covers each way a row goes:
-- a pruned copy, Pages turned off, the repository deleted.
CREATE FUNCTION pages_files_release() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE blobs SET refcount = GREATEST(refcount - 1, 0),
           unreferenced_since = CASE WHEN refcount <= 1 THEN now() ELSE unreferenced_since END
     WHERE sha256 = OLD.blob_sha256;
    RETURN OLD;
END $$;
CREATE TRIGGER pages_files_release AFTER DELETE ON pages_files FOR EACH ROW EXECUTE FUNCTION pages_files_release();
