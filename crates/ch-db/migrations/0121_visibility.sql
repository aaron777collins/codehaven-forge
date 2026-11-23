-- Three tiers of repository visibility (epic ch-4pn): private, internal and public.
--
-- `access_tier` is the column that decides. `visibility` stays, kept in step by a trigger, so an
-- older binary still reads and writes something it understands:
--   private  -> visibility 'private'
--   internal -> visibility 'public'  (what the old word meant: every signed-in account)
--   public   -> visibility 'public'  (anyone, with no account, while the instance allows it)
--
-- Nothing here can make a repository public. The only backfill above private is internal, the
-- instance switch starts off, a row can never be inserted public, and a row can only be raised to
-- public while the switch is on and `public_since` is set.

CREATE FUNCTION ch_tier_rank(t TEXT) RETURNS INT LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $$ SELECT CASE t WHEN 'public' THEN 2 WHEN 'internal' THEN 1 ELSE 0 END $$;

-- One row. The site admin's switch for public repositories.
CREATE TABLE visibility_settings (
    only_row           BOOLEAN PRIMARY KEY DEFAULT true CHECK (only_row),
    allow_public_repos BOOLEAN NOT NULL DEFAULT false,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO visibility_settings DEFAULT VALUES;

ALTER TABLE repositories
    ADD COLUMN access_tier TEXT NOT NULL DEFAULT 'private'
        CHECK (access_tier IN ('private', 'internal', 'public')),
    ADD COLUMN public_since TIMESTAMPTZ,
    ADD COLUMN public_by BIGINT REFERENCES users (id) ON DELETE SET NULL,
    -- Who may open issues, comment, react, review and send pull requests without a grant.
    -- No effect on a private repository, where only grants get in.
    ADD COLUMN contribution_policy TEXT NOT NULL DEFAULT 'everyone'
        CHECK (contribution_policy IN ('everyone', 'grants'));

-- The only mapping. No branch of it writes 'public'.
UPDATE repositories SET access_tier = 'internal' WHERE visibility = 'public';

-- A fork above its parent only narrows. Repeat until a whole chain is settled.
DO $$ BEGIN
  LOOP
    UPDATE repositories c SET access_tier = 'private', visibility = 'private'
      FROM repositories p
     WHERE c.fork_parent_id = p.id AND c.access_tier <> 'private' AND p.access_tier = 'private';
    EXIT WHEN NOT FOUND;
  END LOOP;
END $$;

ALTER TABLE repositories
    ADD CONSTRAINT repo_tier_matches_legacy CHECK ((visibility = 'private') = (access_tier = 'private')),
    ADD CONSTRAINT repo_public_is_confirmed CHECK (access_tier <> 'public' OR public_since IS NOT NULL);

CREATE FUNCTION ch_repo_tier_before() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE parent TEXT; allowed BOOLEAN;
BEGIN
  IF NEW.visibility IS NULL OR NEW.visibility NOT IN ('public', 'private') THEN
    RAISE EXCEPTION 'visibility must be public or private' USING ERRCODE = 'check_violation';
  END IF;
  -- A write by an older binary names only `visibility`. It can mean private or internal, never public.
  IF TG_OP = 'INSERT' THEN
    IF NEW.access_tier = 'private' AND NEW.visibility = 'public' THEN NEW.access_tier := 'internal'; END IF;
  ELSIF NEW.access_tier = OLD.access_tier AND NEW.visibility <> OLD.visibility THEN
    NEW.access_tier := CASE NEW.visibility WHEN 'private' THEN 'private' ELSE 'internal' END;
  END IF;
  -- A fork is never more visible than its parent. Checked only when the tier or the parent
  -- changes, so an ordinary update (a push) takes no lock on the parent.
  IF NEW.fork_parent_id IS NOT NULL AND ch_tier_rank(NEW.access_tier) > 0
     AND (TG_OP = 'INSERT' OR NEW.access_tier IS DISTINCT FROM OLD.access_tier
          OR NEW.fork_parent_id IS DISTINCT FROM OLD.fork_parent_id) THEN
    SELECT access_tier INTO parent FROM repositories WHERE id = NEW.fork_parent_id FOR SHARE;
    IF FOUND AND ch_tier_rank(NEW.access_tier) > ch_tier_rank(parent) THEN NEW.access_tier := parent; END IF;
  END IF;
  IF NEW.access_tier = 'public' THEN
    IF TG_OP = 'INSERT' THEN
      RAISE EXCEPTION 'a repository cannot be created public' USING ERRCODE = 'check_violation';
    ELSIF OLD.access_tier <> 'public' THEN
      SELECT allow_public_repos INTO allowed FROM visibility_settings FOR SHARE;
      IF allowed IS NOT TRUE THEN
        RAISE EXCEPTION 'public repositories are off' USING ERRCODE = 'check_violation';
      END IF;
    END IF;
  ELSE
    NEW.public_since := NULL; NEW.public_by := NULL;
  END IF;
  NEW.visibility := CASE NEW.access_tier WHEN 'private' THEN 'private' ELSE 'public' END;
  RETURN NEW;
END $$;
CREATE TRIGGER repositories_tier_before BEFORE INSERT OR UPDATE ON repositories
    FOR EACH ROW EXECUTE FUNCTION ch_repo_tier_before();

-- Lowering a repository lowers every fork below it, in the same transaction. Raising one raises none.
CREATE FUNCTION ch_repo_tier_after() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  UPDATE repositories SET access_tier = NEW.access_tier
   WHERE fork_parent_id = NEW.id AND ch_tier_rank(access_tier) > ch_tier_rank(NEW.access_tier);
  RETURN NULL;
END $$;
CREATE TRIGGER repositories_tier_after AFTER UPDATE ON repositories
    FOR EACH ROW WHEN (OLD.access_tier IS DISTINCT FROM NEW.access_tier)
    EXECUTE FUNCTION ch_repo_tier_after();

-- Turning the switch off lowers every public repository to internal. Turning it on raises none.
CREATE FUNCTION ch_public_switch_after() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.allow_public_repos AND NOT NEW.allow_public_repos THEN
    UPDATE repositories SET access_tier = 'internal' WHERE access_tier = 'public';
  END IF;
  RETURN NULL;
END $$;
CREATE TRIGGER visibility_settings_switch_after AFTER UPDATE ON visibility_settings
    FOR EACH ROW EXECUTE FUNCTION ch_public_switch_after();

CREATE INDEX repositories_public_idx ON repositories (owner_id, name) WHERE access_tier = 'public';

-- An old name records who could see the repository when it was renamed or moved. A signed-out
-- visitor follows it only when that was public, so publishing never reveals an earlier private name.
ALTER TABLE repo_redirects ADD COLUMN audience TEXT NOT NULL DEFAULT 'private'
    CHECK (audience IN ('private', 'internal', 'public'));
UPDATE repo_redirects x SET audience = r.access_tier FROM repositories r WHERE r.id = x.repo_id;
CREATE FUNCTION ch_redirect_audience() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  SELECT access_tier INTO NEW.audience FROM repositories WHERE id = NEW.repo_id;
  IF NEW.audience IS NULL THEN NEW.audience := 'private'; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER repo_redirects_audience BEFORE INSERT OR UPDATE ON repo_redirects
    FOR EACH ROW EXECUTE FUNCTION ch_redirect_audience();

-- The tier a pull request's author agreed their fork's commits would be shown at. Read by the
-- copy rule of a later stage. NULL means no consent was recorded.
ALTER TABLE pull_requests ADD COLUMN head_publish_tier TEXT CHECK (head_publish_tier IN ('private', 'internal', 'public'));
UPDATE pull_requests x SET head_publish_tier = r.access_tier FROM repositories r WHERE r.id = x.repo_id AND x.head_repo_id IS NOT NULL;

-- CI records keep the tier they were made at. Logs written for a smaller audience stay with it
-- after the repository is widened. NULL reads as private.
ALTER TABLE workflow_runs   ADD COLUMN recorded_tier TEXT CHECK (recorded_tier IN ('private', 'internal', 'public'));
ALTER TABLE check_runs      ADD COLUMN recorded_tier TEXT CHECK (recorded_tier IN ('private', 'internal', 'public'));
ALTER TABLE commit_statuses ADD COLUMN recorded_tier TEXT CHECK (recorded_tier IN ('private', 'internal', 'public'));
UPDATE workflow_runs x   SET recorded_tier = r.access_tier FROM repositories r WHERE r.id = x.repo_id;
UPDATE check_runs x      SET recorded_tier = r.access_tier FROM repositories r WHERE r.id = x.repo_id;
UPDATE commit_statuses x SET recorded_tier = r.access_tier FROM repositories r WHERE r.id = x.repo_id;

CREATE FUNCTION ch_recorded_tier() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.recorded_tier IS NULL AND TG_TABLE_NAME = 'check_runs' THEN
    SELECT w.recorded_tier INTO NEW.recorded_tier
      FROM workflow_jobs j JOIN workflow_runs w ON w.id = j.run_id
     WHERE j.id = (to_jsonb(NEW) ->> 'job_id')::bigint;
  END IF;
  IF NEW.recorded_tier IS NULL THEN
    SELECT access_tier INTO NEW.recorded_tier FROM repositories WHERE id = NEW.repo_id;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER workflow_runs_tier   BEFORE INSERT ON workflow_runs   FOR EACH ROW EXECUTE FUNCTION ch_recorded_tier();
CREATE TRIGGER check_runs_tier      BEFORE INSERT ON check_runs      FOR EACH ROW EXECUTE FUNCTION ch_recorded_tier();
CREATE TRIGGER commit_statuses_tier BEFORE INSERT ON commit_statuses FOR EACH ROW EXECUTE FUNCTION ch_recorded_tier();
