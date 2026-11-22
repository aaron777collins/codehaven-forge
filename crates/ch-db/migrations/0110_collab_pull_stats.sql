-- Added by the collab stream. A pull request list shows commit and line counts for every
-- row, and asking git for each one per page view would be slow. They are refreshed whenever
-- the head or base moves. `auto_merge_by` remembers who turned auto-merge on, because the
-- merge is made in their name. The version number leaves room for the other streams'
-- migrations that land first.
ALTER TABLE pull_requests
    ADD COLUMN commits_count INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN additions     INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN deletions     INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN changed_files INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN auto_merge_by BIGINT REFERENCES users (id) ON DELETE SET NULL;

-- Default review comments are listed per pull request in time order, and a reply looks up its
-- thread, so these two cover the hot paths.
CREATE INDEX review_comments_thread_idx ON review_comments (thread_id);
CREATE INDEX reviews_author_idx ON reviews (author_id);
