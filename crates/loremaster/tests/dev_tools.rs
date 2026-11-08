//! `dev seed` and `dev bench`.
#![cfg(feature = "dev")]

mod common;

use std::fs;
use std::process::{Command, Output};

use common::{DeliveredRecall, Sandbox, embedder, shared_models};
use loremaster::embed::{MODEL_ID, MODEL_URL_ENV};
use loremaster::now_ms;
use loremaster::recall::Recall;

fn dev(sandbox: &Sandbox, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .arg("dev")
        .args(args)
        .env("LOREMASTER_HOME", sandbox.paths.home())
        .output()
        .unwrap()
}

fn seed_file(sandbox: &Sandbox, text: &str) -> String {
    let path = sandbox.dir.path().join("seed.jsonl");
    fs::write(&path, text).unwrap();
    path.display().to_string()
}

#[test]
fn seeded_memory_is_complete_and_recalled() {
    let sandbox = Sandbox::with_model();
    let file = seed_file(
        &sandbox,
        "{\"type\":\"fact\",\"kind\":\"preference\",\"scope\":\"user\",\"text\":\"Prefers pnpm over npm\"}\n",
    );
    let output = dev(&sandbox, &["seed", &file]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "seeded 1");

    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    let (id, created, hlc, vector, model): (String, i64, String, Vec<u8>, String) = conn
        .query_row(
            "SELECT memory_id, created_at, hlc, vector, model_id FROM memories",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(id.len(), 26);
    assert!((now_ms() - created).abs() < 120_000);
    assert!(hlc.ends_with("-local"));
    assert_eq!(vector.len(), 384 * 4);
    assert_eq!(model, MODEL_ID);

    let recall = Recall::new(sandbox.store()).unwrap();
    recall.set_embedder(embedder()).unwrap();
    let output = recall
        .for_prompt_delivered("s", "p", "which package manager should I use?", now_ms())
        .unwrap()
        .unwrap();
    assert!(
        output.contains("- [preference] Prefers pnpm over npm"),
        "{output}"
    );
}

#[test]
fn seed_keeps_given_ids_and_stores_tombstones_without_a_vector() {
    let sandbox = Sandbox::with_model();
    let file = seed_file(
        &sandbox,
        "{\"memory_id\":\"gone\",\"deleted\":true,\"hlc\":\"1761750000123-00000-x\",\"created_at\":5}\n",
    );
    let output = dev(&sandbox, &["seed", &file]);
    assert!(output.status.success(), "{output:?}");
    let stored = sandbox.store().get_memory("gone").unwrap().unwrap();
    assert!(stored.record.deleted);
    assert_eq!(stored.record.hlc, "1761750000123-00000-x");
    assert_eq!(stored.record.created_at, Some(5));
    assert_eq!(stored.vector, None);
}

#[test]
fn seed_with_a_login_writes_as_the_device_and_queues_each_memory() {
    let sandbox = Sandbox::with_model();
    fs::write(
        sandbox.paths.credentials(),
        r#"{"device_id":"dev_01SEEDDEVICE","token":"t"}"#,
    )
    .unwrap();
    let file = seed_file(
        &sandbox,
        "{\"type\":\"fact\",\"kind\":\"preference\",\"scope\":\"user\",\"text\":\"Prefers pnpm\"}\n\
         {\"type\":\"fact\",\"kind\":\"decision\",\"scope\":\"user\",\"text\":\"Uses UTC\"}\n",
    );
    let output = dev(&sandbox, &["seed", &file]);
    assert!(output.status.success(), "{output:?}");

    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    let mut query = conn
        .prepare(
            "SELECT m.writer, m.hlc, m.model_id FROM outbox o JOIN memories m USING (memory_id)
             ORDER BY o.rowid",
        )
        .unwrap();
    let rows: Vec<(String, String, String)> = query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 2);
    for (writer, hlc, model) in &rows {
        assert_eq!(writer, "dev_01SEEDDEVICE");
        assert!(hlc.ends_with("-dev_01SEEDDEVICE"), "{hlc}");
        assert_eq!(model, MODEL_ID);
    }
    assert!(rows[0].1 < rows[1].1);
    assert_eq!(sandbox.query_i64("SELECT count(*) FROM memories"), 2);
}

