-- Holds an organization member back from a repository their organization's base permission would
-- otherwise open to them (ch-ze4). When a private repository is deleted, the private forks that
-- hung below it must not open to people who could not see it: until now the lineage rule hid them
-- and the delete would have lifted that. A collaborator or team grant is withdrawn (ch-w3x); a
-- member's base permission cannot be taken back for one person, so a hold says "not through the
-- base permission" for that one person and that one fork. It never blocks an explicit grant made
-- later, and it ends when the member leaves the organization.
CREATE TABLE repo_access_holds (
    repo_id    BIGINT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repo_id, user_id)
);
CREATE INDEX repo_access_holds_user_idx ON repo_access_holds (user_id);
