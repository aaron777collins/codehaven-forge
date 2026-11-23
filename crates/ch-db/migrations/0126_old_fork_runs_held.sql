-- Fork runs held under the old scheme carry no hold reason (ch-3xn). Migration 0123 cancelled
-- them, and a re-run checks the hold reason to refuse a run nobody approved, so one of these
-- could be started again without approval. Mark each one that never started and that nobody
-- approved as held, so a re-run refuses it like any other run that was never approved.

UPDATE workflow_runs
   SET event_payload = jsonb_set(event_payload, '{meta,hold_reason}', '"Pull request from a fork"')
 WHERE COALESCE((event_payload->'meta'->>'fork')::boolean, false)
   AND status = 'completed' AND conclusion = 'cancelled' AND started_at IS NULL
   AND event_payload->'meta'->>'approved_by' IS NULL
   AND event_payload->'meta'->>'hold_reason' IS NULL;
