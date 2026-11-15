//! The long-running daemon: answers hooks over a Unix socket and processes captured turns.

use std::fs::{self, File, TryLockError};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use loremaster_agents::{AdapterError, CoreEvent, LastTurn, adapter_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};

use crate::config::{Paths, create_private_dir};
use crate::embed::{self, EmbedError, Embedder};
use crate::project::ProjectIds;
use crate::recall::{Recall, RecallError, Recalled};
use crate::redact::Redactor;
use crate::spool::{self, SpoolEntry};
use crate::store::{QueuedTurn, Store, StoreError};
use crate::sync::{Cadence, SyncClient, Triggers};
use crate::{Verb, now_ms};

/// Why the daemon could not start. Shown to whoever started it.
#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("loremasterd already running")]
    AlreadyRunning,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Recall(#[from] RecallError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// How long a connected client may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause after a failed accept, such as running out of file descriptors, so it cannot spin.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
/// A transcript untouched for this long belongs to a session that is no longer mid-turn.
const TRANSCRIPT_IDLE: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct Request {
    verb: Verb,
    agent: String,
    event: Value,
}

#[derive(Default, Serialize)]
struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_context: Option<String>,
}

/// A turn-end event waiting to be processed.
struct CaptureJob {
    agent: String,
    verb: Verb,
    session_id: String,
    cwd: PathBuf,
    transcript_path: Option<PathBuf>,
    last_message: Option<String>,
    last_turn: LastTurn,
}

/// What a request produced: the reply, and what to record once the reply is delivered.
#[derive(Default)]
struct Answer {
    context: Option<String>,
    injected: Option<(String, Vec<String>)>,
}

impl Answer {
    fn delivering(session_id: String, recalled: Option<Recalled>) -> Self {
        recalled.map_or_else(Self::default, |Recalled { text, memory_ids }| Self {
            context: Some(text),
            injected: Some((session_id, memory_ids)),
        })
    }
}

#[derive(Debug, Error)]
enum CaptureError {
    #[error("unknown agent {0}")]
    UnknownAgent(String),
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Turns agent transcripts into redacted rows in the queue. Runs on one thread, so transcript
/// offsets are never read and advanced concurrently.
struct Capturer {
    recall: Arc<Recall>,
    projects: Arc<ProjectIds>,
    redactor: Redactor,
}

impl CaptureError {
    /// True when trying again later cannot help.
    fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::UnknownAgent(_)
                | Self::Adapter(AdapterError::TranscriptNotFound(_) | AdapterError::EmptyTurn)
        )
    }
}

impl Capturer {
    fn capture(&mut self, job: &CaptureJob) -> Result<(), CaptureError> {
        let adapter =
            adapter_for(&job.agent).ok_or_else(|| CaptureError::UnknownAgent(job.agent.clone()))?;
        let path_text = job.transcript_path.as_deref().map(|p| p.to_string_lossy());
        let known = self.recall.store().get_session(&job.session_id)?;
        let (offset, stored_path, project_id) = match known {
            Some(session) => (
                session.transcript_offset,
                session.transcript_path,
                session.project_id,
            ),
            None => {
                let project_id = self.projects.get(&job.cwd);
                self.recall.store().upsert_session(
                    &job.session_id,
                    &job.agent,
                    &project_id,
                    path_text.as_deref(),
                )?;
                (0, None, project_id)
            }
        };
        let path = job
            .transcript_path
            .clone()
            .or_else(|| stored_path.map(PathBuf::from));
        let (turns, end) = adapter.read_captured(
            path.as_deref(),
            offset,
            job.last_message.as_deref(),
            job.last_turn,
        )?;
        let mut position = offset;
        for (mut turn, next_offset) in turns {
            turn.map_text(|text| self.redactor.redact(text));
            let prepared = turn.prepare();
            self.recall.store().commit_turn(QueuedTurn {
                session_id: &job.session_id,
                agent: &job.agent,
                project_id: &project_id,
                turn: &prepared,
                next_offset,
                now_ms: now_ms(),
            })?;
            position = next_offset;
        }
        if end != position {
            self.recall.store().set_offset(&job.session_id, end)?;
        }
        Ok(())
    }

