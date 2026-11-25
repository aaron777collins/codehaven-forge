-- Uploads the Actions toolkit makes in pieces (ch-ctk.6): a cache reserved, then sent in ranges,
-- then committed. Gone when its job is deleted; the janitor drops any left unfinished.
CREATE TABLE toolkit_uploads (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_id      BIGINT NOT NULL REFERENCES workflow_jobs (id) ON DELETE CASCADE,
    kind        TEXT NOT NULL CHECK (kind IN ('cache')),
    key         TEXT NOT NULL,
    version     TEXT NOT NULL,
    git_ref     TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX toolkit_uploads_job ON toolkit_uploads (job_id);
