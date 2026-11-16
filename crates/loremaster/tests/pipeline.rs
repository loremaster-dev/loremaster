//! The pipeline from queued turns to stored facts, against the fake service.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::fake_service::{FakeService, Route};
use common::{
    Sandbox, embedder, hook_input, memory, request, save_credential, set_api_url, wait_for,
};
use loremaster::now_ms;
use loremaster::pipeline::{Pause, Pipeline, Step};
use loremaster::recall::Recall;
use loremaster::store::{QueuedTurn, StoredMemory};
use loremaster_core::{DeviceCredential, Kind, Record, RecordType, Scope};
use serde_json::{Value, json};

const USER: &str = "gh:1001";
const WEB: &str = "github.com/acme/web";
const PNPM: &str = "Prefers pnpm over npm";

/// A logged-in home with the model loaded, whose pipeline runs only when a test asks it to.
struct Home {
    sandbox: Sandbox,
    device: DeviceCredential,
    recall: Arc<Recall>,
    pipeline: Pipeline,
}

impl Home {
    fn new(fake: &FakeService, memories: &[StoredMemory]) -> Self {
        let sandbox = Sandbox::with_model();
        set_api_url(&sandbox, &fake.url);
        let device = fake.add_device(USER);
        save_credential(&sandbox, &device);
        sandbox.store().put_memories(memories).unwrap();
        let recall = Arc::new(Recall::new(sandbox.store()).unwrap());
        recall
            .set_embedder(embedder(), Some(&device.device_id))
            .unwrap();
        let pipeline = Pipeline::new(recall.clone(), sandbox.paths.clone());
        Self {
            sandbox,
            device,
            recall,
            pipeline,
        }
    }

    /// Queues a prepared turn of a session in `project_id`.
    fn queue(&self, project_id: &str, turn: &str) {
        let store = self.sandbox.store();
        store
            .upsert_session("s1", "claude-code", project_id, None)
            .unwrap();
        store
            .commit_turn(QueuedTurn {
                session_id: "s1",
                agent: "claude-code",
                project_id,
                turn,
                next_offset: 0,
                now_ms: now_ms(),
            })
            .unwrap();
    }

