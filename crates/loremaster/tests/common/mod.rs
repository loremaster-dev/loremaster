#![allow(dead_code)]

pub mod fake_service;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

use loremaster::config::Paths;
use loremaster::embed::{self, Embedder, MODEL_ID};
use loremaster::store::{Store, StoredMemory};
use loremaster_core::{Kind, Record, RecordType, Scope, new_ulid};

pub const SECRET: &str = "ghp_Z9xK2mQ7vL4tR8wN1pB6sD3fH5jG0cY2uE7a";
const WAIT: Duration = Duration::from_secs(30);

/// Models root shared by all tests, so the model is downloaded once.
pub fn shared_models() -> PathBuf {
    static DOWNLOAD: Once = Once::new();
    let models = Path::new(env!("CARGO_TARGET_TMPDIR")).join("shared-models");
    DOWNLOAD.call_once(|| {
        embed::ensure_model(
            &embed::model_dir(&models),
            embed::MODEL_URL,
            &embed::MODEL_FILES,
        )
        .expect("model download");
    });
    models
}

pub fn embedder() -> Embedder {
    Embedder::load(&embed::model_dir(&shared_models())).unwrap()
}

/// A temporary Loremaster home that uses the shared model.
pub struct Sandbox {
    pub dir: tempfile::TempDir,
    pub paths: Paths,
}

impl Sandbox {
    pub fn with_model() -> Self {
        let sandbox = Self::without_model();
        symlink(shared_models(), sandbox.paths.models()).unwrap();
        sandbox
    }

    pub fn without_model() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        Self { dir, paths }
    }

    pub fn store(&self) -> Store {
        Store::open(&self.paths.store_db()).unwrap()
    }

    pub fn log(&self) -> String {
        fs::read_to_string(self.paths.logs().join("loremasterd.log")).unwrap_or_default()
    }

    pub fn query_i64(&self, sql: &str) -> i64 {
        let conn = rusqlite::Connection::open(self.paths.store_db()).unwrap();
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    pub fn turns(&self) -> Vec<String> {
        let conn = rusqlite::Connection::open(self.paths.store_db()).unwrap();
        let mut query = conn
            .prepare("SELECT turn FROM turn_queue ORDER BY id")
            .unwrap();
        query
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    pub fn daemon_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_loremaster"));
        command
            .arg("daemon")
            .env("LOREMASTER_HOME", self.paths.home());
        command
    }

    pub fn start_daemon(&self) -> Daemon {
        let child = self.daemon_command().spawn().unwrap();
        wait_for(
            || UnixStream::connect(self.paths.sock()).is_ok(),
            "daemon socket",
        );
        // The socket is bound before the spool replay; a reply means the replay is done.
        self.ask("ping");
        Daemon { child }
    }

    pub fn wait_for_embedder(&self) {
        wait_for(|| self.log().contains("embedder ready"), "embedder ready");
    }

    pub fn hook(&self, agent: &str, event: &str, input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_loremaster"))
            .args(["hook", agent, event])
            .env("LOREMASTER_HOME", self.paths.home())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Sends one raw line to the daemon socket and returns the reply line.
    pub fn ask(&self, line: &str) -> String {
        let stream = UnixStream::connect(self.paths.sock()).unwrap();
        (&stream).write_all(format!("{line}\n").as_bytes()).unwrap();
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply).unwrap();
        reply.trim_end().to_owned()
    }
}

pub struct Daemon {
    pub child: Child,
}

impl Daemon {
    /// Stops it the way a service manager does.
    pub fn terminate(mut self) -> std::process::ExitStatus {
        let pid = self.child.id().to_string();
        Command::new("kill").arg(&pid).status().unwrap();
        let status = self.child.wait().unwrap();
        std::mem::forget(self);
        status
    }

    pub fn kill9(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        // A killed process cannot remove the temp files ONNX Runtime made for it.
        let marker = format!("-{}-", self.child.id());
        for entry in fs::read_dir(std::env::temp_dir()).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("onnxruntime-") && name.contains(&marker) {
                let _ = fs::remove_dir_all(entry.path());
                let _ = fs::remove_file(entry.path());
            }
        }
        std::mem::forget(self);
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg(self.child.id().to_string())
            .status();
        let _ = self.child.wait();
    }
}

pub fn wait_for(mut condition: impl FnMut() -> bool, what: &str) {
    let started = Instant::now();
    while !condition() {
        assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn memory(
    embedder: &Embedder,
    kind: Option<Kind>,
    project: Option<&str>,
    text: &str,
    created_at: i64,
) -> StoredMemory {
    StoredMemory {
        record: Record {
            user_id: None,
            memory_id: new_ulid(),
            seq: None,
            hlc: "1761750000123-00000-test".into(),
            writer: "test".into(),
            record_type: Some(if kind.is_some() {
                RecordType::Fact
            } else {
                RecordType::Episode
            }),
            kind,
            scope: Some(project.map_or(Scope::User, |_| Scope::Project)),
            project_id: project.map(str::to_owned),
            text: Some(text.to_owned()),
            created_at: Some(created_at),
            superseded_by: None,
            deleted: false,
            evict_key: None,
        },
        vector: embedder.embed(&[text]).unwrap().pop(),
        model_id: Some(MODEL_ID.to_owned()),
    }
}

pub fn fixture_dir(agent: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../loremaster-agents/tests/fixtures")
        .join(agent)
}

/// Hook input from a fixture, pointing at `dir` for its transcript and at `cwd` if given.
pub fn hook_input(agent: &str, file: &str, dir: &Path, cwd: Option<&Path>) -> String {
    let text = fs::read_to_string(fixture_dir(agent).join(file)).unwrap();
    let text = text.replace("__FIXTURE_DIR__", &dir.display().to_string());
    match cwd {
        Some(cwd) => text.replace("/tmp/fixture-project", &cwd.display().to_string()),
        None => text,
    }
}

/// Wraps hook input as a socket request.
pub fn request(verb: &str, agent: &str, event: &str) -> String {
    let event: serde_json::Value = serde_json::from_str(event).unwrap();
    serde_json::json!({"verb": verb, "agent": agent, "event": event}).to_string()
}

/// A git repo whose origin normalizes to `github.com/acme/web`.
pub fn acme_web_repo(root: &Path) -> PathBuf {
    let repo = root.join("web");
    fs::create_dir_all(&repo).unwrap();
    for args in [
        ["init", "-q", "."].as_slice(),
        ["remote", "add", "origin", "git@github.com:acme/web.git"].as_slice(),
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    }
    repo
}

/// Calls recall the way the daemon does when the reply reaches the agent: text, then marking.
pub trait DeliveredRecall {
    fn for_prompt_delivered(
        &self,
        session_id: &str,
        project_id: &str,
        prompt: &str,
        now_ms: i64,
    ) -> Result<Option<String>, loremaster::recall::RecallError>;
}

impl DeliveredRecall for loremaster::recall::Recall {
    fn for_prompt_delivered(
        &self,
        session_id: &str,
        project_id: &str,
        prompt: &str,
        now_ms: i64,
    ) -> Result<Option<String>, loremaster::recall::RecallError> {
        let Some(recalled) = self.for_prompt(session_id, project_id, prompt, now_ms)? else {
            return Ok(None);
        };
        self.mark_injected(session_id, &recalled.memory_ids)?;
        Ok(Some(recalled.text))
    }
}