#[test]
fn seed_with_a_login_refuses_another_device_s_stamp() {
    let sandbox = Sandbox::with_model();
    fs::write(
        sandbox.paths.credentials(),
        r#"{"device_id":"dev_01SEEDDEVICE","token":"t"}"#,
    )
    .unwrap();
    let file = seed_file(
        &sandbox,
        "{\"type\":\"fact\",\"kind\":\"preference\",\"scope\":\"user\",\"text\":\"ok\"}\n\
         {\"memory_id\":\"m\",\"deleted\":true,\"hlc\":\"1761750000123-00000-dev_other\"}\n",
    );
    let output = dev(&sandbox, &["seed", &file]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "line 2: hlc writer is not this device"
    );
    assert!(!sandbox.paths.store_db().exists());
}

#[test]
fn seed_without_a_login_queues_nothing() {
    let sandbox = Sandbox::with_model();
    let file = seed_file(
        &sandbox,
        "{\"type\":\"fact\",\"kind\":\"preference\",\"scope\":\"user\",\"text\":\"Prefers pnpm\"}\n",
    );
    let output = dev(&sandbox, &["seed", &file]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(sandbox.query_i64("SELECT count(*) FROM outbox"), 0);
    assert_eq!(
        sandbox.query_i64("SELECT count(*) FROM memories WHERE writer = 'local'"),
        1
    );
}

#[test]
fn any_bad_line_seeds_nothing() {
    let sandbox = Sandbox::with_model();
    let long = "x".repeat(2001);
    let file = seed_file(
        &sandbox,
        &format!(
            "{{\"type\":\"fact\",\"kind\":\"pitfall\",\"scope\":\"user\",\"text\":\"{long}\"}}\n\
             {{\"type\":\"fact\",\"kind\":\"pitfall\",\"scope\":\"user\",\"text\":\"ok\"}}\n\
             not json\n\
             {{\"type\":\"episode\",\"scope\":\"project\",\"text\":\"t\"}}\n"
        ),
    );
    let output = dev(&sandbox, &["seed", &file]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "line 1: text over 2000 chars\nline 3: invalid JSON\nline 4: project scope needs project_id"
    );
    assert!(!sandbox.paths.store_db().exists());
}

#[test]
fn missing_file_is_reported() {
    let sandbox = Sandbox::with_model();
    let output = dev(&sandbox, &["seed", "/nonexistent/seed.jsonl"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "cannot read /nonexistent/seed.jsonl"
    );
}

fn without_network(sandbox: &Sandbox, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .arg("dev")
        .args(args)
        .env("LOREMASTER_HOME", sandbox.paths.home())
        .env(MODEL_URL_ENV, "http://127.0.0.1:1")
        .output()
        .unwrap()
}

#[test]
fn seeding_without_the_model_is_reported() {
    let sandbox = Sandbox::without_model();
    let file = seed_file(
        &sandbox,
        "{\"type\":\"fact\",\"kind\":\"pitfall\",\"scope\":\"user\",\"text\":\"t\"}\n",
    );
    let output = without_network(&sandbox, &["seed", &file]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "embedding model not available"
    );
}

#[test]
fn bench_without_the_model_is_reported() {
    let sandbox = Sandbox::without_model();
    let output = without_network(&sandbox, &["bench", "--runs", "1"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "embedding model not available"
    );
}

#[test]
fn bench_reports_every_verb_for_every_agent_and_cleans_up() {
    let sandbox = Sandbox::without_model();
    std::os::unix::fs::symlink(shared_models(), sandbox.paths.models()).unwrap();
    let before: Vec<_> = fs::read_dir(sandbox.paths.home())
        .unwrap()
        .count()
        .to_string()
        .into();
    let output = dev(&sandbox, &["bench", "--runs", "3"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("seeded 50000 memories in "), "{stdout}");
    assert!(stdout.contains("verb"), "{stdout}");
    let agent = "claude-code";
    for verb in [
        "session_start",
        "recall",
        "recall_long",
        "capture",
        "session_end",
    ] {
        let row = stdout
            .lines()
            .find(|line| line.starts_with(verb) && line.contains(agent));
        assert!(row.is_some(), "{verb} {agent}: {stdout}");
    }
    let failed = stdout.contains("FAIL");
    assert_eq!(output.status.success(), !failed, "{stdout}");
    let after: Vec<_> = fs::read_dir(sandbox.paths.home())
        .unwrap()
        .count()
        .to_string()
        .into();
    assert_eq!(before, after);
}
