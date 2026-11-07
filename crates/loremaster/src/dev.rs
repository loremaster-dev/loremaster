//! Development tools: seeding memories and measuring hook latency.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use clap::Subcommand;
use loremaster_core::{Hlc, Kind, Record, RecordType, Scope, new_ulid};
use serde::Deserialize;

use crate::config::{self, Paths, create_private_dir};
use crate::embed::{self, Embedder, MODEL_ID};
use crate::index::DIMENSIONS;
use crate::now_ms;
use crate::recall::query_text;
use crate::store::{Store, StoredMemory};

const MAX_TEXT_CHARS: usize = 2000;
const BENCH_MEMORIES: usize = 50_000;
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);
const FIXTURE_PROMPT: &str = "how do we deploy?";
const LONG_PROMPT_WORDS: usize = 800;

/// Development subcommands.
#[derive(Debug, Subcommand)]
pub enum DevCommand {
    /// Write memories from a JSONL file into the store
    Seed { file: PathBuf },
    /// Measure hook latency against 50k memories
    Bench {
        /// Hook runs per verb
        #[arg(long, default_value_t = 1000)]
        runs: usize,
    },
}

/// Runs a development subcommand.
pub fn run(command: DevCommand) -> ExitCode {
    let result = match command {
        DevCommand::Seed { file } => Paths::from_env()
            .map_err(|e| vec![e.to_string()])
            .and_then(|paths| seed(&paths, &file)),
        DevCommand::Bench { runs } => Paths::from_env()
            .map_err(|e| vec![e.to_string()])
            .and_then(|paths| bench(&paths, runs)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(errors) => {
            for error in errors {
                eprintln!("{error}");
            }
            ExitCode::FAILURE
        }
    }
}

#[derive(Deserialize)]
struct SeedLine {
    memory_id: Option<String>,
    #[serde(rename = "type")]
    record_type: Option<RecordType>,
    kind: Option<Kind>,
    scope: Option<Scope>,
    project_id: Option<String>,
    text: Option<String>,
    created_at: Option<i64>,
    hlc: Option<String>,
    superseded_by: Option<String>,
    #[serde(default)]
    deleted: bool,
}

fn line_error(line: &SeedLine) -> Option<String> {
    let needs = |present: bool, name: &str| (!present).then(|| format!("missing {name}"));
    let text_length = line.text.as_ref().map_or(0, |text| text.chars().count());
    if text_length > MAX_TEXT_CHARS {
        return Some("text over 2000 chars".to_owned());
    }
    if line.scope == Some(Scope::Project) && line.project_id.is_none() {
        return Some("project scope needs project_id".to_owned());
    }
    if line.deleted {
        return None;
    }
    needs(line.record_type.is_some(), "type")
        .or_else(|| needs(line.scope.is_some(), "scope"))
        .or_else(|| needs(line.text.is_some(), "text"))
        .or_else(|| {
            needs(
                line.record_type != Some(RecordType::Fact) || line.kind.is_some(),
                "kind",
            )
        })
}

fn parse_seed(text: &str) -> Result<Vec<SeedLine>, Vec<String>> {
    let mut lines = Vec::new();
    let mut errors = Vec::new();
    for (number, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let number = number + 1;
        match serde_json::from_str::<SeedLine>(line) {
            Err(_) => errors.push(format!("line {number}: invalid JSON")),
            Ok(seed) => match line_error(&seed) {
                Some(error) => errors.push(format!("line {number}: {error}")),
                None => lines.push(seed),
            },
        }
    }
    if errors.is_empty() {
        Ok(lines)
    } else {
        Err(errors)
    }
}

fn seed(paths: &Paths, file: &Path) -> Result<(), Vec<String>> {
    let text =
        fs::read_to_string(file).map_err(|_| vec![format!("cannot read {}", file.display())])?;
    let lines = parse_seed(&text)?;

    let unavailable = |_| vec!["embedding model not available".to_owned()];
    let dir = embed::model_dir(&paths.models());
    embed::ensure_model(&dir, &embed::base_url(), &embed::MODEL_FILES).map_err(unavailable)?;
    let embedder = Embedder::load(&dir).map_err(unavailable)?;

    // A logged-in device writes as itself and queues the memories for push. Before login there is
    // nothing to push them to.
    let device = config::load_credential(&paths.credentials()).map(|c| c.device_id);
    let mut clock = Hlc::new("local");
    let now = now_ms();
    let mut memories = Vec::with_capacity(lines.len());
    for line in lines {
        let vector = match &line.text {
            Some(text) => embedder
                .embed(&[text])
                .map_err(|e| vec![e.to_string()])?
                .pop(),
            None => None,
        };
        memories.push(StoredMemory {
            record: Record {
                user_id: None,
                memory_id: line.memory_id.unwrap_or_else(new_ulid),
                seq: None,
                hlc: match (&line.hlc, &device) {
                    (Some(hlc), _) => hlc.clone(),
                    (None, Some(_)) => String::new(),
                    (None, None) => clock.tick(now as u64),
                },
                writer: "local".to_owned(),
                record_type: line.record_type,
                kind: line.kind,
                scope: line.scope,
                project_id: line.project_id,
                text: line.text,
                created_at: line.created_at.or(Some(now)),
                superseded_by: line.superseded_by,
                deleted: line.deleted,
                evict_key: None,
            },
            vector,
            model_id: Some(MODEL_ID.to_owned()),
        });
    }
    let store = open_store(paths)?;
    match &device {
        Some(device_id) => store.write_local(&mut memories, device_id, now),
        None => store.put_memories(&memories),
    }
    .map_err(|e| vec![e.to_string()])?;
    println!("seeded {}", memories.len());
    Ok(())
}

fn open_store(paths: &Paths) -> Result<Store, Vec<String>> {
    create_private_dir(paths.home()).map_err(|e| vec![e.to_string()])?;
    Store::open(&paths.store_db()).map_err(|e| vec![e.to_string()])
}

/// Deterministic unit vector. Search cost does not depend on what vectors mean.
pub fn random_unit_vector(seed: u64) -> Vec<f32> {
    // xorshift64; `| 1` keeps the state non-zero.
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut vector: Vec<f32> = (0..DIMENSIONS)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state as f32 / u64::MAX as f32) - 0.5
        })
        .collect();
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    vector.iter_mut().for_each(|v| *v /= norm);
    vector
}