    /// Handles one job, logging failures. Returns true when it should be tried again later.
    fn process(&mut self, job: &CaptureJob) -> bool {
        let mut retry = false;
        if let Err(e) = self.capture(job) {
            tracing::error!(session = %job.session_id, error = %e, "capture failed");
            retry = !e.is_permanent();
        }
        if job.verb == Verb::SessionEnd
            && let Err(e) = self.recall.store().end_session(&job.session_id)
        {
            tracing::error!(session = %job.session_id, error = %e, "cannot end session");
            retry = true;
        }
        retry
    }

    /// Picks up turns from sessions that were still open when the daemon last stopped. A daemon
    /// killed just after replying to a session's last event has nothing else to prompt it.
    fn catch_up_open_sessions(&mut self) -> Result<(), StoreError> {
        let sessions = self.recall.store().open_sessions()?;
        for session in sessions {
            let Some(path) = session.transcript_path.map(PathBuf::from) else {
                continue;
            };
            let Ok(modified) = fs::metadata(&path).and_then(|meta| meta.modified()) else {
                continue;
            };
            let idle = modified.elapsed().is_ok_and(|age| age >= TRANSCRIPT_IDLE);
            self.process(&CaptureJob {
                agent: session.agent,
                verb: Verb::Capture,
                session_id: session.session_id,
                cwd: PathBuf::new(),
                transcript_path: Some(path),
                last_message: None,
                last_turn: if idle {
                    LastTurn::Finished
                } else {
                    LastTurn::Running
                },
            });
        }
        Ok(())
    }

    /// Processes turns that ended while the daemon was down. Entries that hit a passing failure,
    /// like a full disk, stay in the spool for the next start.
    fn replay_spool(&mut self, path: &Path) -> io::Result<()> {
        let entries = spool::read(path)?;
        let kept: Vec<SpoolEntry> = entries
            .into_iter()
            .filter(|entry| {
                self.process(&CaptureJob {
                    agent: entry.agent.clone(),
                    verb: entry.verb,
                    session_id: entry.session_id.clone(),
                    cwd: entry.cwd.clone(),
                    transcript_path: entry.transcript_path.clone(),
                    last_message: None,
                    last_turn: if entry.verb == Verb::SessionEnd {
                        LastTurn::Finished
                    } else {
                        LastTurn::MaybeRunning
                    },
                })
            })
            .collect();
        spool::rewrite(path, &kept)
    }
}

impl CaptureJob {
    fn spool_entry(&self) -> SpoolEntry {
        SpoolEntry {
            agent: self.agent.clone(),
            verb: self.verb,
            session_id: self.session_id.clone(),
            transcript_path: self.transcript_path.clone(),
            cwd: self.cwd.clone(),
        }
    }
}

/// State shared by connection tasks.
struct Daemon {
    recall: Arc<Recall>,
    projects: Arc<ProjectIds>,
    /// `None` tells the worker to finish what is queued and stop.
    jobs: mpsc::Sender<Option<CaptureJob>>,
    sync: Triggers,
}

