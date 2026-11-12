//! `loremaster status` as a real process.

mod common;

use std::fs;
use std::process::Output;

use common::Sandbox;
use loremaster::embed::MODEL_ID;
use loremaster::now_ms;
use loremaster::store::StoredMemory;
use loremaster_core::{Kind, Record, RecordType, Scope, new_ulid};

fn status(sandbox: &Sandbox) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .arg("status")
        .env("LOREMASTER_HOME", sandbox.paths.home())
        .output()
        .unwrap()
}

fn embedded_fact() -> StoredMemory {
    StoredMemory {
        record: Record {
            user_id: None,
            memory_id: new_ulid(),
            seq: Some(1),
            hlc: "1761750000123-00000-dev_other".into(),
            writer: "dev_other".into(),
            record_type: Some(RecordType::Fact),
            kind: Some(Kind::Preference),
            scope: Some(Scope::User),
            project_id: None,
            text: Some("Prefers pnpm".into()),
            created_at: Some(now_ms()),
            superseded_by: None,
            deleted: false,
            evict_key: None,
        },
        vector: Some(vec![0.5; 384]),
        model_id: Some(MODEL_ID.into()),
    }
}

#[test]
fn a_logged_in_home_shows_its_device_and_progress() {
    let sandbox = Sandbox::without_model();
    fs::write(
        sandbox.paths.config_toml(),
        "api_url = \"https://api.example\"\n",
    )
    .unwrap();
    fs::write(
        sandbox.paths.credentials(),
        r#"{"device_id":"dev_01STATUS","token":"t"}"#,
    )
    .unwrap();
    let store = sandbox.store();
    let pulled: Vec<StoredMemory> = (0..1_200).map(|_| embedded_fact()).collect();
    store
        .apply_pulled(pulled, 1_200, 5_000, "dev_01STATUS")
        .unwrap();
    let mut local = [embedded_fact()];
    local[0].record.hlc = String::new();
    store
        .write_local(&mut local, "dev_01STATUS", now_ms())
        .unwrap();

    let output = status(&sandbox);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Sync: on, device dev_01STATUS\nMemories: 1,201 of 5,000 recallable\nOutbox: 1 waiting\n"
    );
}

#[test]
fn an_empty_home_shows_nothing_to_sync() {
    let sandbox = Sandbox::without_model();
    let output = status(&sandbox);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Sync: off: no api_url\nMemories: 0 recallable\nOutbox: 0 waiting\n"
    );
}

#[test]
fn a_store_from_a_newer_version_is_reported() {
    let sandbox = Sandbox::without_model();
    let conn = rusqlite::Connection::open(sandbox.paths.store_db()).unwrap();
    conn.execute_batch("PRAGMA user_version = 99;").unwrap();
    drop(conn);
    let output = status(&sandbox);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "store.db is from a newer Loremaster (schema 99)"
    );
    assert!(output.stdout.is_empty());
}