    fn statuses(&self) -> Vec<(String, i64)> {
        let store = self.sandbox.paths.store_db();
        let conn = rusqlite::Connection::open(store).unwrap();
        conn.prepare("SELECT status, attempts FROM turn_queue ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// Live and replaced facts, oldest first.
    fn facts(&self) -> Vec<Record> {
        let mut facts = Vec::new();
        self.sandbox
            .store()
            .for_each_memory(|memory| facts.push(memory.record))
            .unwrap();
        facts.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
        facts
    }

    fn outbox(&self) -> Vec<String> {
        self.sandbox
            .store()
            .outbox_batch(100)
            .unwrap()
            .into_iter()
            .map(|memory| memory.record.memory_id)
            .collect()
    }
}

fn candidate(text: &str, scope: &str) -> Value {
    json!({"text": text, "scope": scope, "kind": "preference"})
}

fn script_extract(fake: &FakeService, candidates: &[Value]) {
    fake.script(
        Route::Extract,
        StatusCode::OK,
        json!({ "candidates": candidates }),
    );
}

fn stored_fact(text: &str, project: Option<&str>) -> StoredMemory {
    memory(&embedder(), Some(Kind::Preference), project, text, now_ms())
}

#[test]
fn a_new_candidate_is_stored_and_queued_without_reconcile() {
    let fake = FakeService::start(100);
    let mut home = Home::new(&fake, &[]);
    home.queue(WEB, r#"{"prompt":"I prefer pnpm over npm"}"#);
    script_extract(&fake, &[candidate(PNPM, "user")]);
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);

    let facts = home.facts();
    assert_eq!(facts.len(), 1);
    let fact = &facts[0];
    assert_eq!(fact.text.as_deref(), Some(PNPM));
    assert_eq!(fact.kind, Some(Kind::Preference));
    assert_eq!(fact.scope, Some(Scope::User));
    assert_eq!(fact.project_id, None);
    assert_eq!(fact.record_type, Some(RecordType::Fact));
    assert_eq!(fact.writer, home.device.device_id);
    assert_eq!(home.outbox(), [fact.memory_id.as_str()]);
    assert_eq!(fake.requests(Route::Reconcile), 0);
    assert_eq!(home.statuses(), [("extracted".to_owned(), 0)]);
    assert_eq!(
        fake.bodies(Route::Extract),
        [json!({"turn": r#"{"prompt":"I prefer pnpm over npm"}"#})]
    );
    let recalled = home
        .recall
        .for_prompt("s2", WEB, "should I use pnpm or npm?", now_ms())
        .unwrap()
        .unwrap();
    assert_eq!(recalled.memory_ids, [fact.memory_id.as_str()]);
}

#[test]
fn a_candidate_that_is_already_stored_is_skipped() {
    let fake = FakeService::start(100);
    let mut home = Home::new(&fake, &[stored_fact(PNPM, None)]);
    home.queue(WEB, "{}");
    script_extract(&fake, &[candidate(PNPM, "user")]);
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    assert_eq!(home.facts().len(), 1);
    assert!(home.outbox().is_empty());
    assert_eq!(fake.requests(Route::Reconcile), 0);
    assert_eq!(home.statuses(), [("extracted".to_owned(), 0)]);
}

#[test]
fn the_same_text_in_another_scope_or_project_is_new() {
    let fake = FakeService::start(100);
    let mut home = Home::new(&fake, &[stored_fact(PNPM, Some("github.com/acme/api"))]);
    home.queue(WEB, "{}");
    script_extract(
        &fake,
        &[
            candidate(PNPM, "project"),
            candidate("Uses tabs for indents", "user"),
        ],
    );
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    let written: Vec<(Option<Scope>, Option<String>)> = home
        .facts()
        .into_iter()
        .filter(|fact| fact.writer == home.device.device_id)
        .map(|fact| (fact.scope, fact.project_id))
        .collect();
    assert_eq!(
        written,
        [
            (Some(Scope::Project), Some(WEB.to_owned())),
            (Some(Scope::User), None)
        ]
    );
}

#[test]
fn a_candidate_over_the_length_limit_or_empty_is_dropped() {
    let fake = FakeService::start(100);
    let mut home = Home::new(&fake, &[]);
    home.queue(WEB, "{}");
    let longest = "y".repeat(2_000);
    script_extract(
        &fake,
        &[
            candidate(&"x".repeat(2_001), "user"),
            candidate("  \n ", "user"),
            candidate(&format!("  {longest} "), "user"),
        ],
    );
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    let texts: Vec<Option<String>> = home.facts().into_iter().map(|fact| fact.text).collect();
    assert_eq!(texts, [Some(longest)]);
    assert_eq!(home.statuses(), [("extracted".to_owned(), 0)]);
}

#[test]
fn a_turn_with_no_candidates_is_done_and_the_queue_then_empty() {
    let fake = FakeService::start(100);
    let mut home = Home::new(&fake, &[]);
    home.queue(WEB, "{}");
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    assert!(home.facts().is_empty());
    assert_eq!(home.statuses(), [("extracted".to_owned(), 0)]);
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Idle);
    assert_eq!(fake.requests(Route::Extract), 1);
}

/// Similarity 0.87 to `PNPM` with the pinned model: between the new-fact and the duplicate limits.
const YARN: &str = "Prefers yarn over npm";
const TABS: &str = "Indents with tabs";

fn script_decision(fake: &FakeService, decision: Value) {
    fake.script(Route::Reconcile, StatusCode::OK, decision);
}

#[test]
fn a_related_candidate_is_reconciled_with_its_neighbours() {
    let fake = FakeService::start(100);
    let pnpm = stored_fact(PNPM, None);
    let tabs = stored_fact(TABS, None);
    let other_project = stored_fact(PNPM, Some(WEB));
    let mut home = Home::new(&fake, &[pnpm.clone(), tabs.clone(), other_project]);
    // Skip first: once the fact is added, the same candidate is a duplicate and needs no reconcile.
    for decision in ["skip", "add"] {
        home.queue(WEB, "{}");
        script_extract(&fake, &[candidate(YARN, "user")]);
        script_decision(&fake, json!({ "decision": decision }));
        assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    }
    let reconciles = fake.bodies(Route::Reconcile);
    assert_eq!(reconciles.len(), 2);
    assert_eq!(
        reconciles[0],
        json!({
            "candidate": candidate(YARN, "user"),
            "neighbours": [
                {"memory_id": pnpm.record.memory_id, "text": PNPM},
                {"memory_id": tabs.record.memory_id, "text": TABS},
            ],
        })
    );
    let yarn: Vec<Record> = home
        .facts()
        .into_iter()
        .filter(|fact| fact.text.as_deref() == Some(YARN))
        .collect();
    assert_eq!(yarn.len(), 1, "add writes the fact, skip writes nothing");
    assert_eq!(home.outbox(), [yarn[0].memory_id.as_str()]);
    assert_eq!(
        home.statuses(),
        [("extracted".to_owned(), 0), ("extracted".to_owned(), 0)]
    );
}

#[test]
fn a_superseding_candidate_replaces_the_old_fact() {
    let fake = FakeService::start(100);
    let pnpm = stored_fact(PNPM, None);
    let old_id = pnpm.record.memory_id.clone();
    let mut home = Home::new(&fake, &[pnpm]);
    home.queue(WEB, "{}");
    script_extract(&fake, &[candidate(YARN, "user")]);
    let merged = "Prefers yarn over npm and pnpm";
    script_decision(
        &fake,
        json!({"decision": "supersede", "memory_id": old_id, "text": merged}),
    );
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);

    let facts = home.facts();
    let old = facts.iter().find(|f| f.memory_id == old_id).unwrap();
    let new = facts.iter().find(|f| f.memory_id != old_id).unwrap();
    assert_eq!(facts.len(), 2);
    assert_eq!(new.text.as_deref(), Some(merged));
    assert_eq!(new.kind, Some(Kind::Preference));
    let vector = home
        .sandbox
        .store()
        .get_memory(&new.memory_id)
        .unwrap()
        .unwrap()
        .vector;
    assert_eq!(vector, embedder().embed(&[merged]).unwrap().pop());
    assert_eq!(old.superseded_by.as_deref(), Some(new.memory_id.as_str()));
    assert_eq!(old.text.as_deref(), Some(PNPM));
    let mut outbox = home.outbox();
    outbox.sort();
    assert_eq!(outbox, [old_id.clone(), new.memory_id.clone()]);
    let recalled = home
        .recall
        .for_prompt("s2", WEB, "which package manager do I prefer?", now_ms())
        .unwrap()
        .unwrap();
    assert_eq!(recalled.memory_ids, [new.memory_id.as_str()]);
}

#[test]
fn superseding_a_fact_that_is_gone_adds_the_candidate() {
    let fake = FakeService::start(100);
    let pnpm = stored_fact(PNPM, None);
    let mut gone = stored_fact("Prefers npm only", None);
    gone.record.deleted = true;
    gone.record.text = None;
    let gone_id = gone.record.memory_id.clone();
    let mut home = Home::new(&fake, &[pnpm, gone.clone()]);
    home.queue(WEB, "{}");
    script_extract(&fake, &[candidate(YARN, "user")]);
    script_decision(
        &fake,
        json!({"decision": "supersede", "memory_id": gone_id, "text": YARN}),
    );
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    let facts = home.facts();
    assert_eq!(facts.len(), 3);
    assert!(facts.iter().all(|fact| fact.superseded_by.is_none()));
    let after = facts.iter().find(|f| f.memory_id == gone_id).unwrap();
    assert_eq!(after, &gone.record);
}

fn pending(attempts: i64) -> (String, i64) {
    ("pending".to_owned(), attempts)
}

#[test]
fn a_service_that_cannot_answer_keeps_the_turn_and_counts_nothing() {
    let fake = FakeService::start(100);
    let mut home = Home::new(&fake, &[]);
    home.queue(WEB, "{}");
    let cases = [
        (StatusCode::SERVICE_UNAVAILABLE, Pause::Unavailable),
        (StatusCode::GATEWAY_TIMEOUT, Pause::Unavailable),
        (StatusCode::TOO_MANY_REQUESTS, Pause::DailyCap),
    ];
    for (status, pause) in cases {
        fake.script(Route::Extract, status, json!({"error": "busy"}));
        assert_eq!(
            home.pipeline.process_next().unwrap(),
            Step::Retry(pause),
            "{status}"
        );
        assert_eq!(home.statuses(), [pending(0)], "{status}");
    }

    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    set_api_url(&home.sandbox, &url);
    assert_eq!(
        home.pipeline.process_next().unwrap(),
        Step::Retry(Pause::Unavailable)
    );
    assert_eq!(home.statuses(), [pending(0)]);
}

#[test]
fn a_refused_device_waits_for_a_new_login() {
    let fake = FakeService::start(100);
    let mut home = Home::new(&fake, &[]);
    home.queue(WEB, "{}");
    fake.script(
        Route::Extract,
        StatusCode::UNAUTHORIZED,
        json!({"error": "no"}),
    );
    assert_eq!(
        home.pipeline.process_next().unwrap(),
        Step::Retry(Pause::Login)
    );
    assert_eq!(
        home.pipeline.process_next().unwrap(),
        Step::Retry(Pause::Login)
    );
    assert_eq!(fake.requests(Route::Extract), 1);
    assert_eq!(home.statuses(), [pending(0)]);

    save_credential(&home.sandbox, &fake.new_token(&home.device));
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    assert_eq!(home.statuses(), [("extracted".to_owned(), 0)]);
}

#[test]
fn a_turn_that_keeps_failing_is_given_up_after_five_tries() {
    let fake = FakeService::start(100);
    let mut home = Home::new(&fake, &[]);
    home.queue(WEB, "first");
    home.queue(WEB, "second");
    let failures = [
        (
            StatusCode::BAD_GATEWAY,
            json!({"error": "model is unavailable"}),
        ),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "internal"}),
        ),
        // A reply that does not parse.
        (StatusCode::OK, json!("candidates")),
        (StatusCode::OK, json!({"candidates": "none"})),
    ];
    for (attempt, (status, body)) in failures.into_iter().enumerate() {
        fake.script(Route::Extract, status, body);
        assert_eq!(
            home.pipeline.process_next().unwrap(),
            Step::Retry(Pause::Unavailable)
        );
        assert_eq!(home.statuses(), [pending(attempt as i64 + 1), pending(0)]);
    }
    fake.script(
        Route::Extract,
        StatusCode::BAD_GATEWAY,
        json!({"error": "again"}),
    );
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    assert_eq!(home.statuses(), [("failed".to_owned(), 5), pending(0)]);

    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    assert_eq!(
        fake.bodies(Route::Extract).last(),
        Some(&json!({"turn": "second"}))
    );
    assert_eq!(home.statuses()[1], ("extracted".to_owned(), 0));
}

