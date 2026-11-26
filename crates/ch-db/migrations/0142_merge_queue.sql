-- A simple merge queue (ch-dxk). A repository that turns it on merges pull requests only through
-- the queue: they go in one at a time, each brought up to date with its base and checked, and merge
-- once every required check passes on that result. Nothing here changes how an older image merges.
ALTER TABLE repositories ADD COLUMN merge_queue BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE merge_queue_entries (
    issue_id    BIGINT PRIMARY KEY REFERENCES issues (id) ON DELETE CASCADE,
    repo_id     BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    base_ref    TEXT NOT NULL,
    method      TEXT NOT NULL CHECK (method IN ('merge', 'squash', 'rebase')),
    enqueued_by BIGINT REFERENCES users (id) ON DELETE SET NULL,
    enqueued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The head commit the queue last saw or made. A push by anyone else changes it, which takes
    -- the pull request out of the queue.
    head_sha    TEXT NOT NULL,
    -- What the queue is waiting for, in words for the pull request page.
    note        TEXT NOT NULL DEFAULT ''
);
CREATE INDEX merge_queue_entries_line_idx ON merge_queue_entries (repo_id, base_ref, enqueued_at, issue_id);
