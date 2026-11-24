-- Codehaven Pages (docs/design/pages.md, section 7): whether a file is valid UTF-8, found once
-- when the copy is made, so serving it can say its charset while streaming the file.
ALTER TABLE pages_files ADD COLUMN utf8 BOOLEAN NOT NULL DEFAULT false;
