-- Auto-merge is armed for one head commit (ch-m2y). A later head keeps it armed only when someone
-- with write access on the base pushed it; anything else clears it. Armed rows keep the head they
-- have now.
ALTER TABLE pull_requests ADD COLUMN auto_merge_head_sha TEXT;
UPDATE pull_requests SET auto_merge_head_sha = head_sha WHERE auto_merge_method IS NOT NULL;

-- A package whose repository is deleted or moved to another owner belongs to its owner alone
-- from then on, never to every member of an organization (ch-ctk.12). The repository row is
-- marked here before the foreign key clears `repo_id`, so no way of deleting a repository can
-- skip it.
ALTER TABLE packages ADD COLUMN orphaned BOOLEAN NOT NULL DEFAULT false;

CREATE FUNCTION packages_orphan_on_repo_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE packages SET orphaned = true WHERE repo_id = OLD.id;
    RETURN OLD;
END
$$;

CREATE TRIGGER repositories_orphan_packages BEFORE DELETE ON repositories
    FOR EACH ROW EXECUTE FUNCTION packages_orphan_on_repo_delete();

CREATE FUNCTION packages_orphan_on_repo_owner_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.owner_id IS DISTINCT FROM OLD.owner_id THEN
        UPDATE packages SET orphaned = true, repo_id = NULL WHERE repo_id = OLD.id;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER repositories_orphan_packages_on_transfer AFTER UPDATE OF owner_id ON repositories
    FOR EACH ROW EXECUTE FUNCTION packages_orphan_on_repo_owner_change();
