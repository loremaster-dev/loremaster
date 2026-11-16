//! The daemon as a real process, driven over its socket.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;

use common::{Sandbox, acme_web_repo, embedder, hook_input, memory, request, wait_for};
use loremaster::now_ms;
use loremaster_agents::{Adapter, ClaudeCodeAdapter};
use loremaster_core::Kind;

const DEPLOY: &str = "Deploys go through ArgoCD, not manual kubectl";
const WEB: &str = "github.com/acme/web";
const NO_MODEL_URL: &str = "http://127.0.0.1:1";

fn seed_deploy_fact(sandbox: &Sandbox) {
    sandbox
        .store()
        .put_memory(&memory(
            &embedder(),
            Some(Kind::Decision),
            Some(WEB),
            DEPLOY,
            now_ms(),
        ))
        .unwrap();
}

/// Copies an agent's fixture transcript into `dir` so that tests can point hooks at it.
fn copy_transcript(agent: &str, dir: &std::path::Path) -> u64 {
    fs::create_dir_all(dir).unwrap();
    fs::copy(
        common::fixture_dir(agent).join("transcript.jsonl"),
        dir.join("transcript.jsonl"),
    )
    .unwrap()
}

#[test]
fn unparseable_input_gets_the_empty_reply() {
    let sandbox = Sandbox::with_model();
    let _daemon = sandbox.start_daemon();
    assert_eq!(sandbox.ask("garbage"), "{}");
}

#[test]
fn stale_socket_is_replaced() {
    let sandbox = Sandbox::with_model();
    sandbox.start_daemon().kill9();
    assert!(sandbox.paths.sock().exists());
    let _daemon = sandbox.start_daemon();
    assert_eq!(sandbox.ask("garbage"), "{}");
}

#[test]
fn startup_logs_that_it_is_listening() {
    let sandbox = Sandbox::with_model();
    let _daemon = sandbox.start_daemon();
    wait_for(|| sandbox.log().contains("listening"), "listening line");
}

#[test]
fn clean_stop_exits_zero_and_removes_the_socket() {
    let sandbox = Sandbox::with_model();
    let status = sandbox.start_daemon().terminate();
    assert!(status.success());
    assert!(!sandbox.paths.sock().exists());
}

#[test]
fn second_daemon_refuses_to_start_and_the_first_keeps_running() {
    let sandbox = Sandbox::with_model();
    let _daemon = sandbox.start_daemon();
    let output = sandbox.daemon_command().output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "loremasterd already running"
    );
    assert_eq!(sandbox.ask("garbage"), "{}");
}

#[test]
fn newer_store_schema_refuses_to_start() {
    let sandbox = Sandbox::with_model();
    {
        let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
        conn.execute_batch("PRAGMA user_version = 3").unwrap();
    }
    let output = sandbox.daemon_command().output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "store.db is from a newer Loremaster (schema 3)"
    );
}

#[test]
fn spool_is_replayed_then_emptied_and_bad_lines_are_skipped() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    copy_transcript("claude-code", &dir);
    // A transcript holding only the first two turns.
    let path = dir.join("transcript.jsonl");
    let (_, after_first) = ClaudeCodeAdapter.read_turn(&path, 0).unwrap();
    let (_, after_second) = ClaudeCodeAdapter.read_turn(&path, after_first).unwrap();
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..after_second as usize]).unwrap();
    let line = |verb: &str| {
        serde_json::json!({
            "agent": "claude-code",
            "verb": verb,
            "session_id": "s1",
            "transcript_path": path,
            "cwd": "/tmp/fixture-project",
        })
        .to_string()
    };
    fs::write(
        sandbox.paths.spool(),
        format!("{}\nnot json\n{}\n", line("capture"), line("capture")),
    )
    .unwrap();
    let _daemon = sandbox.start_daemon();
    assert_eq!(sandbox.turns().len(), 2);
    assert!(!sandbox.paths.spool().exists());
}

#[test]
fn client_disconnecting_before_the_reply_does_not_stop_service() {
    let sandbox = Sandbox::with_model();
    let _daemon = sandbox.start_daemon();
    {
        let stream = UnixStream::connect(sandbox.paths.sock()).unwrap();
        (&stream).write_all(b"garbage\n").unwrap();
    }
    drop(UnixStream::connect(sandbox.paths.sock()).unwrap());
    assert_eq!(sandbox.ask("garbage"), "{}");
}

