-- A reset link a person asked for by email is marked, so a later request (which anyone may
-- make) replaces only such links and never cancels one an admin issued (ch-ev5).
ALTER TABLE one_time_links ADD COLUMN self_requested BOOLEAN NOT NULL DEFAULT false;
