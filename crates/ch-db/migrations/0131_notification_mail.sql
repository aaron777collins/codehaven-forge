-- Notification emails (ch-0km). Each person chooses; off until they turn it on. The queue
-- survives restarts, and the sender checks again at send time that the person may still see
-- the repository, so an email never outlives the access that caused it.
ALTER TABLE users ADD COLUMN notify_email BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE notification_mail_queue (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    repo_id      BIGINT REFERENCES repositories (id) ON DELETE CASCADE,
    reason       TEXT NOT NULL,
    subject_type TEXT NOT NULL,
    subject_id   BIGINT NOT NULL,
    title        TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempts     INT NOT NULL DEFAULT 0,
    next_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    done_at      TIMESTAMPTZ,
    outcome      TEXT
);
CREATE INDEX notification_mail_queue_due ON notification_mail_queue (next_at) WHERE done_at IS NULL;
