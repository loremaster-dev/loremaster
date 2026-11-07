//! Push and pull against a fake sync service, in process and through the daemon.

mod common;

use std::fs;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::fake_service::{FakeService, Route};
use common::{DeliveredRecall, Sandbox, embedder, memory, request, wait_for};
use loremaster::embed::MODEL_ID;
use loremaster::now_ms;
use loremaster::recall::Recall;
use loremaster::store::StoredMemory;
use loremaster::sync::{Cadence, SyncClient, SyncError};
use loremaster_core::{DeviceCredential, Kind, Record, RecordType, Scope, new_ulid};
use serde_json::json;

const USER: &str = "gh:1";
const QUICK: Cadence = Cadence {
    outbox_check: Duration::from_millis(50),
    period: Duration::from_secs(3600),
    retry: Duration::from_millis(200),
};

fn set_api_url(sandbox: &Sandbox, url: &str) {
    fs::write(
        sandbox.paths.config_toml(),
        format!("api_url = \"{url}\"\n"),
    )
    .unwrap();
}

fn save_credential(sandbox: &Sandbox, credential: &DeviceCredential) {
    fs::write(
        sandbox.paths.credentials(),
        serde_json::to_string(credential).unwrap(),
    )
    .unwrap();
}

/// A home logged in to `fake` as a new device of the user.
fn home(fake: &FakeService, sandbox: Sandbox) -> (Sandbox, DeviceCredential) {
    set_api_url(&sandbox, &fake.url);
    let credential = fake.add_device(USER);
    save_credential(&sandbox, &credential);
    (sandbox, credential)
}

fn client(sandbox: &Sandbox) -> (Arc<Recall>, SyncClient) {
    let recall = Arc::new(Recall::new(sandbox.store()).unwrap());
    let client = SyncClient::new(recall.clone(), sandbox.paths.clone());
    (recall, client)
}

/// A user fact with no vector, as a test writes it locally.
fn fact(text: &str) -> StoredMemory {
    StoredMemory {
        record: Record {
            user_id: None,
            memory_id: new_ulid(),
            seq: None,
            hlc: String::new(),
            writer: String::new(),
            record_type: Some(RecordType::Fact),
            kind: Some(Kind::Preference),
            scope: Some(Scope::User),
            project_id: None,
            text: Some(text.to_owned()),
            created_at: Some(now_ms()),
            superseded_by: None,
            deleted: false,
            evict_key: None,
        },
        vector: None,
        model_id: None,
    }
}

fn write_local(sandbox: &Sandbox, credential: &DeviceCredential, memories: &mut [StoredMemory]) {
    sandbox
        .store()
        .write_local(memories, &credential.device_id, now_ms())
        .unwrap();
}

/// A record as another device of the user pushed it.
fn remote(text: &str, hlc_ms: u64) -> Record {
    Record {
        hlc: format!("{hlc_ms:013}-00000-dev_other"),
        writer: "dev_other".to_owned(),
        ..fact(text).record
    }
}

fn outbox(sandbox: &Sandbox) -> i64 {
    sandbox.query_i64("SELECT count(*) FROM outbox")
}

fn prompt(recall: &Recall, text: &str) -> Option<String> {
    recall
        .for_prompt_delivered(&new_ulid(), "p", text, now_ms())
        .unwrap()
}

/// A URL where nothing listens.
fn dead_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

