-- Added by the identity stream. A team carried no permission of its own: the REST layer
-- answered "pull" for every team and the web page guessed the most its repositories grant.
-- The permission is now a column on the team, the way GitHub models it, and the per
-- repository rows in team_repos remain the overrides added on top of it.
ALTER TABLE teams
    ADD COLUMN permission TEXT NOT NULL DEFAULT 'read'
        CHECK (permission IN ('read', 'triage', 'write', 'maintain', 'admin'));
