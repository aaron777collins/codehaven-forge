-- The toolkit's artifact service (ch-ctk.6, v4): an artifact is created, its blocks are sent to a
-- signed link, and it is finalized. The same table holds the unfinished upload as for caches; the
-- key is the artifact's name.
ALTER TABLE toolkit_uploads DROP CONSTRAINT toolkit_uploads_kind_check;
ALTER TABLE toolkit_uploads ADD CONSTRAINT toolkit_uploads_kind_check CHECK (kind IN ('cache', 'artifact'));
ALTER TABLE toolkit_uploads ADD COLUMN retention_days INT;
