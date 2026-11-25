-- Job token permissions (ch-ctk.8): what each job's token may do, worked out from the
-- workflow's `permissions:` when the job is created. NULL for jobs made before this, which keep
-- the restricted default (read contents and packages).
ALTER TABLE workflow_jobs ADD COLUMN token_permissions JSONB;

-- A repository admin may cap every job token at read.
ALTER TABLE repositories ADD COLUMN actions_token_ceiling TEXT NOT NULL DEFAULT 'write'
    CHECK (actions_token_ceiling IN ('read', 'write'));

-- The account a job's changes are recorded as. It has no password, email, token or key, so it
-- can never sign in, and it owns nothing. If a person already has the name, no bot is made and
-- jobs record changes with no account, as before; that person is never treated as the bot.
INSERT INTO users (kind, username, display_name, description)
VALUES ('bot', 'codehaven-actions', 'Codehaven Actions', 'Changes made by workflow jobs.')
ON CONFLICT (username) DO NOTHING;
