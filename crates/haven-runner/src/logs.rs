//! Job logs: assembling lines from raw output, masking them, and shipping them to the server in
//! batches (every 500 ms or 64 KB, whichever comes first).

use crate::client::Client;
use crate::mask::Masker;
use ch_api_types::runner::LogBatch;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Longest line the log keeps whole. A longer one is cut into pieces of this size.
pub const MAX_LINE: usize = 64 * 1024;
const BATCH_BYTES: usize = 64 * 1024;
const BATCH_EVERY: Duration = Duration::from_millis(500);

/// Turns chunks of output into lines. Output from the two streams is kept apart, so half a
/// line on stderr never glues onto half a line on stdout.
#[derive(Default)]
pub struct LineSplitter {
    out: Vec<u8>,
    err: Vec<u8>,
}

impl LineSplitter {
    pub fn push(&mut self, stderr: bool, data: &[u8]) -> Vec<String> {
        let buf = if stderr { &mut self.err } else { &mut self.out };
        buf.extend_from_slice(data);
        let mut lines = Vec::new();
        while let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let rest = buf.split_off(i + 1);
            buf.pop();
            let line = std::mem::replace(buf, rest);
            push_line(&mut lines, &line);
        }
        // A line with no end in sight is cut rather than held forever.
        while buf.len() >= MAX_LINE {
            let rest = buf.split_off(MAX_LINE);
            let line = std::mem::replace(buf, rest);
            push_line(&mut lines, &line);
        }
        lines
    }

    /// What is left when the stream ends: a final line with no newline.
    pub fn finish(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        for buf in [&mut self.out, &mut self.err] {
            if !buf.is_empty() {
                let line = std::mem::take(buf);
                push_line(&mut lines, &line);
            }
        }
        lines
    }
}

fn push_line(lines: &mut Vec<String>, raw: &[u8]) {
    let s = String::from_utf8_lossy(raw);
    let s = s.strip_suffix('\r').unwrap_or(&s);
    // A progress bar redraws one line with carriage returns. Keep the last drawing.
    let s = match s.rfind('\r') {
        Some(i) => &s[i + 1..],
        None => s,
    };
    lines.push(s.to_string());
}

enum Msg {
    Lines { step: i32, lines: Vec<String> },
    Flush(oneshot::Sender<()>),
}

/// The handle the job engine writes lines to. Cheap to clone.
#[derive(Clone)]
pub struct LogWriter {
    tx: mpsc::Sender<Msg>,
    masker: Arc<Masker>,
    written: Arc<AtomicU64>,
    limit: u64,
}

impl LogWriter {
    /// Writes one line to a step's log (0 is the job's own log), after masking.
    pub async fn line(&self, step: i32, line: &str) {
        self.lines(step, std::slice::from_ref(&line.to_string())).await;
    }

    pub async fn lines(&self, step: i32, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        let mut out = Vec::with_capacity(lines.len());
        for l in lines {
            let masked = self.masker.mask(l);
            let n = self.written.fetch_add(masked.len() as u64 + 1, Ordering::Relaxed);
            if n >= self.limit {
                if n < self.limit + masked.len() as u64 + 1 + 64 {
                    out.push(format!(
                        "##[warning]The log limit of {} MB was reached. The rest of this job's log is not kept.",
                        self.limit / (1024 * 1024)
                    ));
                }
                continue;
            }
            out.push(masked);
        }
        if !out.is_empty() {
            let _ = self.tx.send(Msg::Lines { step, lines: out }).await;
        }
    }

    /// Waits until everything written so far has been sent (or given up on).
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    pub fn masker(&self) -> &Arc<Masker> {
        &self.masker
    }

    pub fn bytes_written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }
}

/// What the shipper reports when it ends.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ShipStats {
    pub batches: u64,
    pub lines_lost: u64,
}

pub fn spawn(client: Arc<Client>, job_id: i64, masker: Arc<Masker>, max_bytes: u64) -> (LogWriter, tokio::task::JoinHandle<ShipStats>) {
    let (tx, rx) = mpsc::channel(256);
    let writer = LogWriter { tx, masker, written: Arc::new(AtomicU64::new(0)), limit: max_bytes };
    let handle = tokio::spawn(ship(client, job_id, rx));
    (writer, handle)
}