#[test]
fn a_turn_the_service_can_never_take_is_given_up_at_once() {
    let fake = FakeService::start(100);
    let mut home = Home::new(&fake, &[]);
    for status in [StatusCode::BAD_REQUEST, StatusCode::PAYLOAD_TOO_LARGE] {
        home.queue(WEB, "{}");
        fake.script(Route::Extract, status, json!({"error": "no"}));
        assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    }
    assert_eq!(
        home.statuses(),
        [("failed".to_owned(), 0), ("failed".to_owned(), 0)]
    );
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Idle);
}

#[test]
fn a_retried_turn_does_not_store_a_fact_twice() {
    let fake = FakeService::start(100);
    let mut home = Home::new(&fake, &[]);
    home.queue(WEB, "{}");
    let candidates = [candidate(PNPM, "user"), candidate(YARN, "user")];
    script_extract(&fake, &candidates);
    fake.script(
        Route::Reconcile,
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"error": "busy"}),
    );
    assert_eq!(
        home.pipeline.process_next().unwrap(),
        Step::Retry(Pause::Unavailable)
    );
    assert_eq!(home.statuses(), [pending(0)]);
    let texts = |home: &Home| -> Vec<String> {
        let mut texts: Vec<String> = home.facts().into_iter().filter_map(|f| f.text).collect();
        texts.sort();
        texts
    };
    assert_eq!(texts(&home), [PNPM]);

    script_extract(&fake, &candidates);
    assert_eq!(home.pipeline.process_next().unwrap(), Step::Handled);
    assert_eq!(texts(&home), [PNPM, YARN]);
    assert_eq!(home.statuses(), [("extracted".to_owned(), 0)]);
}

