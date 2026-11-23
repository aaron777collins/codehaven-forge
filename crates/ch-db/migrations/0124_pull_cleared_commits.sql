-- Commits of a pull request that CI let run (ch-x12).
--
-- Update branch, an applied suggestion and a web edit write a new commit for a person on top of
-- the head branch. That commit holds the old head's code too, which someone else may have pushed
-- and which may still wait for approval. The person vouches for their own change only, so CI
-- counts them for the new commit only when the commit under it is listed here: CI ran it at once
-- because its author and pusher could write, or someone with write access approved it.
-- A commit missing from here (from before this table, or one CI never judged) means CI waits.

CREATE TABLE pull_cleared_commits (
    pull_id    BIGINT NOT NULL REFERENCES pull_requests (issue_id) ON DELETE CASCADE,
    head_sha   TEXT NOT NULL,
    cleared_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (pull_id, head_sha)
);
