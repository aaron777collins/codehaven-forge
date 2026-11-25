//! State that routes and workers must share but `AppState` has no slot for: the live log hub,
//! the wake-up for waiting runners, the master key and the clock.
//!
//! `api_routes()` and `runner_routes()` take no arguments, so handlers find this by the
//! instance's data directory, which is the same for everything in one process and different
//! for every test.

use crate::clock::{Clock, SystemClock};
use crate::source::WorkflowSource;
use ch_core::Config;
use ch_core::{Error, Result};
use ch_db::sealing::MasterKey;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};
use tokio::sync::{Notify, broadcast};

/// What a live log viewer receives.
#[derive(Debug, Clone)]
pub enum LogMsg {
    /// New lines for a step. `end` is the byte offset in the log file after them, which is also
    /// the SSE event id, so a viewer that reconnects can resume from where it stopped.
    Lines { step: i32, lines: Vec<String>, end: u64 },
    /// The job finished. The stream ends.
    Done,
}

#[derive(Default)]
pub struct Hub {
    logs: Mutex<HashMap<i64, broadcast::Sender<LogMsg>>>,
    /// Signalled whenever a job becomes queued, so long-polling runners wake at once.
    pub queue: Notify,
}

impl Hub {
    pub fn subscribe(&self, job: i64) -> broadcast::Receiver<LogMsg> {
        let mut g = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(job).or_insert_with(|| broadcast::channel(1024).0).subscribe()
    }

    pub fn publish(&self, job: i64, msg: LogMsg) {
        let mut g = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        let done = matches!(msg, LogMsg::Done);
        if let Some(tx) = g.get(&job) {
            // No receivers is fine: nobody is watching.
            let _ = tx.send(msg);
        }
        if done {
            g.remove(&job);
        }
    }

    /// Forgets a job's channel when nobody but the caller is listening. A viewer of a job that
    /// already finished would otherwise leave an entry behind that nothing ever removes.
    pub fn release_if_idle(&self, job: i64) {
        let mut g = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        if g.get(&job).is_some_and(|tx| tx.receiver_count() <= 1) {
            g.remove(&job);
        }
    }

    pub fn wake_runners(&self) {
        self.queue.notify_waiters();
    }

    /// Jobs somebody is watching right now.
    pub fn watched(&self) -> usize {
        self.logs.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Failed registration attempts per client address in a sliding window, so nobody can guess at
/// registration tokens or fill the audit log with failures. The tokens are 40 random characters,
/// so this is about the log and the noise, not about guessing.
#[derive(Default)]
pub struct RegisterThrottle {
    window: Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>,
}

impl RegisterThrottle {
    pub const MAX_FAILURES: u32 = 10;
    pub const WINDOW: std::time::Duration = std::time::Duration::from_secs(10 * 60);

    /// Seconds to wait, when this address has used up its failures.
    pub fn blocked(&self, ip: std::net::IpAddr) -> Option<u64> {
        let g = self.window.lock().unwrap_or_else(|e| e.into_inner());
        let (n, since) = g.get(&ip)?;
        let age = since.elapsed();
        (*n >= Self::MAX_FAILURES && age < Self::WINDOW).then(|| (Self::WINDOW - age).as_secs().max(1))
    }

    pub fn failed(&self, ip: std::net::IpAddr) {
        let mut g = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() > 10_000 {
            g.retain(|_, (_, t)| t.elapsed() < Self::WINDOW);
            if g.len() > 10_000 {
                g.clear();
            }
        }
        let e = g.entry(ip).or_insert((0, std::time::Instant::now()));
        if e.1.elapsed() >= Self::WINDOW {
            *e = (0, std::time::Instant::now());
        }
        e.0 += 1;
    }
}

pub struct Shared {
    pub hub: Hub,
    pub register_throttle: RegisterThrottle,
    key: OnceLock<MasterKey>,
    clock: RwLock<Arc<dyn Clock>>,
    source: RwLock<Option<Arc<dyn WorkflowSource>>>,
    /// Where GitHub actions are fetched from instead of github.com. Tests only.
    github: RwLock<Option<String>>,
    /// Signs the toolkit's archive links. Made at start: a link outlives no restart.
    toolkit_key: OnceLock<[u8; 32]>,
}

static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Arc<Shared>>>> = LazyLock::new(Default::default);

/// The shared state for this instance, created on first use.
pub fn shared_for(config: &Config) -> Arc<Shared> {
    let mut g = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    g.entry(config.storage.data_dir.clone())
        .or_insert_with(|| {
            Arc::new(Shared {
                hub: Hub::default(),
                register_throttle: RegisterThrottle::default(),
                key: OnceLock::new(),
                clock: RwLock::new(Arc::new(SystemClock)),
                source: RwLock::new(None),
                github: RwLock::new(None),
                toolkit_key: OnceLock::new(),
            })
        })
        .clone()
}

impl Shared {
    pub fn toolkit_key(&self) -> [u8; 32] {
        *self.toolkit_key.get_or_init(|| {
            let mut k = [0u8; 32];
            rand::Rng::fill(&mut rand::rng(), &mut k);
            k
        })
    }

    /// Tests: fetch "GitHub" actions from local repositories under this directory.
    pub fn set_github_base(&self, base: Option<String>) {
        *self.github.write().unwrap_or_else(|e| e.into_inner()) = base;
    }
    pub fn github_base(&self) -> Option<String> {
        self.github.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// The master key, read from `data/secrets/master.key` on first use.
    pub fn key(&self, config: &Config) -> Result<&MasterKey> {
        if let Some(k) = self.key.get() {
            return Ok(k);
        }
        let k = MasterKey::load_or_create(&config.master_key_path()).map_err(|e| Error::internal(anyhow::anyhow!("master key: {e}")))?;
        Ok(self.key.get_or_init(|| k))
    }

    pub fn clock(&self) -> Arc<dyn Clock> {
        self.clock.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The workflow source a test installed, if any. Production never installs one.
    pub fn source_override(&self) -> Option<Arc<dyn WorkflowSource>> {
        self.source.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// For tests: read workflow files from somewhere other than `GitService`.
    pub fn set_source(&self, s: Arc<dyn WorkflowSource>) {
        *self.source.write().unwrap_or_else(|e| e.into_inner()) = Some(s);
    }

    /// For tests: schedules read this clock instead of the system's.
    pub fn set_clock(&self, c: Arc<dyn Clock>) {
        *self.clock.write().unwrap_or_else(|e| e.into_inner()) = c;
    }
}
