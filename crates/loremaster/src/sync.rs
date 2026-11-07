//! Sync with the service: pushes local changes from the outbox and pulls what other devices wrote.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use loremaster_core::{DeviceCredential, PullResponse, PushRequest, Record};
use thiserror::Error;

use crate::config::{self, ConfigError, Paths};
use crate::recall::{Recall, RecallError};
use crate::store::StoreError;

/// The service takes at most this many records in one push.
const MAX_PUSH_RECORDS: usize = 99;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A 409 means another push for the user was in progress, which ends quickly.
const CONFLICT_TRIES: usize = 3;
const PULL_CONFLICT_DELAY: Duration = Duration::from_secs(1);

/// When the sync thread acts on its own.
#[derive(Debug, Clone, Copy)]
pub struct Cadence {
    /// How often to look for local changes written by another process.
    pub outbox_check: Duration,
    /// How often to push and pull with nothing else to start it.
    pub period: Duration,
    /// How often to try again while the service cannot be reached.
    pub retry: Duration,
}

impl Cadence {
    pub const DAEMON: Self = Self {
        outbox_check: Duration::from_secs(5),
        period: Duration::from_secs(5 * 60),
        retry: Duration::from_secs(30),
    };
}

/// Why a push or pull did not complete.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("sync is off: no api_url or no login")]
    NotConfigured,
    #[error("the service refused this device; run loremaster login")]
    Unauthorized,
    #[error("service unavailable: {0}")]
    Unavailable(String),
    #[error("the service was busy with another push; trying again later")]
    Conflict,
    #[error("the service refused the push: {0}")]
    Refused(String),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Recall(#[from] RecallError),
}

impl From<ureq::Error> for SyncError {
    fn from(error: ureq::Error) -> Self {
        Self::Unavailable(error.to_string())
    }
}

/// Where to sync and as which device. Read on each use, so a login takes effect at once.
struct Target {
    url: String,
    credential: DeviceCredential,
}

/// Pushes and pulls for one device.
pub struct SyncClient {
    recall: Arc<Recall>,
    paths: Paths,
    agent: ureq::Agent,
    /// A token the service refused. It is not sent again until login saves another.
    refused_token: Option<String>,
    stop: Arc<AtomicBool>,
}

