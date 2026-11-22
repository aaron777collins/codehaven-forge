//! Live job logs over server-sent events.
//!
//! The hub only says "there is more". The log file is the truth, and each event carries the byte
//! offset reached as its id, so a viewer that drops and reconnects (the browser sends
//! `Last-Event-ID`) resumes exactly where it stopped, and a viewer that connects late first gets
//! everything already written.

use crate::ctx::Ctx;
use crate::logs::read_range;
use crate::shared::LogMsg;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use ch_core::access::CurrentActor;
use ch_core::state::AppState;
use ch_core::{Error, Permission, Result};
use futures::Stream;
use std::collections::VecDeque;
use std::convert::Infallible;
use tokio::sync::broadcast::{self, error::RecvError};

#[derive(serde::Deserialize, Default)]
pub struct FromQuery {
    from: Option<u64>,
}

struct St {
    ctx: Ctx,
    run: i64,
    job: i64,
    cursor: u64,
    rx: broadcast::Receiver<LogMsg>,
    queue: VecDeque<Event>,
    first: bool,
    finished: bool,
}

impl Drop for St {
    fn drop(&mut self) {
        // Our own receiver is still alive here, so "idle" means nobody else is watching.
        self.ctx.shared.hub.release_if_idle(self.job);
    }
}

impl St {
    /// Reads whatever whole lines are new and queues them as one event.
    async fn pull(&mut self) {
        loop {
            let Ok(bytes) = read_range(&self.ctx, self.run, self.job, self.cursor, None).await else { return };
            if bytes.is_empty() {
                return;
            }
            // Only whole lines: a line still being written waits for the next read.
            let Some(last_nl) = bytes.iter().rposition(|b| *b == b'\n') else { return };
            let text = String::from_utf8_lossy(&bytes[..last_nl]);
            let lines: Vec<&str> = text.split('\n').collect();
            self.cursor += (last_nl + 1) as u64;
            let data = serde_json::json!({ "offset": self.cursor, "lines": lines });
            self.queue.push_back(Event::default().event("log").id(self.cursor.to_string()).data(data.to_string()));
            if (bytes.len() as u64) < crate::logs::MAX_READ_BYTES {
                return;
            }
        }
    }

    fn done(&mut self) {
        self.queue.push_back(Event::default().event("done").id(self.cursor.to_string()).data("{}"));
        self.finished = true;
    }
}

pub async fn stream_logs(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(q): Query<FromQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    let repo = state.repos.get_by_name(&actor, &owner, &name).await?;
    state.authz.require(&actor, repo.id, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let row: Option<(i64, i64)> =
        sqlx::query_as("SELECT run_id, repo_id FROM workflow_jobs WHERE id = $1").bind(id).fetch_optional(&ctx.db).await?;
    let Some((run, repo_id)) = row else { return Err(Error::NotFound) };
    if repo_id != repo.id.0 {
        return Err(Error::NotFound);
    }
    let from = headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).or(q.from).unwrap_or(0);
    // Subscribe before reading the file, so a line written in between is not lost.
    let rx = ctx.shared.hub.subscribe(id);
    let st = St { ctx, run, job: id, cursor: from, rx, queue: VecDeque::new(), first: true, finished: false };
    let stream = futures::stream::unfold(st, |mut st| async move {
        loop {
            if let Some(ev) = st.queue.pop_front() {
                return Some((Ok(ev), st));
            }
            if st.finished {
                return None;
            }
            if st.first {
                st.first = false;
                st.pull().await;
                let status: Option<String> = sqlx::query_scalar("SELECT status FROM workflow_jobs WHERE id = $1")
                    .bind(st.job)
                    .fetch_optional(&st.ctx.db)
                    .await
                    .ok()
                    .flatten();
                if status.as_deref() != Some("in_progress") && status.as_deref() != Some("queued") && status.as_deref() != Some("waiting") {
                    st.pull().await;
                    st.done();
                }
                continue;
            }
            match st.rx.recv().await {
                Ok(LogMsg::Lines { .. }) | Err(RecvError::Lagged(_)) => st.pull().await,
                Ok(LogMsg::Done) | Err(RecvError::Closed) => {
                    st.pull().await;
                    st.done();
                }
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