#[test]
fn recall_returns_the_relevant_memory_for_each_agent() {
    let sandbox = Sandbox::with_model();
    seed_deploy_fact(&sandbox);
    let repo = acme_web_repo(sandbox.dir.path());
    let _daemon = sandbox.start_daemon();
    sandbox.wait_for_embedder();
    let agent = "claude-code";
    let event = serde_json::json!({
        "session_id": format!("s-{agent}"),
        "cwd": repo,
        "prompt": "how do we deploy?",
        "hook_event_name": "UserPromptSubmit",
    })
    .to_string();
    assert_eq!(
        sandbox.ask(&request("recall", agent, &event)),
        format!(
            "{{\"additional_context\":\"Loremaster memories relevant to this prompt:\\n- [decision] {DEPLOY}\"}}"
        ),
        "{agent}"
    );
}

#[test]
fn session_start_with_no_memories_replies_empty_and_records_the_session() {
    let sandbox = Sandbox::with_model();
    let _daemon = sandbox.start_daemon();
    let agent = "claude-code";
    let dir = sandbox.dir.path().join(agent);
    let event = hook_input(agent, "session_start.json", &dir, None);
    assert_eq!(sandbox.ask(&request("session_start", agent, &event)), "{}");
    assert_eq!(sandbox.query_i64("SELECT count(*) FROM sessions"), 1);
}

#[test]
fn session_start_returns_the_profile_even_without_the_model() {
    let sandbox = Sandbox::without_model();
    let repo = acme_web_repo(sandbox.dir.path());
    seed_deploy_fact(&sandbox);
    let mut command = sandbox.daemon_command();
    command.env(loremaster::embed::MODEL_URL_ENV, NO_MODEL_URL);
    let child = command.spawn().unwrap();
    let _daemon = common::Daemon { child };
    wait_for(
        || UnixStream::connect(sandbox.paths.sock()).is_ok(),
        "socket",
    );
    wait_for(
        || sandbox.log().contains("embedding model not available"),
        "model failure",
    );

    let start = serde_json::json!({
        "session_id": "s1", "cwd": repo, "hook_event_name": "SessionStart",
    })
    .to_string();
    let reply = sandbox.ask(&request("session_start", "claude-code", &start));
    assert!(reply.contains(DEPLOY), "{reply}");

    let prompt = serde_json::json!({
        "session_id": "s1", "cwd": repo, "prompt": "how do we deploy?",
        "hook_event_name": "UserPromptSubmit",
    })
    .to_string();
    assert_eq!(
        sandbox.ask(&request("recall", "claude-code", &prompt)),
        "{}"
    );
}

#[test]
fn capture_replies_then_queues_redacted_turns_once() {
    let agent = "claude-code";
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    let end = copy_transcript(agent, &dir);
    let _daemon = sandbox.start_daemon();
    let stop = request(
        "capture",
        agent,
        &hook_input(agent, "stop.json", &dir, None),
    );

    assert_eq!(sandbox.ask(&stop), "{}");
    wait_for(|| sandbox.turns().len() == 3, "three queued turns");

    assert_eq!(sandbox.ask(&stop), "{}");

    let turns = sandbox.turns();
    assert!(turns[1].contains("[REDACTED:github-pat]"), "{}", turns[1]);
    assert!(!turns.iter().any(|turn| turn.contains("ghp_")));
    assert_eq!(
        sandbox.query_i64("SELECT count(*) FROM turn_queue WHERE status = 'pending'"),
        3
    );
    wait_for(
        || {
            sandbox.query_i64("SELECT coalesce(max(transcript_offset), 0) FROM sessions")
                == end as i64
        },
        "offset at the end",
    );

    let end_event = request(
        "session_end",
        agent,
        &hook_input(agent, "session_end.json", &dir, None),
    );
    assert_eq!(sandbox.ask(&end_event), "{}");
    wait_for(
        || sandbox.query_i64("SELECT coalesce(max(ended), 0) FROM sessions") == 1,
        "session ended",
    );
    assert_eq!(sandbox.turns().len(), 3, "{agent}");
}

