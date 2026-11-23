-- Approval of a fork's pull request comes before anything else (ch-x12).
--
-- Until now a held fork run was created at once with status 'waiting': its run-name was worked
-- out with the repository's variables and shown to anyone, its jobs and check runs existed, and
-- approving it was per run. Now nothing is created for a commit of a pull request from a fork,
-- unless its author and its pusher both have write access, until someone with write access
-- approves that exact commit. A row here is the request (approved_at NULL) and then the
-- approval. A new commit needs a new one.

CREATE TABLE fork_run_approvals (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id      BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    pull_id      BIGINT NOT NULL REFERENCES pull_requests (issue_id) ON DELETE CASCADE,
    head_sha     TEXT NOT NULL,
    -- The event that asked, replayed when the commit is approved.
    action       TEXT NOT NULL,
    trigger_id   TEXT NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- approved_at says whether it is approved; approved_by may later go NULL with the account.
    approved_by  BIGINT REFERENCES users (id) ON DELETE SET NULL,
    approved_at  TIMESTAMPTZ,
    UNIQUE (pull_id, head_sha)
);
CREATE INDEX fork_run_approvals_repo_idx ON fork_run_approvals (repo_id);

-- Fork runs still waiting under the old scheme are cancelled, with their jobs, steps and check
-- runs. Their commit can be approved again on the pull request once it is pushed or updated.
UPDATE check_runs SET status = 'completed', conclusion = 'cancelled', completed_at = now()
 WHERE status <> 'completed'
   AND job_id IN (SELECT j.id FROM workflow_jobs j JOIN workflow_runs r ON r.id = j.run_id
                   WHERE r.status = 'waiting' AND COALESCE((r.event_payload->'meta'->>'fork')::boolean, false));
UPDATE workflow_steps SET status = 'completed', conclusion = 'skipped'
 WHERE status = 'queued'
   AND job_id IN (SELECT j.id FROM workflow_jobs j JOIN workflow_runs r ON r.id = j.run_id
                   WHERE r.status = 'waiting' AND COALESCE((r.event_payload->'meta'->>'fork')::boolean, false));
UPDATE workflow_jobs SET status = 'completed', conclusion = 'cancelled', completed_at = now()
 WHERE status <> 'completed'
   AND run_id IN (SELECT id FROM workflow_runs
                   WHERE status = 'waiting' AND COALESCE((event_payload->'meta'->>'fork')::boolean, false));
UPDATE workflow_runs SET status = 'completed', conclusion = 'cancelled', completed_at = now()
 WHERE status = 'waiting' AND COALESCE((event_payload->'meta'->>'fork')::boolean, false);
