-- The head the branch tracker last announced (ch-hyd). `head_sha` is moved by any page or merge
-- that refreshes a pull request, so the tracker compares against this instead and a push that
-- a refresh saw first still gets its `synchronize` event and its CI run.
ALTER TABLE pull_requests ADD COLUMN synced_head_sha TEXT;
UPDATE pull_requests SET synced_head_sha = head_sha;

CREATE FUNCTION ch_pull_synced_head() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.synced_head_sha IS NULL THEN
    NEW.synced_head_sha := NEW.head_sha;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER pull_requests_synced_head BEFORE INSERT ON pull_requests FOR EACH ROW EXECUTE FUNCTION ch_pull_synced_head();
