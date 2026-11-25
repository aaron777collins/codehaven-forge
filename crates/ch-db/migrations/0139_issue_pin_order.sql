-- The order of a repository's pinned issues (ch-u6j). Position 0 comes first.
ALTER TABLE issues ADD COLUMN pin_position INTEGER;
UPDATE issues i SET pin_position = r.pos
  FROM (SELECT id, row_number() OVER (PARTITION BY repo_id ORDER BY updated_at, number) - 1 AS pos FROM issues WHERE pinned) r
 WHERE i.id = r.id;
ALTER TABLE issues ADD CONSTRAINT issues_pin_position_matches CHECK ((pinned AND pin_position IS NOT NULL) OR (NOT pinned AND pin_position IS NULL));
