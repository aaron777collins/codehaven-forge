//! Job logs: appended to `data/logs/<run>/<job>.log`, with each step's byte range recorded, a
//! broadcast to anyone watching live, and reading by byte range.
//!
//! The runner is the only writer of step output and of the `##[group]` lines around it. A step's
//! range comes from the step number on each batch, never from the separate step-update calls:
//! those arrive at a different time from the batches, and placing both by arrival order put one
//! step's lines in the next step's panel.
//!
//! The runner sends batches numbered by `seq`. A batch the server already has is acknowledged
//! and dropped, a batch from the future is refused with the number expected, so a retry after a
//! lost response never duplicates or reorders lines.

use crate::ctx::Ctx;
use crate::runs::{JobRow, RunRow};
use crate::shared::LogMsg;
use bytes::Bytes;
use ch_api_types::runner::{LogAck, LogBatch};
use ch_core::{Error, Result};
use std::io::SeekFrom;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// A job's log stops growing here. A runaway `yes` should not fill the disk.
pub const MAX_LOG_BYTES: u64 = 64 << 20;
/// One line is cut at this length.
pub const MAX_LINE_BYTES: usize = 64 << 10;
/// Most lines in one batch.
pub const MAX_BATCH_LINES: usize = 20_000;
/// Most bytes one read returns.
pub const MAX_READ_BYTES: u64 = 8 << 20;

pub fn log_path(ctx: &Ctx, run: i64, job: i64) -> PathBuf {
    ctx.config.logs_dir().join(run.to_string()).join(format!("{job}.log"))
}

fn clean_line(l: &str) -> String {
    // A log line never carries a bare newline: the file format is one line per entry.
    let mut s: String = l.replace(['\n', '\r', '\0'], " ");
    if s.len() > MAX_LINE_BYTES {
        let mut cut = MAX_LINE_BYTES;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str(" [line cut]");
    }
    s
}

/// Appends text to a job's log file and returns the byte range it now occupies. The caller
/// holds the job row lock, so appends to one job never interleave.
async fn write_log(ctx: &Ctx, run: i64, job: i64, text: &str) -> std::io::Result<(u64, u64)> {
    let path = log_path(ctx, run, job);
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await?;
    let start = f.metadata().await?.len();
    f.write_all(text.as_bytes()).await?;
    f.flush().await?;
    Ok((start, start + text.len() as u64))
}

/// A line the server itself wants in the log, such as "The runner stopped responding". Best
/// effort: a failure here is logged, not raised, because it is already explaining a failure.
pub async fn append_system_line(ctx: &Ctx, run: &RunRow, job: &JobRow, line: &str) {
    let text = format!("{}\n", clean_line(line));
    match write_log(ctx, run.id, job.id, &text).await {
        Ok((_, end)) => ctx.shared.hub.publish(job.id, LogMsg::Lines { step: 0, lines: vec![clean_line(line)], end }),
        Err(e) => tracing::warn!(job = job.id, error = %e, "could not write a log line"),
    }
}

