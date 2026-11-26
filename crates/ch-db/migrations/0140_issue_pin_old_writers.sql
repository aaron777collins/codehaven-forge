-- The image before 0139 pins and unpins with `UPDATE issues SET pinned = ...` alone, and
-- 0139's CHECK wants pin_position to agree with it. After a rollback every pin and unpin on that
-- image would fail (ch-c7c). This trigger fills in what an older writer leaves out: a pinned
-- issue with no position goes last, and an issue that is not pinned has none. Code that sets
-- both (this version) is unaffected. It runs before the CHECK does.
CREATE FUNCTION issues_pin_position_fill() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NOT NEW.pinned THEN
    NEW.pin_position := NULL;
  ELSIF NEW.pin_position IS NULL THEN
    NEW.pin_position := COALESCE((SELECT max(i.pin_position) + 1 FROM issues i WHERE i.repo_id = NEW.repo_id AND i.pinned AND i.id <> NEW.id), 0);
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER issues_pin_position_fill BEFORE INSERT OR UPDATE OF pinned, pin_position ON issues
  FOR EACH ROW EXECUTE FUNCTION issues_pin_position_fill();
