-- Why a package has no repository (ch-0kp): it was unlinked by its owner, its repository was
-- deleted, or its repository moved to another owner. A package that was never linked has no
-- reason and is not orphaned. Rows orphaned before this keep a null reason, and the page words
-- those for any of the causes.
ALTER TABLE packages ADD COLUMN orphan_reason TEXT CHECK (orphan_reason IN ('unlinked', 'repo_deleted', 'repo_moved'));

CREATE OR REPLACE FUNCTION packages_orphan_on_repo_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE packages SET orphaned = true, orphan_reason = 'repo_deleted' WHERE repo_id = OLD.id;
    RETURN OLD;
END
$$;

CREATE OR REPLACE FUNCTION packages_orphan_on_repo_owner_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.owner_id IS DISTINCT FROM OLD.owner_id THEN
        UPDATE packages SET orphaned = true, orphan_reason = 'repo_moved', repo_id = NULL WHERE repo_id = OLD.id;
    END IF;
    RETURN NEW;
END
$$;