impl SyncClient {
    pub fn new(recall: Arc<Recall>, paths: Paths) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            recall,
            paths,
            agent,
            refused_token: None,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Pushes the outbox until it is empty. Returns the number of memories sent.
    pub fn push_outbox(&mut self) -> Result<usize, SyncError> {
        let target = self.target()?;
        let mut sent = 0;
        while !self.stopping() {
            let batch = self.recall.store().outbox_batch(MAX_PUSH_RECORDS)?;
            if batch.is_empty() {
                break;
            }
            let records: Vec<Record> = batch
                .into_iter()
                .map(|memory| Record {
                    seq: None,
                    user_id: None,
                    evict_key: None,
                    ..memory.record
                })
                .collect();
            match self.push(&target, &records) {
                Ok(()) => {
                    let pushed: Vec<(&str, &str)> = records
                        .iter()
                        .map(|r| (r.memory_id.as_str(), r.hlc.as_str()))
                        .collect();
                    self.recall.store().remove_pushed(&pushed)?;
                    sent += records.len();
                }
                Err(SyncError::Refused(reason)) => {
                    // Sending it again cannot help, and it would hold up every change after it.
                    let Some(bad) = records
                        .iter()
                        .find(|r| reason.starts_with(&format!("record {}:", r.memory_id)))
                    else {
                        return Err(SyncError::Refused(reason));
                    };
                    tracing::error!(memory = %bad.memory_id, %reason, "service refused a memory; not pushing it again");
                    self.recall.store().remove_from_outbox(&bad.memory_id)?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(sent)
    }

    /// Pulls every page after the saved cursor. Returns the number of memories stored.
    pub fn pull(&mut self) -> Result<usize, SyncError> {
        let target = self.target()?;
        let mut stored = 0;
        loop {
            let cursor = self.recall.store().sync_cursor()?;
            let page = self.pull_page(&target, cursor)?;
            let more = page.more;
            stored += self
                .recall
                .apply_pulled(page, &target.credential.device_id)?;
            if !more || self.stopping() {
                return Ok(stored);
            }
        }
    }

    /// Runs on its own thread until stopped: push and pull at start, then on triggers and on
    /// `cadence`.
    pub fn spawn(self, cadence: Cadence) -> SyncHandle {
        let (sender, messages) = mpsc::channel();
        let stop = self.stop.clone();
        let thread = thread::spawn(move || self.run(cadence, messages));
        SyncHandle {
            triggers: Triggers(sender),
            stop,
            thread,
        }
    }

    fn run(mut self, cadence: Cadence, messages: Receiver<Message>) {
        let mut state = LoopState::default();
        self.attempt(true, true, cadence, &mut state);
        let mut next_check = Instant::now() + cadence.outbox_check;
        let mut next_period = Instant::now() + cadence.period;
        loop {
            let wake = state.retry_at.unwrap_or(next_check.min(next_period));
            match messages.recv_timeout(wake.saturating_duration_since(Instant::now())) {
                Ok(Message::Push) => self.attempt(true, false, cadence, &mut state),
                Ok(Message::Pull) => self.attempt(false, true, cadence, &mut state),
                Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    if let Some(retry_at) = state.retry_at {
                        if now >= retry_at {
                            self.attempt(true, true, cadence, &mut state);
                        }
                    } else if now >= next_period {
                        self.attempt(true, true, cadence, &mut state);
                        next_period = Instant::now() + cadence.period;
                    } else if now >= next_check && self.has_outbox() {
                        self.attempt(true, false, cadence, &mut state);
                    }
                    if now >= next_check {
                        next_check = Instant::now() + cadence.outbox_check;
                    }
                }
            }
        }
    }

    /// One push, one pull or both. Logs each new problem once, and enters or leaves retry mode.
    fn attempt(&mut self, push: bool, pull: bool, cadence: Cadence, state: &mut LoopState) {
        let mut result = Ok(());
        if push {
            result = self.push_outbox().map(drop);
        }
        if pull && result.is_ok() {
            result = self.pull().map(drop);
        }
        match result {
            Ok(()) => {
                if state.retry_at.take().is_some() {
                    tracing::info!("service reachable again");
                }
                state.reported = None;
            }
            Err(error) => {
                state.retry_at = matches!(error, SyncError::Unavailable(_))
                    .then(|| Instant::now() + cadence.retry);
                let message = error.to_string();
                if state.reported.as_deref() != Some(message.as_str()) {
                    match error {
                        SyncError::NotConfigured => tracing::info!("{message}"),
                        _ => tracing::warn!(error = %message, "sync failed"),
                    }
                    state.reported = Some(message);
                }
            }
        }
    }

    fn has_outbox(&self) -> bool {
        match self.recall.store().outbox_count() {
            Ok(count) => count > 0,
            Err(e) => {
                tracing::error!(error = %e, "cannot read the outbox");
                false
            }
        }
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn target(&self) -> Result<Target, SyncError> {
        let url = config::service_url(&self.paths.config_toml())?;
        let credential = config::load_credential(&self.paths.credentials());
        let (Some(url), Some(credential)) = (url, credential) else {
            return Err(SyncError::NotConfigured);
        };
        if self.refused_token.as_ref() == Some(&credential.token) {
            return Err(SyncError::Unauthorized);
        }
        Ok(Target { url, credential })
    }

    fn push(&mut self, target: &Target, records: &[Record]) -> Result<(), SyncError> {
        let body = serde_json::to_string(&PushRequest {
            records: records.to_vec(),
        })
        .expect("request types always serialize");
        let mut tries = 0;
        loop {
            tries += 1;
            let reply = self
                .agent
                .post(format!("{}/v1/sync/push", target.url))
                .header(
                    "Authorization",
                    format!("Bearer {}", target.credential.token),
                )
                .header("Content-Type", "application/json")
                .send(&body);
            match self.read(target, reply) {
                Err(SyncError::Conflict) if tries < CONFLICT_TRIES => {}
                result => return result.map(drop),
            }
        }
    }

    fn pull_page(&mut self, target: &Target, cursor: i64) -> Result<PullResponse, SyncError> {
        let mut tries = 0;
        loop {
            tries += 1;
            let reply = self
                .agent
                .get(format!("{}/v1/sync/pull?cursor={cursor}", target.url))
                .header(
                    "Authorization",
                    format!("Bearer {}", target.credential.token),
                )
                .call();
            match self.read(target, reply) {
                Ok(body) => {
                    return serde_json::from_str(&body)
                        .map_err(|e| SyncError::Unavailable(format!("bad pull reply: {e}")));
                }
                Err(SyncError::Conflict) if tries < CONFLICT_TRIES => {
                    thread::sleep(PULL_CONFLICT_DELAY);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The body of a 200, or the error for any other reply.
    fn read(
        &mut self,
        target: &Target,
        reply: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<String, SyncError> {
        let mut reply = reply?;
        let status = reply.status().as_u16();
        let body = reply.body_mut().read_to_string()?;
        match status {
            200 => Ok(body),
            400 => Err(SyncError::Refused(
                serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["error"].as_str().map(str::to_owned))
                    .unwrap_or(body),
            )),
            401 => {
                self.refused_token = Some(target.credential.token.clone());
                Err(SyncError::Unauthorized)
            }
            409 => Err(SyncError::Conflict),
            _ => Err(SyncError::Unavailable(format!("status {status}"))),
        }
    }
}

enum Message {
    Push,
    Pull,
    Stop,
}

#[derive(Default)]
struct LoopState {
    /// Set while the service cannot be reached: when to try again.
    retry_at: Option<Instant>,
    /// The last problem logged, so a repeated one is not logged again.
    reported: Option<String>,
}

/// Asks the sync thread to act now. Does not wait for it.
#[derive(Clone)]
pub struct Triggers(Sender<Message>);

impl Triggers {
    pub fn push(&self) {
        // A failed send means sync has stopped, and there is nothing left to ask.
        let _ = self.0.send(Message::Push);
    }

    pub fn pull(&self) {
        let _ = self.0.send(Message::Pull);
    }
}

/// The running sync thread.
pub struct SyncHandle {
    triggers: Triggers,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl SyncHandle {
    pub fn triggers(&self) -> Triggers {
        self.triggers.clone()
    }

    /// Stops the thread after the request it is making, and waits for it.
    pub fn stop(self) -> thread::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.triggers.0.send(Message::Stop);
        self.thread.join()
    }
}