const AGENT: &str = "claude-code";

/// A home logged in to `fake` for a daemon, with the fixture transcript in `agent/`.
fn daemon_home(fake: &FakeService) -> (Sandbox, String) {
    let sandbox = Sandbox::with_model();
    let device = fake.add_device(USER);
    save_credential(&sandbox, &device);
    let dir = sandbox.dir.path().join("agent");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        common::fixture_dir(AGENT).join("transcript.jsonl"),
        dir.join("transcript.jsonl"),
    )
    .unwrap();
    let stop = request(
        "capture",
        AGENT,
        &hook_input(AGENT, "stop.json", &dir, None),
    );
    (sandbox, stop)
}

fn count(sandbox: &Sandbox, status: &str) -> i64 {
    sandbox.query_i64(&format!(
        "SELECT count(*) FROM turn_queue WHERE status = '{status}' AND attempts = 0"
    ))
}

#[test]
fn turns_captured_while_the_service_is_down_wait_and_are_extracted_later() {
    let fake = FakeService::start(100);
    let (sandbox, stop) = daemon_home(&fake);
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    set_api_url(
        &sandbox,
        &format!("http://{}", closed.local_addr().unwrap()),
    );
    drop(closed);

    let daemon = sandbox.start_daemon();
    sandbox.wait_for_embedder();
    assert_eq!(sandbox.ask(&stop), "{}");
    wait_for(|| sandbox.turns().len() == 3, "three queued turns");
    wait_for(
        || sandbox.log().contains("service unreachable"),
        "a failed extract",
    );
    assert!(daemon.terminate().success());
    assert_eq!(count(&sandbox, "pending"), 3);

    set_api_url(&sandbox, &fake.url);
    script_extract(&fake, &[candidate(PNPM, "user")]);
    let _daemon = sandbox.start_daemon();
    wait_for(
        || count(&sandbox, "extracted") == 3,
        "three extracted turns",
    );
    wait_for(
        || {
            fake.records(USER)
                .iter()
                .any(|record| record.text.as_deref() == Some(PNPM))
        },
        "the fact pushed",
    );
}

#[test]
fn a_captured_turn_wakes_an_idle_pipeline() {
    let fake = FakeService::start(100);
    let (sandbox, stop) = daemon_home(&fake);
    set_api_url(&sandbox, &fake.url);
    {
        let store = sandbox.store();
        store.upsert_session("s0", AGENT, WEB, None).unwrap();
        store
            .commit_turn(QueuedTurn {
                session_id: "s0",
                agent: AGENT,
                project_id: WEB,
                turn: "{}",
                next_offset: 0,
                now_ms: now_ms(),
            })
            .unwrap();
    }
    let _daemon = sandbox.start_daemon();
    wait_for(|| count(&sandbox, "extracted") == 1, "the queued turn");

    // The queue is empty now, so only a wake from the capture starts the pipeline again soon.
    assert_eq!(sandbox.ask(&stop), "{}");
    wait_for(|| count(&sandbox, "extracted") == 4, "the captured turns");
}
