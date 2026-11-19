//! Turns queued turns into stored facts: the service proposes candidates, and the device skips,
//! adds or replaces against what it already knows.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use loremaster_core::{
    Candidate, DeviceCredential, EpisodeRequest, EpisodeResponse, ExtractRequest, ExtractResponse,
    ReconcileRequest, ReconcileResponse, estimate_tokens,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::config::{self, ConfigError, Paths};
use crate::index::DUPLICATE_SIMILARITY;
use crate::recall::{Recall, RecallError, TurnFacts};
use crate::store::{DueEpisode, PendingTurn, StoreError, TurnStatus};
use crate::sync::Triggers;

/// Above the service's own limit on a model call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A candidate no more similar than this to every stored fact is new.
const NEW_FACT_SIMILARITY: f32 = 0.75;
const MAX_FACT_CHARS: usize = 2_000;
/// Failed tries of one turn before it is given up. Only failures that the turn can cause count.
const MAX_ATTEMPTS: i64 = 5;
/// Failed tries of one session's episode before it is given up.
const MAX_EPISODE_ATTEMPTS: i64 = 3;
/// The most turn text, in estimated tokens, that one episode is made from.
const MAX_EPISODE_TOKENS: usize = 100_000;
/// A turn starts only once the model has loaded, and a loaded model stays.
const MODEL_LOADED: &str = "the embedding model is loaded";

/// How long the pipeline thread waits before it tries again.
#[derive(Debug, Clone, Copy)]
pub struct Cadence {
    /// While there is no login, no service URL or no model, or after a refused login.
    pub not_ready: Duration,
    /// After the service could not answer, or failed on the turn.
    pub retry: Duration,
    /// After the daily budget of model tokens is used up.
    pub daily_cap: Duration,
    /// With no turn waiting. A captured turn wakes the thread sooner.
    pub idle: Duration,
    /// With no turn for this long, a session that did not report its end gets its episode.
    pub episode_idle: Duration,
}

impl Cadence {
    pub const DAEMON: Self = Self {
        not_ready: Duration::from_secs(5),
        retry: Duration::from_secs(30),
        daily_cap: Duration::from_secs(60 * 60),
        idle: Duration::from_secs(5 * 60),
        // Under an hour, so the episode reaches other devices within an hour of the last turn.
        episode_idle: Duration::from_secs(50 * 60),
    };

    fn after(&self, step: Step) -> Duration {
        match step {
            Step::Handled => Duration::ZERO,
            Step::Idle => self.idle,
            Step::Retry(Pause::NotReady | Pause::Login) => self.not_ready,
            Step::Retry(Pause::Unavailable) => self.retry,
            Step::Retry(Pause::DailyCap) => self.daily_cap,
        }
    }
}

/// What one call of `Pipeline::process_next` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// A turn was extracted or a session's episode made, or one was given up as never possible.
    Handled,
    /// No turn or episode is waiting.
    Idle,
    /// The turn stays queued until the pause ends.
    Retry(Pause),
}

/// Why the pipeline cannot go on now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// No login, no service URL, or no embedding model yet.
    NotReady,
    /// The service could not be reached or failed; try again soon.
    Unavailable,
    /// The user's daily budget of model tokens is used up.
    DailyCap,
    /// The service refused this device; wait for a new login.
    Login,
}

/// A failure on this device. The turn stays queued.
#[derive(Debug, Error)]
pub enum PipelineError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Recall(#[from] RecallError),
}

/// Why a service call gave no answer.
enum CallError {
    Unavailable,
    DailyCap,
    Unauthorized,
    /// The service failed on this turn; it counts toward giving the turn up.
    Failed(String),
    /// The service can never accept this turn.
    Refused(String),
}

enum TurnError {
    Call(CallError),
    Local(PipelineError),
}

impl<E: Into<PipelineError>> From<E> for TurnError {
    fn from(error: E) -> Self {
        Self::Local(error.into())
    }
}

struct Target {
    url: String,
    credential: DeviceCredential,
}

/// Extracts queued turns for one device, oldest first.
pub struct Pipeline {
    recall: Arc<Recall>,
    paths: Paths,
    agent: ureq::Agent,
    /// A token the service refused. It is not sent again until login saves another.
    refused_token: Option<String>,
    episode_idle: Duration,
    stop: Arc<AtomicBool>,
}

impl Pipeline {
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
            episode_idle: Cadence::DAEMON.episode_idle,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Runs on its own thread until stopped: at start, on each wake, and on `cadence`. Asks
    /// `sync` to push after each turn it handles.
    pub fn spawn(mut self, cadence: Cadence, sync: Triggers) -> PipelineHandle {
        self.episode_idle = cadence.episode_idle;
        let (sender, messages) = mpsc::channel();
        let stop = self.stop.clone();
        let thread = thread::spawn(move || self.run(cadence, &messages, &sync));
        PipelineHandle {
            wake: Wake(sender),
            stop,
            thread,
        }
    }

