-- When a job last became queued (ch-7rq). The reaper fails a job that waits too long for a
-- runner: soon when no registered runner has its labels, after a day otherwise. A trigger keeps
-- it right for every path that queues a job (a run starting, a need finishing, a re-run, an
-- approval), so no caller can forget it.

ALTER TABLE workflow_jobs ADD COLUMN queued_at TIMESTAMPTZ;
UPDATE workflow_jobs SET queued_at = created_at WHERE status = 'queued';

CREATE FUNCTION ch_job_queued_at() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.status = 'queued' AND (TG_OP = 'INSERT' OR OLD.status IS DISTINCT FROM 'queued') THEN
        NEW.queued_at := now();
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER workflow_jobs_queued_at BEFORE INSERT OR UPDATE OF status ON workflow_jobs
    FOR EACH ROW EXECUTE FUNCTION ch_job_queued_at();