/// Takes one batch from a runner. `runner` is the runner that must own the job.
pub async fn append_batch(ctx: &Ctx, job_id: i64, runner: i64, batch: LogBatch) -> Result<(LogAck, bool)> {
    if batch.lines.len() > MAX_BATCH_LINES {
        return Err(Error::invalid("Log", "lines", format!("a batch holds at most {MAX_BATCH_LINES} lines")));
    }
    let mut tx = ctx.db.begin().await?;
    let row: Option<(i64, i64, String, serde_json::Value)> =
        sqlx::query_as("SELECT run_id, COALESCE(runner_id, 0), status, spec FROM workflow_jobs WHERE id = $1 FOR UPDATE")
            .bind(job_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((run_id, owner, status, spec)) = row else { return Err(Error::NotFound) };
    if owner != runner {
        return Err(Error::NotFound);
    }
    if status != "in_progress" {
        return Err(Error::conflict("This job is not running."));
    }
    let next = spec.get("log_seq").and_then(|v| v.as_u64()).unwrap_or(0);
    if batch.seq < next {
        return Ok((LogAck { next_seq: next }, true));
    }
    if batch.seq > next {
        return Ok((LogAck { next_seq: next }, false));
    }
    let lines: Vec<String> = batch.lines.iter().map(|l| clean_line(l)).collect();
    let mut text = String::new();
    for l in &lines {
        text.push_str(l);
        text.push('\n');
    }
    let size = tokio::fs::metadata(log_path(ctx, run_id, job_id)).await.map(|m| m.len()).unwrap_or(0);
    let truncated = spec.get("log_truncated").and_then(|v| v.as_bool()).unwrap_or(false);
    let (published, range) = if size + text.len() as u64 > MAX_LOG_BYTES {
        if truncated {
            (Vec::new(), None)
        } else {
            let note = "##[warning]The log limit was reached. The rest of this job's output is not kept.";
            sqlx::query("UPDATE workflow_jobs SET spec = jsonb_set(spec, '{log_truncated}', 'true') WHERE id = $1")
                .bind(job_id)
                .execute(&mut *tx)
                .await?;
            let r = write_log(ctx, run_id, job_id, &format!("{note}\n")).await.map_err(Error::internal)?;
            (vec![note.to_string()], Some(r))
        }
    } else if text.is_empty() {
        (Vec::new(), None)
    } else {
        let r = write_log(ctx, run_id, job_id, &text).await.map_err(Error::internal)?;
        (lines, Some(r))
    };
    if let Some((start, end)) = range {
        if batch.step > 0 {
            sqlx::query("UPDATE workflow_steps SET log_start = COALESCE(log_start, $3), log_end = $4 WHERE job_id = $1 AND number = $2")
                .bind(job_id)
                .bind(batch.step)
                .bind(start as i64)
                .bind(end as i64)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE workflow_jobs SET log_path = COALESCE(log_path, $2) WHERE id = $1")
            .bind(job_id)
            .bind(format!("{run_id}/{job_id}.log"))
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE workflow_jobs SET spec = jsonb_set(spec, '{log_seq}', to_jsonb($2::bigint)) WHERE id = $1")
        .bind(job_id)
        .bind((next + 1) as i64)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    if let Some((_, end)) = range {
        ctx.shared.hub.publish(job_id, LogMsg::Lines { step: batch.step, lines: published, end });
    }
    Ok((LogAck { next_seq: next + 1 }, true))
}

/// The size of a job's log file, 0 when there is none yet.
pub async fn log_len(ctx: &Ctx, run: i64, job: i64) -> Result<u64> {
    match tokio::fs::metadata(log_path(ctx, run, job)).await {
        Ok(m) => Ok(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(Error::internal(e)),
    }
}

/// Bytes `from..to` of a job's log (to the end when `to` is `None`), at most 8 MiB.
pub async fn read_range(ctx: &Ctx, run: i64, job: i64, from: u64, to: Option<u64>) -> Result<Bytes> {
    let path = log_path(ctx, run, job);
    let mut f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Bytes::new()),
        Err(e) => return Err(Error::internal(e)),
    };
    let len = f.metadata().await.map_err(Error::internal)?.len();
    let end = to.unwrap_or(len).min(len).min(from.saturating_add(MAX_READ_BYTES));
    if from >= end {
        return Ok(Bytes::new());
    }
    f.seek(SeekFrom::Start(from)).await.map_err(Error::internal)?;
    let mut buf = vec![0u8; (end - from) as usize];
    f.read_exact(&mut buf).await.map_err(Error::internal)?;
    Ok(Bytes::from(buf))
}

/// Deletes the log files of runs older than `days`. Returns how many runs lost their logs.
pub async fn purge_old(ctx: &Ctx, days: u32) -> Result<u64> {
    let runs: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM workflow_runs WHERE status = 'completed' AND completed_at < now() - make_interval(days => $1)
           AND EXISTS (SELECT 1 FROM workflow_jobs j WHERE j.run_id = workflow_runs.id AND j.log_path IS NOT NULL)",
    )
    .bind(days as i32)
    .fetch_all(&ctx.db)
    .await?;
    let mut n = 0;
    for r in runs {
        let dir = ctx.config.logs_dir().join(r.to_string());
        if tokio::fs::remove_dir_all(&dir).await.is_ok() {
            n += 1;
        }
        sqlx::query("UPDATE workflow_jobs SET log_path = NULL WHERE run_id = $1").bind(r).execute(&ctx.db).await?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_never_hold_a_newline_or_grow_without_bound() {
        assert_eq!(clean_line("a\nb\r\nc"), "a b  c");
        let long = "é".repeat(MAX_LINE_BYTES);
        let c = clean_line(&long);
        assert!(c.len() <= MAX_LINE_BYTES + 12);
        assert!(c.ends_with("[line cut]"));
        assert!(c.is_char_boundary(c.len()));
        assert_eq!(clean_line("plain"), "plain");
    }
}