#[test]
fn push_sends_the_outbox_in_batches_without_local_fields() {
    let fake = FakeService::start(100);
    let (sandbox, device) = home(&fake, Sandbox::without_model());
    let mut memories: Vec<StoredMemory> = (0..150).map(|n| fact(&format!("fact {n}"))).collect();
    for memory in &mut memories {
        memory.vector = Some(vec![0.5; 384]);
        memory.model_id = Some(MODEL_ID.to_owned());
        memory.record.seq = Some(7);
    }
    write_local(&sandbox, &device, &mut memories);
    let (_recall, mut client) = client(&sandbox);

    assert_eq!(client.push_outbox().unwrap(), 150);

    let pushes = fake.pushes();
    let sizes: Vec<usize> = pushes
        .iter()
        .map(|p| p["records"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, [99, 51]);
    for record in pushes.iter().flat_map(|p| p["records"].as_array().unwrap()) {
        for field in ["vector", "model_id", "seq", "user_id", "evict_key"] {
            assert!(record.get(field).is_none(), "{field} in {record}");
        }
        assert_eq!(record["writer"], device.device_id.as_str());
    }
    assert_eq!(fake.records(USER).len(), 150);
    assert_eq!(outbox(&sandbox), 0);
    assert_eq!(client.push_outbox().unwrap(), 0);
}

#[test]
fn pull_pages_until_done_and_embeds_what_it_stores() {
    let fake = FakeService::start(10);
    let (sandbox, _device) = home(&fake, Sandbox::with_model());
    let records: Vec<Record> = (0..25)
        .map(|n| remote(&format!("fact {n}"), 1_761_750_000_000 + n))
        .collect();
    fake.insert(USER, &records);
    let (recall, mut client) = client(&sandbox);
    recall.set_embedder(embedder()).unwrap();

    assert_eq!(client.pull().unwrap(), 25);

    assert_eq!(fake.requests(Route::Pull), 3);
    let store = sandbox.store();
    assert_eq!(store.sync_cursor().unwrap(), 25);
    for record in &records {
        let stored = store.get_memory(&record.memory_id).unwrap().unwrap();
        assert!(stored.record.seq.is_some());
        assert_eq!(stored.vector.map(|v| v.len()), Some(384));
        assert_eq!(stored.model_id.as_deref(), Some(MODEL_ID));
    }
    assert_eq!(client.pull().unwrap(), 0);
}

#[test]
fn own_pushes_that_come_back_are_not_stored_again() {
    let fake = FakeService::start(100);
    let (sandbox, device) = home(&fake, Sandbox::without_model());
    write_local(&sandbox, &device, &mut [fact("a"), fact("b")]);
    let (_recall, mut client) = client(&sandbox);
    client.push_outbox().unwrap();

    assert_eq!(client.pull().unwrap(), 0);
    assert_eq!(sandbox.store().sync_cursor().unwrap(), 2);
}

#[test]
fn an_older_pulled_change_is_ignored_and_a_tombstone_leaves_recall() {
    let fake = FakeService::start(100);
    let (sandbox, _device) = home(&fake, Sandbox::with_model());
    let embedder = embedder();
    let mut kept = memory(
        &embedder,
        Some(Kind::Preference),
        None,
        "Prefers pnpm over npm",
        now_ms(),
    );
    kept.record.hlc = "1761750009000-00000-dev_me".to_owned();
    let gone = memory(
        &embedder,
        Some(Kind::Decision),
        None,
        "Deploys use ArgoCD",
        now_ms(),
    );
    sandbox
        .store()
        .put_memories(&[kept.clone(), gone.clone()])
        .unwrap();
    let (recall, mut client) = client(&sandbox);
    recall.set_embedder(embedder).unwrap();
    assert!(prompt(&recall, "how do we deploy?").is_some());

    let older = Record {
        text: Some("Prefers yarn".to_owned()),
        ..remote("", 1_761_750_001_000)
    };
    let older = Record {
        memory_id: kept.record.memory_id.clone(),
        ..older
    };
    let tombstone = Record {
        memory_id: gone.record.memory_id.clone(),
        hlc: "1761750099000-00000-dev_other".to_owned(),
        writer: "dev_other".to_owned(),
        deleted: true,
        record_type: None,
        kind: None,
        scope: None,
        text: None,
        created_at: None,
        ..gone.record.clone()
    };
    fake.insert(USER, &[older, tombstone]);

    assert_eq!(client.pull().unwrap(), 1);
    let store = sandbox.store();
    let stored = store.get_memory(&kept.record.memory_id).unwrap().unwrap();
    assert_eq!(stored.record.text.as_deref(), Some("Prefers pnpm over npm"));
    assert!(
        store
            .get_memory(&gone.record.memory_id)
            .unwrap()
            .unwrap()
            .record
            .deleted
    );
    assert_eq!(prompt(&recall, "how do we deploy?"), None);
}

#[test]
fn rows_pulled_before_the_model_loads_get_vectors_when_it_does() {
    let fake = FakeService::start(100);
    let (sandbox, _device) = home(&fake, Sandbox::with_model());
    let record = remote("Deploys go through ArgoCD", 1_761_750_000_000);
    fake.insert(USER, std::slice::from_ref(&record));
    let (recall, mut client) = client(&sandbox);

    assert_eq!(client.pull().unwrap(), 1);
    let stored = sandbox
        .store()
        .get_memory(&record.memory_id)
        .unwrap()
        .unwrap();
    assert_eq!((stored.vector, stored.model_id), (None, None));

    recall.set_embedder(embedder()).unwrap();
    let stored = sandbox
        .store()
        .get_memory(&record.memory_id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.model_id.as_deref(), Some(MODEL_ID));
    let reply = prompt(&recall, "how do we deploy?").unwrap();
    assert!(reply.contains("ArgoCD"), "{reply}");
}

#[test]
fn a_refused_memory_leaves_the_outbox_and_the_rest_is_pushed() {
    let fake = FakeService::start(100);
    let (sandbox, device) = home(&fake, Sandbox::without_model());
    let mut memories = [fact("bad"), fact("good")];
    write_local(&sandbox, &device, &mut memories);
    let bad = &memories[0].record.memory_id;
    fake.script(
        Route::Push,
        StatusCode::BAD_REQUEST,
        json!({"error": format!("record {bad}: text must be 1 to 2,000 characters")}),
    );
    let (_recall, mut client) = client(&sandbox);

    assert_eq!(client.push_outbox().unwrap(), 1);
    let stored: Vec<String> = fake
        .records(USER)
        .into_iter()
        .map(|r| r.memory_id)
        .collect();
    assert_eq!(stored, [memories[1].record.memory_id.clone()]);
    assert_eq!(outbox(&sandbox), 0);
}

#[test]
fn a_refused_push_that_names_no_memory_keeps_the_outbox() {
    let fake = FakeService::start(100);
    let (sandbox, device) = home(&fake, Sandbox::without_model());
    write_local(&sandbox, &device, &mut [fact("a")]);
    fake.script(
        Route::Push,
        StatusCode::BAD_REQUEST,
        json!({"error": "bad JSON"}),
    );
    let (_recall, mut client) = client(&sandbox);

    assert!(
        matches!(client.push_outbox(), Err(SyncError::Refused(reason)) if reason == "bad JSON")
    );
    assert_eq!(outbox(&sandbox), 1);
}

#[test]
fn a_refused_token_is_not_sent_again_until_a_new_login() {
    let fake = FakeService::start(100);
    let (sandbox, device) = home(&fake, Sandbox::without_model());
    write_local(&sandbox, &device, &mut [fact("a")]);
    fake.script(
        Route::Push,
        StatusCode::UNAUTHORIZED,
        json!({"error": "unknown token"}),
    );
    let (_recall, mut client) = client(&sandbox);

    assert!(matches!(client.push_outbox(), Err(SyncError::Unauthorized)));
    assert!(matches!(client.pull(), Err(SyncError::Unauthorized)));
    assert_eq!(fake.requests(Route::Push), 1);
    assert_eq!(fake.requests(Route::Pull), 0);
    assert_eq!(outbox(&sandbox), 1);

    save_credential(&sandbox, &fake.new_token(&device));
    assert_eq!(client.push_outbox().unwrap(), 1);
    assert_eq!(outbox(&sandbox), 0);
}

#[test]
fn conflicts_are_tried_three_times() {
    let fake = FakeService::start(100);
    let (sandbox, device) = home(&fake, Sandbox::without_model());
    write_local(&sandbox, &device, &mut [fact("a")]);
    let (_recall, mut client) = client(&sandbox);
    let busy = || json!({"error": "retry"});

    for _ in 0..3 {
        fake.script(Route::Push, StatusCode::CONFLICT, busy());
    }
    assert!(matches!(client.push_outbox(), Err(SyncError::Conflict)));
    assert_eq!(outbox(&sandbox), 1);

    for _ in 0..2 {
        fake.script(Route::Push, StatusCode::CONFLICT, busy());
        fake.script(Route::Pull, StatusCode::CONFLICT, busy());
    }
    assert_eq!(client.push_outbox().unwrap(), 1);
    assert_eq!(fake.requests(Route::Push), 6);
    assert_eq!(client.pull().unwrap(), 0);
    assert_eq!(fake.requests(Route::Pull), 3);
}

#[test]
fn server_errors_and_bad_replies_mean_unavailable() {
    let fake = FakeService::start(100);
    let (sandbox, _device) = home(&fake, Sandbox::without_model());
    let (_recall, mut client) = client(&sandbox);
    fake.script(
        Route::Pull,
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error": "internal error"}),
    );
    assert!(matches!(client.pull(), Err(SyncError::Unavailable(_))));
    fake.script(Route::Pull, StatusCode::OK, json!({"records": "no"}));
    assert!(matches!(client.pull(), Err(SyncError::Unavailable(_))));

    set_api_url(&sandbox, &dead_url());
    assert!(matches!(client.pull(), Err(SyncError::Unavailable(_))));
}

#[test]
fn no_service_or_no_login_sends_nothing() {
    let fake = FakeService::start(100);
    let sandbox = Sandbox::without_model();
    let (_recall, mut client) = client(&sandbox);
    save_credential(&sandbox, &fake.add_device(USER));
    assert!(matches!(client.pull(), Err(SyncError::NotConfigured)));

    fs::remove_file(sandbox.paths.credentials()).unwrap();
    set_api_url(&sandbox, &fake.url);
    assert!(matches!(
        client.push_outbox(),
        Err(SyncError::NotConfigured)
    ));
    assert_eq!(fake.requests(Route::Push) + fake.requests(Route::Pull), 0);
}

#[test]
fn writes_from_another_process_are_pushed_after_a_late_login() {
    let fake = FakeService::start(100);
    let sandbox = Sandbox::without_model();
    set_api_url(&sandbox, &fake.url);
    let (_recall, client) = client(&sandbox);
    let sync = client.spawn(QUICK);

    let device = fake.add_device(USER);
    save_credential(&sandbox, &device);
    write_local(&sandbox, &device, &mut [fact("a")]);
    wait_for(|| fake.records(USER).len() == 1, "push");
    wait_for(|| outbox(&sandbox) == 0, "empty outbox");
    sync.stop().unwrap();
}

#[test]
fn an_unreachable_service_is_tried_again_until_it_is_back() {
    let fake = FakeService::start(100);
    let (sandbox, device) = home(&fake, Sandbox::without_model());
    set_api_url(&sandbox, &dead_url());
    let (_recall, client) = client(&sandbox);
    let sync = client.spawn(QUICK);
    write_local(&sandbox, &device, &mut [fact("a")]);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(outbox(&sandbox), 1);

    fake.insert(USER, &[remote("from B", 1_761_750_000_000)]);
    set_api_url(&sandbox, &fake.url);
    wait_for(|| outbox(&sandbox) == 0, "push after the outage");
    wait_for(
        || sandbox.store().sync_cursor().unwrap() == 2,
        "pull after the outage",
    );
    sync.stop().unwrap();
}

#[test]
fn a_pull_trigger_pulls_at_once() {
    let fake = FakeService::start(100);
    let (sandbox, _device) = home(&fake, Sandbox::without_model());
    let (_recall, client) = client(&sandbox);
    let sync = client.spawn(QUICK);
    wait_for(|| fake.requests(Route::Pull) == 1, "pull at start");

    let record = remote("late", 1_761_750_000_000);
    fake.insert(USER, std::slice::from_ref(&record));
    sync.triggers().pull();
    wait_for(
        || {
            sandbox
                .store()
                .get_memory(&record.memory_id)
                .unwrap()
                .is_some()
        },
        "triggered pull",
    );
    sync.triggers().push();
    sync.stop().unwrap();
}

#[test]
fn the_period_pushes_and_pulls_with_no_trigger() {
    let fake = FakeService::start(100);
    let (sandbox, device) = home(&fake, Sandbox::without_model());
    let (_recall, client) = client(&sandbox);
    let sync = client.spawn(Cadence {
        outbox_check: Duration::from_secs(3600),
        period: Duration::from_millis(100),
        retry: Duration::from_secs(3600),
    });
    wait_for(|| fake.requests(Route::Pull) == 1, "pull at start");

    write_local(&sandbox, &device, &mut [fact("mine")]);
    let theirs = remote("theirs", 1_761_750_000_000);
    fake.insert(USER, std::slice::from_ref(&theirs));
    wait_for(|| outbox(&sandbox) == 0, "push on the period");
    wait_for(
        || {
            sandbox
                .store()
                .get_memory(&theirs.memory_id)
                .unwrap()
                .is_some()
        },
        "pull on the period",
    );
    sync.stop().unwrap();
}

fn user_prompt(sandbox: &Sandbox, text: &str) -> String {
    let event = json!({
        "session_id": "s1", "cwd": sandbox.dir.path(), "prompt": text,
        "hook_event_name": "UserPromptSubmit",
    })
    .to_string();
    sandbox.ask(&request("recall", "claude-code", &event))
}

fn session_start(sandbox: &Sandbox) -> String {
    let event = json!({
        "session_id": "s1", "cwd": sandbox.dir.path(), "hook_event_name": "SessionStart",
    })
    .to_string();
    sandbox.ask(&request("session_start", "claude-code", &event))
}

#[test]
fn a_fact_written_on_one_home_is_recalled_on_another() {
    let fake = FakeService::start(100);
    let (a, a_device) = home(&fake, Sandbox::with_model());
    let (b, _) = home(&fake, Sandbox::with_model());
    let _a_daemon = a.start_daemon();
    let b_daemon = b.start_daemon();
    b.wait_for_embedder();

    let mut deploy = [memory(
        &embedder(),
        Some(Kind::Decision),
        None,
        "Deploys go through ArgoCD",
        now_ms(),
    )];
    deploy[0].record.hlc = String::new();
    write_local(&a, &a_device, &mut deploy);
    wait_for(|| fake.records(USER).len() == 1, "push from A");

    session_start(&b);
    let id = deploy[0].record.memory_id.clone();
    wait_for(
        || {
            b.store()
                .get_memory(&id)
                .unwrap()
                .is_some_and(|m| m.vector.is_some())
        },
        "pull on B",
    );
    let stored = b.store().get_memory(&id).unwrap().unwrap();
    assert_eq!(stored.model_id.as_deref(), Some(MODEL_ID));
    let reply = user_prompt(&b, "how do we deploy?");
    assert!(reply.contains("ArgoCD"), "{reply}");
    assert!(b_daemon.terminate().success());
}

#[test]
fn a_killed_daemon_pushes_its_outbox_after_restart() {
    let fake = FakeService::start(100);
    let (sandbox, device) = home(&fake, Sandbox::with_model());
    set_api_url(&sandbox, &dead_url());
    let daemon = sandbox.start_daemon();
    write_local(&sandbox, &device, &mut [fact("survives a crash")]);
    daemon.kill9();
    assert_eq!(outbox(&sandbox), 1);

    set_api_url(&sandbox, &fake.url);
    let _daemon = sandbox.start_daemon();
    wait_for(|| fake.records(USER).len() == 1, "push after restart");
    wait_for(|| outbox(&sandbox) == 0, "empty outbox");
}

#[test]
fn a_daemon_with_no_login_sends_nothing_and_still_recalls() {
    let fake = FakeService::start(100);
    let sandbox = Sandbox::with_model();
    set_api_url(&sandbox, &fake.url);
    sandbox
        .store()
        .put_memory(&memory(
            &embedder(),
            Some(Kind::Decision),
            None,
            "Deploys go through ArgoCD",
            now_ms(),
        ))
        .unwrap();
    sandbox
        .store()
        .write_local(&mut [fact("queued")], "local", now_ms())
        .unwrap();
    let _daemon = sandbox.start_daemon();
    sandbox.wait_for_embedder();
    wait_for(|| sandbox.log().contains("sync is off"), "sync off log");

    let reply = user_prompt(&sandbox, "how do we deploy?");
    assert!(reply.contains("ArgoCD"), "{reply}");
    assert_eq!(fake.requests(Route::Push) + fake.requests(Route::Pull), 0);
    assert_eq!(outbox(&sandbox), 1);
}