#[test]
fn session_end_queues_what_is_left() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    copy_transcript("claude-code", &dir);
    let _daemon = sandbox.start_daemon();
    let end_event = request(
        "session_end",
        "claude-code",
        &hook_input("claude-code", "session_end.json", &dir, None),
    );
    assert_eq!(sandbox.ask(&end_event), "{}");
    wait_for(
        || sandbox.query_i64("SELECT coalesce(max(ended), 0) FROM sessions") == 1,
        "session ended",
    );
    assert_eq!(sandbox.turns().len(), 3);
}

#[test]
fn unknown_agent_and_unknown_verb_get_the_empty_reply() {
    let sandbox = Sandbox::with_model();
    let _daemon = sandbox.start_daemon();
    let event = hook_input(
        "claude-code",
        "user_prompt_submit.json",
        sandbox.dir.path(),
        None,
    );
    assert_eq!(sandbox.ask(&request("recall", "cursor", &event)), "{}");
    assert_eq!(sandbox.ask(&request("forget", "claude-code", &event)), "{}");
    assert_eq!(
        sandbox.ask(&request("capture", "claude-code", &event)),
        "{}"
    );
    assert_eq!(sandbox.ask(&request("recall", "claude-code", "{}")), "{}");
}

#[test]
fn capture_with_a_missing_transcript_queues_nothing_and_keeps_the_offset() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("nowhere");
    let _daemon = sandbox.start_daemon();
    let stop = request(
        "capture",
        "claude-code",
        &hook_input("claude-code", "stop.json", &dir, None),
    );
    assert_eq!(sandbox.ask(&stop), "{}");
    wait_for(
        || sandbox.log().contains("capture failed"),
        "capture failure logged",
    );
    assert_eq!(sandbox.turns().len(), 0);
    assert_eq!(
        sandbox.query_i64("SELECT coalesce(max(transcript_offset), 0) FROM sessions"),
        0
    );
}

#[test]
fn capture_with_no_transcript_keeps_the_final_message() {
    let sandbox = Sandbox::with_model();
    let _daemon = sandbox.start_daemon();
    let event = serde_json::json!({
        "session_id": "s1", "cwd": "/tmp/p", "hook_event_name": "Stop",
        "transcript_path": null, "last_assistant_message": "only the reply",
    })
    .to_string();
    assert_eq!(
        sandbox.ask(&request("capture", "claude-code", &event)),
        "{}"
    );
    wait_for(|| sandbox.turns().len() == 1, "queued turn");
    assert_eq!(
        sandbox.turns()[0],
        r#"{"prompt":"","tool_calls":[],"reply":"only the reply"}"#
    );
}

#[test]
fn failed_write_rolls_back_and_the_next_capture_queues_the_turn() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    copy_transcript("claude-code", &dir);
    let stop = request(
        "capture",
        "claude-code",
        &hook_input("claude-code", "stop.json", &dir, None),
    );
    let _daemon = sandbox.start_daemon();
    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_queue BEFORE INSERT ON turn_queue
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();

    assert_eq!(sandbox.ask(&stop), "{}");
    wait_for(
        || sandbox.log().contains("capture failed"),
        "capture failure logged",
    );
    assert_eq!(sandbox.turns().len(), 0);
    assert_eq!(
        sandbox.query_i64("SELECT coalesce(max(transcript_offset), 0) FROM sessions"),
        0
    );

    conn.execute_batch("DROP TRIGGER fail_queue").unwrap();
    assert_eq!(sandbox.ask(&stop), "{}");
    wait_for(|| sandbox.turns().len() == 3, "turns queued after recovery");
}

#[test]
fn custom_secret_pattern_applies_to_captured_turns() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    fs::create_dir_all(&dir).unwrap();
    let line = serde_json::json!({"type":"user","message":{"content":"ticket ACME-123456"}});
    fs::write(dir.join("transcript.jsonl"), format!("{line}\n")).unwrap();
    fs::write(
        sandbox.paths.config_toml(),
        "[redact]\npatterns = [\"ACME-[0-9]{6}\"]\n",
    )
    .unwrap();
    let _daemon = sandbox.start_daemon();
    let stop = request(
        "capture",
        "claude-code",
        &hook_input("claude-code", "stop.json", &dir, None),
    );
    assert_eq!(sandbox.ask(&stop), "{}");
    wait_for(|| sandbox.turns().len() == 1, "queued turn");
    assert!(sandbox.turns()[0].contains("ticket [REDACTED:custom-1]"));
}