    fn run(mut self, cadence: Cadence, messages: &Receiver<Message>, sync: &Triggers) {
        let mut reported = None;
        while !self.stopping() {
            let (wait, wake_ends_wait) = match self.process_next() {
                Ok(step) => {
                    reported = None;
                    if step == Step::Handled {
                        sync.push();
                    }
                    // A new turn cannot make the service answer sooner.
                    let paused = matches!(step, Step::Retry(Pause::Unavailable | Pause::DailyCap));
                    let wait = match step {
                        Step::Idle => self.idle_wait(cadence.idle),
                        _ => cadence.after(step),
                    };
                    (wait, !paused)
                }
                Err(error) => {
                    let message = error.to_string();
                    if reported.as_ref() != Some(&message) {
                        tracing::error!(error = %message, "cannot extract turns");
                        reported = Some(message);
                    }
                    (cadence.retry, true)
                }
            };
            let until = Instant::now() + wait;
            loop {
                match messages.recv_timeout(until.saturating_duration_since(Instant::now())) {
                    Ok(Message::Wake) if !wake_ends_wait => {}
                    Ok(Message::Wake) | Err(RecvTimeoutError::Timeout) => break,
                    Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }

    /// Up to `idle`, and no later than the next session's episode is due.
    fn idle_wait(&self, idle: Duration) -> Duration {
        match self.recall.store().next_episode_at(self.idle_ms()) {
            Ok(Some(due_at)) => idle.min(Duration::from_millis(
                due_at.saturating_sub(crate::now_ms()).max(0) as u64,
            )),
            Ok(None) => idle,
            Err(error) => {
                tracing::error!(%error, "cannot read when an episode is due");
                idle
            }
        }
    }

    fn idle_ms(&self) -> i64 {
        self.episode_idle.as_millis() as i64
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Extracts the oldest pending turn, or else makes the episode of a session that has ended,
    /// if the device is ready to.
    pub fn process_next(&mut self) -> Result<Step, PipelineError> {
        let Some(target) = self.target()? else {
            return Ok(Step::Retry(Pause::NotReady));
        };
        if self.refused_token.as_ref() == Some(&target.credential.token) {
            return Ok(Step::Retry(Pause::Login));
        }
        if !self.recall.can_embed() {
            return Ok(Step::Retry(Pause::NotReady));
        }
        let Some(turn) = self.recall.store().next_pending_turn()? else {
            return self.next_episode(&target);
        };
        match self.extract(&target, &turn) {
            Ok(()) => Ok(Step::Handled),
            Err(TurnError::Local(error)) => Err(error),
            Err(TurnError::Call(error)) => self.after_failure(&turn, error),
        }
    }

    /// Decides every candidate of the turn, then writes them all at once, so a call that fails
    /// part way writes nothing and the retry starts from the same state.
    fn extract(&mut self, target: &Target, turn: &PendingTurn) -> Result<(), TurnError> {
        let reply: ExtractResponse = self
            .call(
                target,
                "/v1/extract",
                &ExtractRequest {
                    turn: turn.turn.clone(),
                },
            )
            .map_err(TurnError::Call)?;
        let mut facts = TurnFacts::default();
        for candidate in reply.candidates {
            let text = candidate.text.trim();
            if !(1..=MAX_FACT_CHARS).contains(&text.chars().count()) {
                tracing::warn!(
                    chars = text.chars().count(),
                    "dropped a candidate of bad length"
                );
                continue;
            }
            let candidate = Candidate {
                text: text.to_owned(),
                ..candidate
            };
            self.resolve(target, turn, &candidate, &mut facts)?;
        }
        self.recall
            .finish_turn(turn.id, facts, &target.credential.device_id)?;
        Ok(())
    }

    /// Skips, adds or replaces one candidate.
    fn resolve(
        &mut self,
        target: &Target,
        turn: &PendingTurn,
        candidate: &Candidate,
        facts: &mut TurnFacts,
    ) -> Result<(), TurnError> {
        let found = self
            .recall
            .neighbours(&candidate.text, candidate.scope, &turn.project_id)?
            .expect(MODEL_LOADED);
        if facts.has_near_copy(&found.vector, candidate.scope) {
            return Ok(());
        }
        let top = found.nearest.first().map_or(f32::MIN, |(_, score)| *score);
        let supersedes = if top >= DUPLICATE_SIMILARITY {
            return Ok(());
        } else if top <= NEW_FACT_SIMILARITY {
            None
        } else {
            let request = ReconcileRequest {
                candidate: candidate.clone(),
                neighbours: found.nearest.into_iter().map(|(n, _)| n).collect(),
            };
            match self
                .call(target, "/v1/reconcile", &request)
                .map_err(TurnError::Call)?
            {
                ReconcileResponse::Skip => return Ok(()),
                ReconcileResponse::Add => None,
                ReconcileResponse::Supersede { memory_id, text } => Some((memory_id, text)),
            }
        };
        let (supersedes, candidate, vector) = match supersedes {
            // The reply's text can combine both facts, so it needs its own vector.
            Some((memory_id, text)) => {
                let vector = self.recall.embed(&text)?.expect(MODEL_LOADED);
                let candidate = Candidate {
                    text,
                    ..candidate.clone()
                };
                (Some(memory_id), candidate, vector)
            }
            None => (None, candidate.clone(), found.vector),
        };
        self.recall.add_fact(
            facts,
            &candidate,
            &turn.project_id,
            vector,
            supersedes.as_deref(),
        )?;
        Ok(())
    }

    fn next_episode(&mut self, target: &Target) -> Result<Step, PipelineError> {
        let due = self
            .recall
            .store()
            .due_episode(crate::now_ms(), self.idle_ms())?;
        let Some(due) = due else {
            return Ok(Step::Idle);
        };
        match self.episode(target, &due) {
            Ok(()) => Ok(Step::Handled),
            Err(TurnError::Local(error)) => Err(error),
            Err(TurnError::Call(error)) => self.after_episode_failure(target, &due, error),
        }
    }

    /// Asks the service to sum up the session, then stores the episode, if there is one, and
    /// deletes the session's queued turns.
    fn episode(&mut self, target: &Target, due: &DueEpisode) -> Result<(), TurnError> {
        let request = EpisodeRequest {
            turns: episode_turns(&due.turns),
        };
        let reply: EpisodeResponse = self
            .call(target, "/v1/episode", &request)
            .map_err(TurnError::Call)?;
        let Some(text) = reply.episode else {
            return Ok(self.drop_episode(target, due)?);
        };
        let text = text.trim();
        if !(1..=MAX_FACT_CHARS).contains(&text.chars().count()) {
            return Err(TurnError::Call(CallError::Failed(format!(
                "episode of {} characters",
                text.chars().count()
            ))));
        }
        let vector = self.recall.embed(text)?.expect(MODEL_LOADED);
        self.recall.write_episode(
            text,
            vector,
            &due.project_id,
            &due.session_id,
            &due.turn_ids(),
            &target.credential.device_id,
        )?;
        Ok(())
    }

    /// Deletes the session's queued turns with no episode.
    fn drop_episode(&self, target: &Target, due: &DueEpisode) -> Result<(), PipelineError> {
        Ok(self.recall.store().finish_episode(
            &due.session_id,
            &due.turn_ids(),
            None,
            &target.credential.device_id,
            crate::now_ms(),
        )?)
    }

    /// Keeps the session's turns for a retry, or gives its episode up when they are the cause.
    fn after_episode_failure(
        &self,
        target: &Target,
        due: &DueEpisode,
        error: CallError,
    ) -> Result<Step, PipelineError> {
        let reason = match error {
            CallError::Unavailable => return Ok(Step::Retry(Pause::Unavailable)),
            CallError::DailyCap => return Ok(Step::Retry(Pause::DailyCap)),
            CallError::Unauthorized => return Ok(Step::Retry(Pause::Login)),
            CallError::Refused(reason) => reason,
            CallError::Failed(reason) => {
                let attempts = self.recall.store().count_episode_failure(&due.session_id)?;
                if attempts < MAX_EPISODE_ATTEMPTS {
                    tracing::warn!(session = %due.session_id, attempts, %reason, "episode failed");
                    return Ok(Step::Retry(Pause::Unavailable));
                }
                reason
            }
        };
        tracing::error!(session = %due.session_id, %reason, "giving up an episode");
        self.drop_episode(target, due)?;
        Ok(Step::Handled)
    }

    /// Keeps the turn for a retry, or gives it up when it is the cause.
    fn after_failure(&self, turn: &PendingTurn, error: CallError) -> Result<Step, PipelineError> {
        let pause = match error {
            CallError::Unavailable => Pause::Unavailable,
            CallError::DailyCap => Pause::DailyCap,
            CallError::Unauthorized => Pause::Login,
            CallError::Refused(reason) => {
                tracing::error!(turn = turn.id, %reason, "giving up a turn");
                self.give_up(turn)?;
                return Ok(Step::Handled);
            }
            CallError::Failed(reason) => {
                let attempts = self.recall.store().count_failed_attempt(turn.id)?;
                if attempts >= MAX_ATTEMPTS {
                    tracing::error!(turn = turn.id, attempts, %reason, "giving up a turn");
                    self.give_up(turn)?;
                    return Ok(Step::Handled);
                }
                tracing::warn!(turn = turn.id, attempts, %reason, "turn failed");
                Pause::Unavailable
            }
        };
        Ok(Step::Retry(pause))
    }

    fn give_up(&self, turn: &PendingTurn) -> Result<(), PipelineError> {
        Ok(self
            .recall
            .store()
            .set_turn_status(turn.id, TurnStatus::Failed)?)
    }

    fn target(&self) -> Result<Option<Target>, PipelineError> {
        let url = config::service_url(&self.paths.config_toml())?;
        let credential = config::load_credential(&self.paths.credentials());
        Ok(url
            .zip(credential)
            .map(|(url, credential)| Target { url, credential }))
    }

    fn call<T: DeserializeOwned>(
        &mut self,
        target: &Target,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, CallError> {
        // A stop waits for the call in progress, so none starts after it.
        if self.stopping() {
            return Err(CallError::Unavailable);
        }
        let body = serde_json::to_string(body).expect("request types always serialize");
        let reply = self
            .agent
            .post(format!("{}{path}", target.url))
            .header(
                "Authorization",
                format!("Bearer {}", target.credential.token),
            )
            .header("Content-Type", "application/json")
            .send(&body);
        let mut reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                tracing::warn!(%error, path, "service unreachable");
                return Err(CallError::Unavailable);
            }
        };
        let status = reply.status().as_u16();
        let Ok(body) = reply.body_mut().read_to_string() else {
            return Err(CallError::Unavailable);
        };
        match status {
            200 => serde_json::from_str(&body)
                .map_err(|e| CallError::Failed(format!("bad reply from {path}: {e}"))),
            400 | 413 => Err(CallError::Refused(format!(
                "{path} refused the turn ({status}): {body}"
            ))),
            401 => {
                self.refused_token = Some(target.credential.token.clone());
                Err(CallError::Unauthorized)
            }
            429 => Err(CallError::DailyCap),
            500 | 502 => Err(CallError::Failed(format!(
                "{path} failed ({status}): {body}"
            ))),
            _ => Err(CallError::Unavailable),
        }
    }
}

/// The turns an episode is made from: the first, which usually states the task, then as many of
/// the newest as fit, oldest first.
fn episode_turns(turns: &[(i64, String)]) -> Vec<String> {
    let ((_, first), rest) = turns
        .split_first()
        .expect("a session is due only with a queued turn");
    let mut used = estimate_tokens(first);
    let newest = rest
        .iter()
        .rev()
        .map(|(_, turn)| turn)
        .take_while(|turn| {
            used += estimate_tokens(turn);
            used <= MAX_EPISODE_TOKENS
        })
        .count();
    std::iter::once(first)
        .chain(rest[rest.len() - newest..].iter().map(|(_, turn)| turn))
        .cloned()
        .collect()
}

enum Message {
    Wake,
    Stop,
}

/// Wakes the pipeline thread, as when a turn was queued. Does not wait for it.
#[derive(Clone)]
pub struct Wake(Sender<Message>);

impl Wake {
    pub fn wake(&self) {
        // A failed send means the pipeline has stopped, and there is nothing left to ask.
        let _ = self.0.send(Message::Wake);
    }
}

/// The running pipeline thread.
pub struct PipelineHandle {
    wake: Wake,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl PipelineHandle {
    pub fn wake(&self) -> Wake {
        self.wake.clone()
    }

    /// Stops the thread after the service call it is making, and waits for it.
    pub fn stop(self) -> thread::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.wake.0.send(Message::Stop);
        self.thread.join()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_step_waits_for_its_own_delay() {
        let cadence = Cadence {
            not_ready: Duration::from_secs(1),
            retry: Duration::from_secs(2),
            daily_cap: Duration::from_secs(3),
            idle: Duration::from_secs(4),
            episode_idle: Duration::from_secs(5),
        };
        let cases = [
            (Step::Handled, 0),
            (Step::Idle, 4),
            (Step::Retry(Pause::NotReady), 1),
            (Step::Retry(Pause::Login), 1),
            (Step::Retry(Pause::Unavailable), 2),
            (Step::Retry(Pause::DailyCap), 3),
        ];
        for (step, seconds) in cases {
            assert_eq!(
                cadence.after(step),
                Duration::from_secs(seconds),
                "{step:?}"
            );
        }
    }
}