#[allow(unused_assignments)]
async fn ship(client: Arc<Client>, job_id: i64, mut rx: mpsc::Receiver<Msg>) -> ShipStats {
    let mut stats = ShipStats::default();
    let mut seq: u64 = 0;
    let mut pending: Vec<String> = Vec::new();
    let mut pending_bytes = 0usize;
    let mut step = 0i32;
    let mut tick = tokio::time::interval(BATCH_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    macro_rules! flush {
        () => {
            if !pending.is_empty() {
                let batch = LogBatch { step, seq, lines: std::mem::take(&mut pending) };
                pending_bytes = 0;
                match client.send_logs(job_id, &batch).await {
                    Ok(ack) => {
                        stats.batches += 1;
                        seq = ack.next_seq.max(seq + 1);
                    }
                    Err(e) => {
                        tracing::warn!(job_id, "could not send {} log lines: {e}", batch.lines.len());
                        stats.lines_lost += batch.lines.len() as u64;
                        // Move on: the next batch must not collide with the one we gave up on.
                        seq += 1;
                    }
                }
            }
        };
    }

    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Msg::Lines { step: s, lines }) => {
                    if s != step {
                        flush!();
                        step = s;
                    }
                    for l in lines {
                        pending_bytes += l.len() + 1;
                        pending.push(l);
                    }
                    if pending_bytes >= BATCH_BYTES {
                        flush!();
                    }
                }
                Some(Msg::Flush(done)) => {
                    flush!();
                    let _ = done.send(());
                }
                None => {
                    flush!();
                    break;
                }
            },
            _ = tick.tick() => {
                flush!();
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_across_chunks() {
        let mut s = LineSplitter::default();
        assert_eq!(s.push(false, b"hel"), Vec::<String>::new());
        assert_eq!(s.push(false, b"lo\nwor"), vec!["hello"]);
        assert_eq!(s.push(false, b"ld\n\nlast"), vec!["world", ""]);
        assert_eq!(s.finish(), vec!["last"]);
        assert!(s.finish().is_empty());
    }

    #[test]
    fn crlf_and_carriage_return_redraws() {
        let mut s = LineSplitter::default();
        assert_eq!(s.push(false, b"a\r\nb\n"), vec!["a", "b"]);
        assert_eq!(s.push(false, b"10%\r50%\r100%\ndone\n"), vec!["100%", "done"]);
    }

    #[test]
    fn the_two_streams_do_not_mix() {
        let mut s = LineSplitter::default();
        assert!(s.push(false, b"out-").is_empty());
        assert_eq!(s.push(true, b"err\n"), vec!["err"]);
        assert_eq!(s.push(false, b"half\n"), vec!["out-half"]);
    }

    #[test]
    fn multibyte_characters_survive_a_split_chunk() {
        let bytes = "caf\u{e9} \u{1f980}\n".as_bytes();
        let mut s = LineSplitter::default();
        let (a, b) = bytes.split_at(5); // inside the crab
        assert!(s.push(false, a).is_empty());
        assert_eq!(s.push(false, b), vec!["caf\u{e9} \u{1f980}"]);
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        let mut s = LineSplitter::default();
        let l = s.push(false, b"bad \xff\xfe bytes\n");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains("bad"));
    }

    #[test]
    fn endless_lines_are_cut() {
        let mut s = LineSplitter::default();
        let big = vec![b'x'; MAX_LINE * 2 + 10];
        let l = s.push(false, &big);
        assert_eq!(l.len(), 2);
        assert!(l.iter().all(|x| x.len() == MAX_LINE));
        assert_eq!(s.finish()[0].len(), 10);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn chunking_never_changes_the_lines(text in "[a-z \\n]{0,200}", cut in 1usize..20) {
                let mut whole = LineSplitter::default();
                let mut a = whole.push(false, text.as_bytes());
                a.extend(whole.finish());
                let mut parts = LineSplitter::default();
                let mut b = Vec::new();
                for c in text.as_bytes().chunks(cut) {
                    b.extend(parts.push(false, c));
                }
                b.extend(parts.finish());
                prop_assert_eq!(a, b);
            }
        }
    }
}