#[test]
fn late_output_with_no_prompt_advances_the_offset_without_a_row() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    fs::create_dir_all(&dir).unwrap();
    let line = serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"late"}]}});
    let text = format!("{line}\n");
    fs::write(dir.join("transcript.jsonl"), &text).unwrap();
    let _daemon = sandbox.start_daemon();
    let stop = request(
        "capture",
        "claude-code",
        &hook_input("claude-code", "stop.json", &dir, None),
    );
    assert_eq!(sandbox.ask(&stop), "{}");
    wait_for(
        || {
            sandbox.query_i64("SELECT coalesce(max(transcript_offset), 0) FROM sessions")
                == text.len() as i64
        },
        "offset past the late output",
    );
    assert!(sandbox.turns().is_empty());
}

#[test]
fn session_end_failure_is_logged_and_service_continues() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("nowhere");
    let _daemon = sandbox.start_daemon();
    let start = hook_input("claude-code", "session_start.json", &dir, None);
    assert_eq!(
        sandbox.ask(&request("session_start", "claude-code", &start)),
        "{}"
    );
    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_end BEFORE UPDATE ON sessions
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    let end_event = hook_input("claude-code", "session_end.json", &dir, None);
    assert_eq!(
        sandbox.ask(&request("session_end", "claude-code", &end_event)),
        "{}"
    );
    wait_for(
        || sandbox.log().contains("cannot end session"),
        "failure logged",
    );
    assert_eq!(sandbox.ask("garbage"), "{}");
}

