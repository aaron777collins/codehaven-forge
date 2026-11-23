-- Whether a pull request was opened from another repository (a fork), kept for its whole life.
--
-- `head_repo_id` alone cannot say so. A branch of the same repository is stored as NULL, and
-- deleting a fork sets its pull requests' `head_repo_id` to NULL too (ON DELETE SET NULL). CI
-- reads this flag, so a pull request whose fork was deleted still runs as a fork: held for
-- approval, no secrets, a read-only token.
--
-- The trigger sets it from every insert or update that names another repository, and nothing
-- ever clears it. A fork deleted before this migration ran left no trace, so its pull requests
-- read as same-repository ones.

ALTER TABLE pull_requests ADD COLUMN from_fork BOOLEAN NOT NULL DEFAULT false;
UPDATE pull_requests SET from_fork = true WHERE head_repo_id IS NOT NULL AND head_repo_id <> repo_id;

CREATE FUNCTION ch_pull_from_fork() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'UPDATE' AND OLD.from_fork THEN
    NEW.from_fork := true;
  END IF;
  IF NEW.head_repo_id IS NOT NULL AND NEW.head_repo_id <> NEW.repo_id THEN
    NEW.from_fork := true;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER pull_requests_from_fork BEFORE INSERT OR UPDATE ON pull_requests
    FOR EACH ROW EXECUTE FUNCTION ch_pull_from_fork();