/// Text long enough to hit the limit on what recall embeds.
fn long_prompt() -> String {
    "please explain how the deploy pipeline works ".repeat(LONG_PROMPT_WORDS / 7)
}

/// Synthetic memories, the first ones exact matches for the measured prompts, so that recall is
/// measured finding something.
fn bench_memories(prompt_vectors: &[Vec<f32>]) -> Vec<StoredMemory> {
    let mut clock = Hlc::new("bench");
    let now = now_ms();
    (0..BENCH_MEMORIES)
        .map(|i| StoredMemory {
            record: Record {
                user_id: None,
                memory_id: new_ulid(),
                seq: None,
                hlc: clock.tick(now as u64),
                writer: "bench".to_owned(),
                record_type: Some(RecordType::Fact),
                kind: Some(Kind::Decision),
                scope: Some(Scope::User),
                project_id: None,
                text: Some(format!("Synthetic bench memory number {i}")),
                created_at: Some(now),
                superseded_by: None,
                deleted: false,
                evict_key: None,
            },
            vector: Some(
                prompt_vectors
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| random_unit_vector(i as u64)),
            ),
            model_id: Some(MODEL_ID.to_owned()),
        })
        .collect()
}

/// `(event, verb label, budget ms)`.
const BENCH_EVENTS: [(&str, &str, u64); 5] = [
    ("SessionStart", "session_start", 2000),
    ("UserPromptSubmit", "recall", 100),
    ("UserPromptSubmit", "recall_long", 100),
    ("Stop", "capture", 50),
    ("SessionEnd", "session_end", 1000),
];

macro_rules! fixture {
    ($file:literal) => {
        include_str!(concat!(
            "../../loremaster-agents/tests/fixtures/claude-code/",
            $file
        ))
    };
}

const FIXTURE_EVENTS: [&str; 5] = [
    fixture!("session_start.json"),
    fixture!("user_prompt_submit.json"),
    fixture!("user_prompt_submit.json"),
    fixture!("stop.json"),
    fixture!("session_end.json"),
];
const FIXTURE_TRANSCRIPT: &str = fixture!("transcript.jsonl");