#[test]
fn socket_path_that_cannot_be_replaced_stops_startup() {
    let sandbox = Sandbox::with_model();
    fs::create_dir_all(sandbox.paths.sock()).unwrap();
    let output = sandbox.daemon_command().output().unwrap();
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn origin_url_credentials_stay_out_of_the_log() {
    let sandbox = Sandbox::with_model();
    let repo = sandbox.dir.path().join("private");
    fs::create_dir_all(&repo).unwrap();
    for args in [
        ["init", "-q", "."].as_slice(),
        [
            "remote",
            "add",
            "origin",
            "https://oauth2:glpat-abc123xyz@/group/app.git",
        ]
        .as_slice(),
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    }
    let _daemon = sandbox.start_daemon();
    let start = serde_json::json!({"session_id":"s1","cwd":repo,"hook_event_name":"SessionStart"})
        .to_string();
    assert_eq!(
        sandbox.ask(&request("session_start", "claude-code", &start)),
        "{}"
    );
    wait_for(
        || sandbox.log().contains("not a recognized remote"),
        "warning logged",
    );
    assert!(!sandbox.log().contains("glpat-abc123xyz"));
}

#[test]
fn recall_abandoned_by_the_client_does_not_count_as_injected() {
    let sandbox = Sandbox::with_model();
    seed_deploy_fact(&sandbox);
    let repo = acme_web_repo(sandbox.dir.path());
    let _daemon = sandbox.start_daemon();
    sandbox.wait_for_embedder();
    let prompt = serde_json::json!({
        "session_id": "s1", "cwd": repo, "prompt": "how do we deploy?",
        "hook_event_name": "UserPromptSubmit",
    })
    .to_string();
    let line = request("recall", "claude-code", &prompt);
    {
        let stream = UnixStream::connect(sandbox.paths.sock()).unwrap();
        (&stream).write_all(format!("{line}\n").as_bytes()).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(sandbox.query_i64("SELECT count(*) FROM injected"), 0);
    assert!(sandbox.ask(&line).contains("ArgoCD"));
    wait_for(
        || sandbox.query_i64("SELECT count(*) FROM injected") == 1,
        "delivery marked",
    );
    assert_eq!(sandbox.ask(&line), "{}");
}

fn deploy_prompt(repo: &std::path::Path) -> String {
    let prompt = serde_json::json!({
        "session_id": "s1", "cwd": repo, "prompt": "how do we deploy?",
        "hook_event_name": "UserPromptSubmit",
    })
    .to_string();
    request("recall", "claude-code", &prompt)
}

fn session_start_line(repo: &std::path::Path, source: &str) -> String {
    let start = serde_json::json!({
        "session_id": "s1", "cwd": repo, "source": source, "hook_event_name": "SessionStart",
    })
    .to_string();
    request("session_start", "claude-code", &start)
}

#[test]
fn compaction_makes_memories_eligible_again() {
    let sandbox = Sandbox::with_model();
    // Episodes are never in the profile, so only the per-prompt path can deliver this one.
    let episode = memory(&embedder(), None, Some(WEB), DEPLOY, now_ms());
    sandbox.store().put_memory(&episode).unwrap();
    let repo = acme_web_repo(sandbox.dir.path());
    let _daemon = sandbox.start_daemon();
    sandbox.wait_for_embedder();
    let line = deploy_prompt(&repo);
    assert!(sandbox.ask(&line).contains("ArgoCD"));
    wait_for(
        || sandbox.query_i64("SELECT count(*) FROM injected") == 1,
        "delivery marked",
    );
    assert_eq!(sandbox.ask(&line), "{}");
    for (source, expect_eligible) in [("resume", false), ("compact", true)] {
        sandbox.ask(&session_start_line(&repo, source));
        assert_eq!(
            sandbox.ask(&line).contains("ArgoCD"),
            expect_eligible,
            "{source}"
        );
    }
}

#[test]
fn a_memory_in_the_profile_is_not_injected_again_on_a_prompt() {
    let sandbox = Sandbox::with_model();
    seed_deploy_fact(&sandbox);
    let repo = acme_web_repo(sandbox.dir.path());
    let _daemon = sandbox.start_daemon();
    sandbox.wait_for_embedder();
    assert!(
        sandbox
            .ask(&session_start_line(&repo, "startup"))
            .contains("ArgoCD")
    );
    wait_for(
        || sandbox.query_i64("SELECT count(*) FROM injected") == 1,
        "delivery marked",
    );
    assert_eq!(sandbox.ask(&deploy_prompt(&repo)), "{}");
}

#[test]
fn replay_keeps_entries_that_hit_a_passing_failure_and_drops_hopeless_ones() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    copy_transcript("claude-code", &dir);
    {
        let store = sandbox.store();
        store
            .upsert_session("seen", "claude-code", "p", None)
            .unwrap();
        drop(store);
        let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_queue BEFORE INSERT ON turn_queue
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
    }
    let entry = |session: &str, path: serde_json::Value| {
        serde_json::json!({
            "agent": "claude-code", "verb": "capture", "session_id": session,
            "transcript_path": path, "cwd": "/tmp/p",
        })
        .to_string()
    };
    let retriable = entry(
        "seen",
        dir.join("transcript.jsonl").display().to_string().into(),
    );
    let hopeless = entry("gone", "/nonexistent/t.jsonl".into());
    let no_transcript = entry("none", serde_json::Value::Null);
    fs::write(
        sandbox.paths.spool(),
        format!("{retriable}\n{hopeless}\n{no_transcript}\n"),
    )
    .unwrap();

    let daemon = sandbox.start_daemon();
    assert_eq!(
        fs::read_to_string(sandbox.paths.spool())
            .unwrap()
            .lines()
            .count(),
        1
    );
    daemon.terminate();

    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    conn.execute_batch("DROP TRIGGER fail_queue").unwrap();
    let _daemon = sandbox.start_daemon();
    assert_eq!(sandbox.turns().len(), 3);
    assert!(!sandbox.paths.spool().exists());
}

#[test]
fn failed_session_end_stays_in_the_spool() {
    let sandbox = Sandbox::with_model();
    sandbox
        .store()
        .upsert_session("s1", "claude-code", "p", None)
        .unwrap();
    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_end BEFORE UPDATE ON sessions
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    let line = serde_json::json!({
        "agent": "claude-code", "verb": "session_end", "session_id": "s1",
        "transcript_path": null, "cwd": "/tmp/p",
    });
    fs::write(sandbox.paths.spool(), format!("{line}\n")).unwrap();
    let _daemon = sandbox.start_daemon();
    assert!(sandbox.paths.spool().exists());
}

#[test]
fn spool_with_invalid_bytes_does_not_stop_startup() {
    let sandbox = Sandbox::with_model();
    fs::write(sandbox.paths.spool(), b"\xff\xfe\n").unwrap();
    let _daemon = sandbox.start_daemon();
    assert!(!sandbox.paths.spool().exists());
}

#[test]
fn daemon_makes_an_existing_home_private() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::with_model();
    fs::set_permissions(sandbox.paths.home(), fs::Permissions::from_mode(0o755)).unwrap();
    let _daemon = sandbox.start_daemon();
    let mode = fs::metadata(sandbox.paths.home())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn silent_client_is_disconnected() {
    let sandbox = Sandbox::with_model();
    let _daemon = sandbox.start_daemon();
    let stream = UnixStream::connect(sandbox.paths.sock()).unwrap();
    let mut reply = String::new();
    let read = std::io::Read::read_to_string(&mut &stream, &mut reply).unwrap();
    assert_eq!(read, 0);
}

#[test]
fn running_out_of_file_descriptors_does_not_stop_the_daemon() {
    let sandbox = Sandbox::without_model();
    let exe = env!("CARGO_BIN_EXE_loremaster");
    let child = std::process::Command::new("sh")
        .args(["-c", &format!("ulimit -n 40; exec '{exe}' daemon")])
        .env("LOREMASTER_HOME", sandbox.paths.home())
        .env(loremaster::embed::MODEL_URL_ENV, NO_MODEL_URL)
        .spawn()
        .unwrap();
    let _daemon = common::Daemon { child };
    wait_for(
        || UnixStream::connect(sandbox.paths.sock()).is_ok(),
        "socket",
    );
    sandbox.ask("ping");
    let idle: Vec<UnixStream> = (0..60)
        .filter_map(|_| UnixStream::connect(sandbox.paths.sock()).ok())
        .collect();
    wait_for(
        || sandbox.log().contains("cannot accept connection"),
        "accept failure",
    );
    drop(idle);
    wait_for(
        || {
            UnixStream::connect(sandbox.paths.sock()).is_ok_and(|stream| {
                (&stream).write_all(b"ping\n").is_ok()
                    && std::io::BufRead::read_line(
                        &mut std::io::BufReader::new(&stream),
                        &mut String::new(),
                    )
                    .is_ok_and(|n| n > 0)
            })
        },
        "service again",
    );
}

#[test]
fn simultaneous_starts_leave_one_daemon() {
    let sandbox = Sandbox::without_model();
    let children: Vec<_> = (0..4)
        .map(|_| {
            sandbox
                .daemon_command()
                .env(loremaster::embed::MODEL_URL_ENV, NO_MODEL_URL)
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    wait_for(
        || UnixStream::connect(sandbox.paths.sock()).is_ok(),
        "socket",
    );
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let mut alive = Vec::new();
    for mut child in children {
        match child.try_wait().unwrap() {
            Some(status) => assert_eq!(status.code(), Some(1)),
            None => alive.push(common::Daemon { child }),
        }
    }
    assert_eq!(alive.len(), 1);
}

#[test]
fn a_kill_after_the_last_reply_is_caught_up_on_restart() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    copy_transcript("claude-code", &dir);
    let daemon = sandbox.start_daemon();
    let blocker = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    // A session row comes first, so that the event below is stuck at its write.
    let start = hook_input("claude-code", "session_start.json", &dir, None);
    assert_eq!(
        sandbox.ask(&request("session_start", "claude-code", &start)),
        "{}"
    );
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let end_event = hook_input("claude-code", "session_end.json", &dir, None);
    assert_eq!(
        sandbox.ask(&request("session_end", "claude-code", &end_event)),
        "{}"
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    daemon.kill9();
    blocker.execute_batch("ROLLBACK").unwrap();
    let idle_since = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    fs::File::options()
        .write(true)
        .open(dir.join("transcript.jsonl"))
        .unwrap()
        .set_modified(idle_since)
        .unwrap();

    let _daemon = sandbox.start_daemon();
    assert_eq!(sandbox.turns().len(), 3);
    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    let project: String = conn
        .query_row("SELECT DISTINCT project_id FROM turn_queue", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(project, "fixture-project");
}

#[test]
fn open_session_without_a_transcript_is_passed_over_on_restart() {
    let sandbox = Sandbox::with_model();
    let store = sandbox.store();
    store
        .upsert_session("no-path", "claude-code", "p", None)
        .unwrap();
    store
        .upsert_session("gone", "claude-code", "p", Some("/nonexistent/t.jsonl"))
        .unwrap();
    drop(store);
    let _daemon = sandbox.start_daemon();
    assert!(sandbox.turns().is_empty());
}

#[test]
fn a_failed_capture_while_running_is_kept_for_the_next_start() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    copy_transcript("claude-code", &dir);
    let daemon = sandbox.start_daemon();
    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_queue BEFORE INSERT ON turn_queue
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    let end_event = hook_input("claude-code", "session_end.json", &dir, None);
    assert_eq!(
        sandbox.ask(&request("session_end", "claude-code", &end_event)),
        "{}"
    );
    wait_for(|| sandbox.paths.spool().exists(), "job kept in the spool");
    daemon.terminate();
    conn.execute_batch("DROP TRIGGER fail_queue").unwrap();

    let _daemon = sandbox.start_daemon();
    assert_eq!(sandbox.turns().len(), 3);
    assert!(!sandbox.paths.spool().exists());
}

#[test]
fn unreadable_memory_does_not_stop_startup() {
    let sandbox = Sandbox::with_model();
    seed_deploy_fact(&sandbox);
    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    conn.execute_batch("UPDATE memories SET kind = 'newkind'")
        .unwrap();
    let _daemon = sandbox.start_daemon();
    assert_eq!(sandbox.ask("garbage"), "{}");
}

#[test]
fn clean_stop_works_when_the_socket_file_is_gone() {
    let sandbox = Sandbox::with_model();
    let daemon = sandbox.start_daemon();
    fs::remove_file(sandbox.paths.sock()).unwrap();
    assert!(daemon.terminate().success());
}

#[test]
fn a_job_that_cannot_be_kept_for_retry_is_logged() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    copy_transcript("claude-code", &dir);
    let _daemon = sandbox.start_daemon();
    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_queue BEFORE INSERT ON turn_queue
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    fs::create_dir(sandbox.paths.spool()).unwrap();
    let end_event = hook_input("claude-code", "session_end.json", &dir, None);
    assert_eq!(
        sandbox.ask(&request("session_end", "claude-code", &end_event)),
        "{}"
    );
    wait_for(
        || sandbox.log().contains("cannot keep job for retry"),
        "failure logged",
    );
    fs::remove_dir(sandbox.paths.spool()).unwrap();
}

#[test]
fn a_restart_during_a_turn_leaves_that_turn_for_its_own_stop() {
    let sandbox = Sandbox::with_model();
    let dir = sandbox.dir.path().join("agent");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("transcript.jsonl");
    let prompt = serde_json::json!({"type":"user","message":{"content":"fix the build"}});
    let interim = serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"Let me look at the log"}]}});
    fs::write(&path, format!("{prompt}\n{interim}\n")).unwrap();
    let start = hook_input("claude-code", "session_start.json", &dir, None);
    let daemon = sandbox.start_daemon();
    sandbox.ask(&request("session_start", "claude-code", &start));
    daemon.kill9();

    let _daemon = sandbox.start_daemon();
    assert!(sandbox.turns().is_empty());

    let mut stop: serde_json::Value =
        serde_json::from_str(&hook_input("claude-code", "stop.json", &dir, None)).unwrap();
    stop["last_assistant_message"] = "The build is fixed".into();
    sandbox.ask(&request("capture", "claude-code", &stop.to_string()));
    wait_for(|| sandbox.turns().len() == 1, "turn captured");
    assert!(sandbox.turns()[0].contains("The build is fixed"));
}

#[test]
fn replay_and_catch_up_failures_do_not_stop_startup() {
    let sandbox = Sandbox::with_model();
    fs::create_dir_all(sandbox.paths.spool()).unwrap();
    drop(sandbox.store());
    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    conn.execute_batch("DROP TABLE sessions").unwrap();
    let _daemon = sandbox.start_daemon();
    assert_eq!(sandbox.ask("garbage"), "{}");
    assert!(sandbox.log().contains("cannot replay spool"));
    assert!(sandbox.log().contains("cannot catch up open sessions"));
}

#[test]
fn stopping_right_after_start_exits_cleanly() {
    for _ in 0..5 {
        let sandbox = Sandbox::with_model();
        assert!(sandbox.start_daemon().terminate().success());
    }
}