impl Daemon {
    /// Answers one request. Anything unusable gets the empty reply, and the agent proceeds.
    fn handle(&self, line: &str) -> Answer {
        self.answer(line).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "request not answered");
            Answer::default()
        })
    }

    fn answer(&self, line: &str) -> Result<Answer, String> {
        let request: Request = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let adapter = adapter_for(&request.agent)
            .ok_or_else(|| format!("unknown agent {}", request.agent))?;
        let event = adapter
            .parse_event(&request.event.to_string())
            .map_err(|e| e.to_string())?;
        match (request.verb, event) {
            (
                Verb::SessionStart,
                CoreEvent::SessionStart {
                    session_id,
                    cwd,
                    transcript_path,
                    source,
                },
            ) => {
                self.sync.pull();
                let project_id = self.projects.get(&cwd);
                let path = transcript_path.as_deref().map(|p| p.to_string_lossy());
                {
                    let store = self.recall.store();
                    store
                        .upsert_session(&session_id, &request.agent, &project_id, path.as_deref())
                        .map_err(|e| e.to_string())?;
                    // These events take earlier memories out of the agent's context.
                    if matches!(source.as_deref(), Some("compact" | "clear")) {
                        store
                            .clear_injected(&session_id)
                            .map_err(|e| e.to_string())?;
                    }
                }
                let profile = self
                    .recall
                    .profile(&project_id, now_ms())
                    .map_err(|e| e.to_string())?;
                Ok(Answer::delivering(session_id, profile))
            }
            (
                Verb::Recall,
                CoreEvent::Recall {
                    session_id,
                    cwd,
                    prompt,
                },
            ) => {
                let project_id = self.projects.get(&cwd);
                let recalled = self
                    .recall
                    .for_prompt(&session_id, &project_id, &prompt, now_ms())
                    .map_err(|e| e.to_string())?;
                Ok(Answer::delivering(session_id, recalled))
            }
            (
                Verb::Capture,
                CoreEvent::Capture {
                    session_id,
                    cwd,
                    transcript_path,
                    last_assistant_message,
                },
            ) => self.enqueue(CaptureJob {
                agent: request.agent,
                verb: Verb::Capture,
                session_id,
                cwd,
                transcript_path,
                last_message: last_assistant_message,
                last_turn: LastTurn::MaybeRunning,
            }),
            (
                Verb::SessionEnd,
                CoreEvent::SessionEnd {
                    session_id,
                    cwd,
                    transcript_path,
                },
            ) => self.enqueue(CaptureJob {
                agent: request.agent,
                verb: Verb::SessionEnd,
                session_id,
                cwd,
                transcript_path,
                last_message: None,
                last_turn: LastTurn::Finished,
            }),
            (verb, _) => Err(format!("{verb:?} does not match its event")),
        }
    }

    fn enqueue(&self, job: CaptureJob) -> Result<Answer, String> {
        self.jobs.send(Some(job)).map_err(|e| e.to_string())?;
        Ok(Answer::default())
    }
}

async fn serve_connection(daemon: Arc<Daemon>, stream: UnixStream) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    tokio::time::timeout(REQUEST_TIMEOUT, BufReader::new(reader).read_line(&mut line)).await??;
    let handler = daemon.clone();
    let answer = tokio::task::spawn_blocking(move || handler.handle(&line))
        .await
        .unwrap_or_default();
    let response = Response {
        additional_context: answer.context,
    };
    let mut reply = serde_json::to_string(&response)?;
    reply.push('\n');
    writer.write_all(reply.as_bytes()).await?;
    // A hook past its deadline has gone, so the write above fails and these memories stay eligible.
    if let Some((session_id, memory_ids)) = answer.injected {
        tokio::task::spawn_blocking(move || daemon.recall.mark_injected(&session_id, &memory_ids))
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
    }
    Ok(())
}

/// Loads the model in the background, so hooks are served while it downloads or loads. Until it is
/// ready, prompts get no recall; profile and capture work.
fn load_model_in_background(recall: Arc<Recall>, models: PathBuf, load_gate: Arc<Mutex<()>>) {
    thread::spawn(move || {
        let dir = embed::model_dir(&models);
        let loaded = embed::ensure_model(&dir, &embed::base_url(), &embed::MODEL_FILES)
            .and_then(|()| {
                let _loading = load_gate.lock().unwrap_or_else(|e| e.into_inner());
                Embedder::load(&dir)
            })
            .map_err(|e: EmbedError| e.to_string())
            .and_then(|embedder| recall.set_embedder(embedder).map_err(|e| e.to_string()));
        match loaded {
            Ok(()) => tracing::info!("embedder ready"),
            Err(e) => tracing::error!(error = %e, "embedding model not available"),
        }
    });
}