/// Waits for the daemon to report its model loaded. On failure returns the last log lines.
fn wait_until_ready(daemon: &mut Child, log: &Path, timeout: Duration) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let text = fs::read_to_string(log).unwrap_or_default();
        if text.contains("embedder ready") {
            return Ok(());
        }
        let exited = matches!(daemon.try_wait(), Ok(Some(_)));
        if exited || started.elapsed() > timeout {
            let lines: Vec<&str> = text.lines().rev().take(20).collect();
            let tail: Vec<&str> = lines.into_iter().rev().collect();
            return Err(format!("daemon did not start:\n{}", tail.join("\n")));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_hook_once(
    exe: &Path,
    home: &Path,
    agent: &str,
    event: &str,
    input: &str,
) -> Result<(Duration, bool), String> {
    let started = Instant::now();
    let mut child = Command::new(exe)
        .args(["hook", agent, event])
        .env("LOREMASTER_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .map_or(Ok(()), |mut stdin| stdin.write_all(input.as_bytes()))
        .map_err(|e| e.to_string())?;
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok((started.elapsed(), !output.stdout.is_empty()))
}

/// Value at a percentile of sorted samples.
fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    sorted[(sorted.len() * percent / 100).min(sorted.len() - 1)]
}

/// One line of the report, and whether it met its budget.
fn report_row(
    verb: &str,
    agent: &str,
    samples: &mut [Duration],
    budget_ms: u64,
    failures: usize,
) -> (String, bool) {
    samples.sort_unstable();
    let (p50, p95) = (percentile(samples, 50), percentile(samples, 95));
    // A shared machine can stall any one run, so failures get the same 5% allowance as P95.
    let ok = p95.as_millis() <= u128::from(budget_ms) && failures * 20 <= samples.len();
    let row = format!(
        "{verb:<14} {agent:<12} {:<7} {:<7} {:<7} {}",
        format!("{}ms", p50.as_millis()),
        format!("{}ms", p95.as_millis()),
        format!("{budget_ms}ms"),
        if ok { "ok" } else { "FAIL" }
    );
    (row, ok)
}

fn stop_daemon(daemon: &mut Child) {
    // A clean stop, because a killed process leaves ONNX Runtime's temp files behind.
    let _ = Command::new("kill").arg(daemon.id().to_string()).status();
    let _ = daemon.wait();
}

fn bench(paths: &Paths, runs: usize) -> Result<(), Vec<String>> {
    let fail = |e: String| vec![e];
    let exe = std::env::current_exe().map_err(|e| fail(e.to_string()))?;
    let model_dir = embed::model_dir(&paths.models());
    let unavailable = |_| fail("embedding model not available".to_owned());
    embed::ensure_model(&model_dir, &embed::base_url(), &embed::MODEL_FILES)
        .map_err(unavailable)?;
    let long_prompt = long_prompt();
    let vectors = Embedder::load(&model_dir)
        .and_then(|embedder| embedder.embed(&[FIXTURE_PROMPT, query_text(&long_prompt)]))
        .map_err(unavailable)?;

    let temp = tempfile::tempdir().map_err(|e| fail(e.to_string()))?;
    let bench_paths = Paths::new(temp.path());
    symlink(paths.models(), bench_paths.models()).map_err(|e| fail(e.to_string()))?;
    copy_sync_files(paths, &bench_paths).map_err(|e| fail(e.to_string()))?;

    let started = Instant::now();
    open_store(&bench_paths)?
        .put_memories(&bench_memories(&vectors))
        .map_err(|e| fail(e.to_string()))?;
    println!(
        "seeded {BENCH_MEMORIES} memories in {:.1}s",
        started.elapsed().as_secs_f64()
    );

    let mut daemon = Command::new(&exe)
        .arg("daemon")
        .env("LOREMASTER_HOME", temp.path())
        .spawn()
        .map_err(|e| fail(e.to_string()))?;
    let log = bench_paths.logs().join("loremasterd.log");
    let outcome = wait_until_ready(&mut daemon, &log, DAEMON_START_TIMEOUT)
        .map_err(fail)
        .and_then(|()| measure(&exe, temp.path(), runs));
    stop_daemon(&mut daemon);
    outcome
}

/// Gives the bench home the service URL and login of the real one, so the bench runs with sync on.
/// Bench memories are not queued, so nothing of them is pushed.
fn copy_sync_files(from: &Paths, to: &Paths) -> std::io::Result<()> {
    for (source, target) in [
        (from.config_toml(), to.config_toml()),
        (from.credentials(), to.credentials()),
    ] {
        match fs::copy(&source, &target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            copied => {
                copied?;
            }
        }
    }
    if to.credentials().exists() {
        fs::set_permissions(to.credentials(), fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn with_session(
    input: &str,
    session_id: &str,
    prompt: Option<&str>,
) -> Result<String, Vec<String>> {
    let mut event: serde_json::Value =
        serde_json::from_str(input).map_err(|e| vec![e.to_string()])?;
    event["session_id"] = session_id.into();
    if let Some(prompt) = prompt {
        event["prompt"] = prompt.into();
    }
    Ok(event.to_string())
}

fn verdict(all_ok: bool) -> Result<(), Vec<String>> {
    all_ok
        .then_some(())
        .ok_or_else(|| vec!["a latency budget was exceeded".to_owned()])
}

fn measure(exe: &Path, home: &Path, runs: usize) -> Result<(), Vec<String>> {
    println!(
        "{:<14} {:<12} {:<7} {:<7} {:<7} result",
        "verb", "agent", "p50", "p95", "budget"
    );
    let mut all_ok = true;
    let agent = "claude-code";
    let dir = home.join(agent);
    fs::create_dir_all(&dir).map_err(|e| vec![e.to_string()])?;
    fs::write(dir.join("transcript.jsonl"), FIXTURE_TRANSCRIPT).map_err(|e| vec![e.to_string()])?;
    for (&(event, verb, budget), input) in BENCH_EVENTS.iter().zip(FIXTURE_EVENTS) {
        let input = input.replace("__FIXTURE_DIR__", &dir.display().to_string());
        let mut samples = Vec::with_capacity(runs);
        let mut failures = 0;
        for run in 0..runs {
            // A new session each time, or recall would have nothing left to inject.
            let session = format!("bench-{agent}-{verb}-{run}");
            let long = long_prompt();
            let run_input = match verb {
                "recall" => with_session(&input, &session, None)?,
                "recall_long" => with_session(&input, &session, Some(&long))?,
                _ => input.clone(),
            };
            let (elapsed, printed) =
                run_hook_once(exe, home, agent, event, &run_input).map_err(|e| vec![e])?;
            samples.push(elapsed);
            failures += usize::from(verb.starts_with("recall") && !printed);
        }
        // A capture past its deadline leaves a spool line behind.
        if verb == "capture" {
            let spool = fs::read_to_string(home.join("spool")).unwrap_or_default();
            failures += spool.lines().count();
        }
        let (row, ok) = report_row(verb, agent, &mut samples, budget, failures);
        println!("{row}");
        if failures > 0 {
            println!("{verb} {agent}: {failures} of {runs} runs did not complete their work");
        }
        all_ok &= ok;
    }
    verdict(all_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_seed_lines_parse() {
        let parsed = parse_seed(
            "{\"type\":\"fact\",\"kind\":\"preference\",\"scope\":\"user\",\"text\":\"t\"}\n\n\
             {\"type\":\"episode\",\"scope\":\"project\",\"project_id\":\"p\",\"text\":\"t\"}\n\
             {\"memory_id\":\"m\",\"deleted\":true}\n",
        )
        .unwrap();
        assert_eq!(parsed.len(), 3);
    }

    #[test]
    fn every_bad_line_is_reported_with_its_number() {
        let long = "x".repeat(2001);
        let text = format!(
            "{{\"type\":\"fact\",\"kind\":\"pitfall\",\"scope\":\"user\",\"text\":\"{long}\"}}\n\
             {{\"type\":\"fact\",\"kind\":\"pitfall\",\"scope\":\"project\",\"text\":\"t\"}}\n\
             not json\n\
             {{\"scope\":\"user\",\"text\":\"t\"}}\n\
             {{\"type\":\"fact\",\"kind\":\"pitfall\",\"text\":\"t\"}}\n\
             {{\"type\":\"fact\",\"kind\":\"pitfall\",\"scope\":\"user\"}}\n\
             {{\"type\":\"fact\",\"scope\":\"user\",\"text\":\"t\"}}\n"
        );
        let errors = parse_seed(&text).err().unwrap();
        assert_eq!(
            errors,
            [
                "line 1: text over 2000 chars",
                "line 2: project scope needs project_id",
                "line 3: invalid JSON",
                "line 4: missing type",
                "line 5: missing scope",
                "line 6: missing text",
                "line 7: missing kind",
            ]
        );
    }

    #[test]
    fn random_vectors_are_unit_length_and_differ() {
        let (a, b) = (random_unit_vector(1), random_unit_vector(2));
        assert_eq!(a.len(), DIMENSIONS);
        let norm: f32 = a.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert_ne!(a, b);
    }

    #[test]
    fn report_marks_rows_over_budget() {
        let mut samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let (row, ok) = report_row("recall", "claude-code", &mut samples, 100, 0);
        assert!(ok);
        assert_eq!(
            row,
            "recall         claude-code  51ms    96ms    100ms   ok"
        );
        let (row, ok) = report_row("capture", "claude-code", &mut samples, 50, 0);
        assert!(!ok);
        assert!(row.ends_with("FAIL"));
        let (_, ok) = report_row("recall", "claude-code", &mut samples, 100, 5);
        assert!(ok, "5% of runs may fail");
        let (row, ok) = report_row("recall", "claude-code", &mut samples, 100, 6);
        assert!(!ok, "more failures fail the row even within budget");
        assert!(row.ends_with("FAIL"));
    }

    #[test]
    fn bench_home_gets_the_service_url_and_a_private_login() {
        let (real, bench) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (from, to) = (Paths::new(real.path()), Paths::new(bench.path()));
        copy_sync_files(&from, &to).unwrap();
        assert!(!to.config_toml().exists() && !to.credentials().exists());

        fs::write(from.config_toml(), "api_url = \"https://api.example\"\n").unwrap();
        fs::write(from.credentials(), r#"{"device_id":"d","token":"t"}"#).unwrap();
        fs::set_permissions(from.credentials(), fs::Permissions::from_mode(0o644)).unwrap();
        copy_sync_files(&from, &to).unwrap();
        for file in [Paths::config_toml, Paths::credentials] {
            assert_eq!(fs::read(file(&to)).unwrap(), fs::read(file(&from)).unwrap());
        }
        let mode = fs::metadata(to.credentials()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        fs::remove_file(to.credentials()).unwrap();
        fs::create_dir(to.credentials()).unwrap();
        assert!(copy_sync_files(&from, &to).is_err());
    }

    #[test]
    fn bench_memories_are_not_queued_for_push() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("store.db")).unwrap();
        let vectors = vec![random_unit_vector(1), random_unit_vector(2)];
        store.put_memories(&bench_memories(&vectors)[..10]).unwrap();
        assert_eq!(store.outbox_count().unwrap(), 0);
    }

    #[test]
    fn with_session_replaces_the_session_only() {
        let output = with_session(r#"{"session_id":"old","cwd":"/x"}"#, "new", Some("p")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["session_id"], "new");
        assert_eq!(value["cwd"], "/x");
        assert_eq!(value["prompt"], "p");
        assert!(with_session("nope", "new", None).is_err());
    }

    #[test]
    fn verdict_fails_when_a_budget_was_exceeded() {
        assert!(verdict(true).is_ok());
        assert_eq!(
            verdict(false).unwrap_err(),
            ["a latency budget was exceeded"]
        );
    }

    #[test]
    fn bench_fixtures_line_up_with_their_events() {
        for ((event, _, _), input) in BENCH_EVENTS.iter().zip(FIXTURE_EVENTS) {
            assert!(input.contains(event), "{event}");
        }
        assert!(!FIXTURE_TRANSCRIPT.is_empty());
    }

    #[test]
    fn readiness_wait_reports_log_tail_when_the_daemon_exits() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        fs::write(&log, "first\nboom\n").unwrap();
        let mut child = Command::new("true").spawn().unwrap();
        let error = wait_until_ready(&mut child, &log, Duration::from_secs(5)).unwrap_err();
        assert_eq!(error, "daemon did not start:\nfirst\nboom");
    }

    #[test]
    fn readiness_wait_times_out_and_succeeds_on_the_ready_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        assert!(wait_until_ready(&mut child, &log, Duration::from_millis(100)).is_err());
        fs::write(&log, "x embedder ready\n").unwrap();
        assert!(wait_until_ready(&mut child, &log, Duration::from_secs(1)).is_ok());
        stop_daemon(&mut child);
    }
}
