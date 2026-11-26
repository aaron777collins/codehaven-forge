-- Which release is the latest (ch-ctk.10). `marked_latest_at` is when a release was last made the
-- latest, by publishing it or by `make_latest`. `never_latest` is a release that was published
-- with `make_latest: false`. The latest is the newest mark among published, non-prerelease
-- releases that do not decline it; with no mark at all, the newest published one.
ALTER TABLE releases ADD COLUMN marked_latest_at TIMESTAMPTZ;
ALTER TABLE releases ADD COLUMN never_latest BOOLEAN NOT NULL DEFAULT false;

-- What was the latest before this column is what stays the latest.
UPDATE releases SET marked_latest_at = published_at WHERE NOT draft AND NOT prerelease AND published_at IS NOT NULL;