fn init_logging(logs: &Path) -> io::Result<()> {
    fs::create_dir_all(logs)?;
    let file = File::options()
        .create(true)
        .append(true)
        .open(logs.join("loremasterd.log"))?;
    // A global subscriber can only be set once per process; that is not a failure to report.
    let _ = tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .try_init();
    Ok(())
}

/// Takes the lock that makes this the only daemon. The system releases it however the process
/// ends, so a killed daemon leaves nothing to clean up.
fn lock_daemon(path: &Path) -> Result<File, DaemonError> {
    let lock = File::options().create(true).append(true).open(path)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(DaemonError::AlreadyRunning),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Binds the socket, replacing one left by a daemon that was killed.
fn bind_socket(sock: &Path) -> Result<UnixListener, DaemonError> {
    if let Err(e) = fs::remove_file(sock)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(e.into());
    }
    Ok(UnixListener::bind(sock)?)
}

/// Runs the daemon until it receives SIGTERM or SIGINT.
pub fn run(paths: &Paths) -> Result<(), DaemonError> {
    create_private_dir(paths.home())?;
    fs::set_permissions(paths.home(), fs::Permissions::from_mode(0o700))?;
    init_logging(&paths.logs())?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_async(paths))
}

async fn run_async(paths: &Paths) -> Result<(), DaemonError> {
    let _lock = lock_daemon(&paths.home().join("daemon.lock"))?;
    let listener = bind_socket(&paths.sock())?;
    let recall = Arc::new(Recall::new(Store::open(&paths.store_db())?)?);
    let projects = Arc::new(ProjectIds::default());
    let mut capturer = Capturer {
        recall: recall.clone(),
        projects: projects.clone(),
        redactor: Redactor::new(paths.config_toml()),
    };
    // Neither failure is worth losing recall over: what they skipped stays in the spool or behind
    // the transcript offset, for the next start or the next event.
    if let Err(e) = capturer.replay_spool(&paths.spool()) {
        tracing::error!(error = %e, "cannot replay spool");
    }

    if let Err(e) = capturer.catch_up_open_sessions() {
        tracing::error!(error = %e, "cannot catch up open sessions");
    }

    let spool_path = paths.spool();
    let (jobs, queue) = mpsc::channel::<Option<CaptureJob>>();
    let worker = thread::spawn(move || {
        // Compile the secret rules now, so the first captured turn does not wait for them.
        capturer.redactor.redact("");
        while let Ok(Some(job)) = queue.recv() {
            // The hook has already gone, so the spool is the only way to retry this later.
            if capturer.process(&job)
                && let Err(e) = spool::append(&spool_path, &job.spool_entry())
            {
                tracing::error!(session = %job.session_id, error = %e, "cannot keep job for retry");
            }
        }
    });
    let load_gate = Arc::new(Mutex::new(()));
    load_model_in_background(recall.clone(), paths.models(), load_gate.clone());
    let sync = SyncClient::new(recall.clone(), paths.clone()).spawn(Cadence::DAEMON);
    let daemon = Arc::new(Daemon {
        recall,
        projects,
        jobs,
        sync: sync.triggers(),
    });

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tracing::info!(sock = %paths.sock().display(), "listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let daemon = daemon.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(daemon, stream).await {
                                tracing::debug!(error = %e, "connection ended early");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "cannot accept connection");
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    }
                }
            }
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }
    tracing::info!("stopping");
    // Exiting while ONNX Runtime is loading a model crashes the process. Wait out a load in progress
    // and never release the gate, so none starts. A download in progress is simply abandoned.
    std::mem::forget(load_gate.lock().unwrap_or_else(|e| e.into_inner()));
    // Finish queued captures first, so nothing below can skip them. A failed send means the worker
    // has already stopped.
    let _ = daemon.jobs.send(None);
    worker
        .join()
        .map_err(|_| io::Error::other("capture worker panicked"))?;
    sync.stop()
        .map_err(|_| io::Error::other("sync thread panicked"))?;
    match fs::remove_file(paths.sock()) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}
