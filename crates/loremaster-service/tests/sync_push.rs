//! `POST /v1/sync/push` against DynamoDB Local.

mod common;

use axum::http::StatusCode;
use common::{
    ALICE, BOB, BOB_ID, Item, PUSH, Service, alice, counters, device, fact, hlc, memory, push,
    push_ok, string, tombstone,
};
use serde_json::{Value, json};

fn seq(item: &Item) -> i64 {
    item["seq"].as_n().unwrap().parse().unwrap()
}

#[tokio::test]
async fn new_records_get_unique_seqs() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let (a, b) = (loremaster_core::new_ulid(), loremaster_core::new_ulid());
    let applied = push_ok(
        &service,
        &laptop.token,
        &[
            fact(&a, 1_000, &laptop.device_id, "Prefers pnpm"),
            json!({
                "memory_id": b,
                "hlc": hlc(1_001, &laptop.device_id),
                "writer": laptop.device_id,
                "type": "episode",
                "scope": "project",
                "project_id": "github.com/a/b",
                "text": "Fixed the flaky test",
                "created_at": 1_761_750_000_500_u64,
            }),
        ],
    )
    .await;
    assert_eq!(applied, [a.clone(), b.clone()]);

    let first = memory(&service, &alice(), &a).await.unwrap();
    let second = memory(&service, &alice(), &b).await.unwrap();
    assert_eq!((seq(&first), seq(&second)), (1, 2));
    assert_eq!(string(&first, "user_id"), alice());
    assert_eq!(string(&first, "text"), "Prefers pnpm");
    assert_eq!(string(&first, "kind"), "preference");
    assert_eq!(string(&first, "evict_key"), "3#1761750000000");
    assert_eq!(string(&second, "project_id"), "github.com/a/b");
    assert_eq!(string(&second, "evict_key"), "2#1761750000500");
    assert!(!second.contains_key("kind"));
    assert_eq!(counters(&service, &alice()).await, (2, 2));
}

#[tokio::test]
async fn replaced_fact_is_evicted_first() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let id = loremaster_core::new_ulid();
    let mut record = fact(&id, 1_000, &laptop.device_id, "Uses npm");
    record["superseded_by"] = json!(loremaster_core::new_ulid());
    push_ok(&service, &laptop.token, &[record]).await;
    let stored = memory(&service, &alice(), &id).await.unwrap();
    assert_eq!(string(&stored, "evict_key"), "1#1761750000000");
}

#[tokio::test]
async fn same_push_again_applies_nothing() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let records = [
        fact(
            &loremaster_core::new_ulid(),
            1_000,
            &laptop.device_id,
            "one",
        ),
        fact(
            &loremaster_core::new_ulid(),
            1_001,
            &laptop.device_id,
            "two",
        ),
    ];
    assert_eq!(push_ok(&service, &laptop.token, &records).await.len(), 2);
    assert!(push_ok(&service, &laptop.token, &records).await.is_empty());
    assert_eq!(counters(&service, &alice()).await, (2, 2));
}

#[tokio::test]
async fn older_hlc_is_ignored_and_newer_hlc_wins() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let id = loremaster_core::new_ulid();
    push_ok(
        &service,
        &laptop.token,
        &[fact(&id, 2_000, &laptop.device_id, "middle")],
    )
    .await;

    let older = push_ok(
        &service,
        &laptop.token,
        &[fact(&id, 1_000, &laptop.device_id, "old")],
    )
    .await;
    assert!(older.is_empty());
    let stored = memory(&service, &alice(), &id).await.unwrap();
    assert_eq!(
        (string(&stored, "text").as_str(), seq(&stored)),
        ("middle", 1)
    );

    let newer = push_ok(
        &service,
        &laptop.token,
        &[fact(&id, 3_000, &laptop.device_id, "new")],
    )
    .await;
    assert_eq!(newer, [id.as_str()]);
    let stored = memory(&service, &alice(), &id).await.unwrap();
    assert_eq!((string(&stored, "text").as_str(), seq(&stored)), ("new", 2));
    assert_eq!(counters(&service, &alice()).await, (2, 1));
}

#[tokio::test]
async fn tombstone_keeps_only_ids_and_lowers_live_count() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let id = loremaster_core::new_ulid();
    push_ok(
        &service,
        &laptop.token,
        &[fact(&id, 1_000, &laptop.device_id, "secret")],
    )
    .await;

    push_ok(
        &service,
        &laptop.token,
        &[tombstone(&id, 2_000, &laptop.device_id)],
    )
    .await;
    let stored = memory(&service, &alice(), &id).await.unwrap();
    let mut names: Vec<_> = stored.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["deleted", "hlc", "memory_id", "seq", "user_id", "writer"]
    );
    assert_eq!(stored["deleted"].as_bool().unwrap(), &true);
    assert_eq!(counters(&service, &alice()).await, (2, 0));

    let again = push_ok(
        &service,
        &laptop.token,
        &[tombstone(&id, 2_500, &laptop.device_id)],
    )
    .await;
    assert_eq!(again.len(), 1);
    assert_eq!(counters(&service, &alice()).await, (3, 0));

    push_ok(
        &service,
        &laptop.token,
        &[fact(&id, 3_000, &laptop.device_id, "back")],
    )
    .await;
    assert_eq!(counters(&service, &alice()).await, (4, 1));
}

#[tokio::test]
async fn users_do_not_share_memories() {
    let service = Service::start().await;
    let alice_laptop = device(&service, ALICE).await;
    let bob_laptop = device(&service, BOB).await;
    let id = loremaster_core::new_ulid();
    push_ok(
        &service,
        &alice_laptop.token,
        &[fact(&id, 1_000, &alice_laptop.device_id, "alice's")],
    )
    .await;
    push_ok(
        &service,
        &bob_laptop.token,
        &[fact(&id, 500, &bob_laptop.device_id, "bob's")],
    )
    .await;

    let bob = format!("gh:{BOB_ID}");
    let alice_item = memory(&service, &alice(), &id).await.unwrap();
    let bob_item = memory(&service, &bob, &id).await.unwrap();
    assert_eq!(string(&alice_item, "text"), "alice's");
    assert_eq!(string(&bob_item, "text"), "bob's");
    assert_eq!((seq(&alice_item), seq(&bob_item)), (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_pushes_keep_every_record() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let desktop = device(&service, ALICE).await;
    let batch = |writer: &str, base: u64| -> Vec<Value> {
        (0..5)
            .map(|n| fact(&loremaster_core::new_ulid(), base + n, writer, "x"))
            .collect()
    };
    let (a, b) = (
        batch(&laptop.device_id, 1_000),
        batch(&desktop.device_id, 2_000),
    );
    // Like a device with an outbox: push again after a 409 until the service takes it.
    let until_stored = |token: String, records: Vec<Value>| {
        let service = &service;
        async move {
            loop {
                let (status, body) = push(service, &token, &records).await;
                match status {
                    StatusCode::OK => break,
                    StatusCode::CONFLICT => assert_eq!(body["error"], "retry"),
                    other => panic!("{other}: {body}"),
                }
            }
        }
    };
    tokio::join!(
        until_stored(laptop.token.clone(), a),
        until_stored(desktop.token.clone(), b)
    );

    let items = service.scan(&service.memories_table).await;
    assert_eq!(items.len(), 10);
    let mut seqs: Vec<_> = items.iter().map(seq).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=10).collect::<Vec<_>>());
    assert_eq!(counters(&service, &alice()).await, (10, 10));
}

#[tokio::test]
async fn bad_pushes_write_nothing() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let other = device(&service, ALICE).await;
    let me = laptop.device_id.as_str();
    let id = loremaster_core::new_ulid();
    let good = || fact(&id, 1_000, me, "ok");
    let with = |field: &str, value: Value| {
        let mut record = good();
        record[field] = value;
        record
    };
    let without = |field: &str| {
        let mut record = good();
        record.as_object_mut().unwrap().remove(field);
        record
    };
    let too_many: Vec<Value> = (0..100)
        .map(|n| fact(&loremaster_core::new_ulid(), 1_000 + n, me, "x"))
        .collect();
    let mut hlc_of_other = good();
    hlc_of_other["hlc"] = json!(hlc(1_000, &other.device_id));
    let cases: Vec<(&str, Vec<Value>)> = vec![
        ("100 records", too_many),
        (
            "text over 2,000 chars",
            vec![with("text", json!("é".repeat(2_001)))],
        ),
        ("empty text", vec![with("text", json!(""))]),
        ("live record with no text", vec![without("text")]),
        ("live record with no type", vec![without("type")]),
        ("live record with no scope", vec![without("scope")]),
        (
            "live record with no created_at",
            vec![without("created_at")],
        ),
        ("fact with no kind", vec![without("kind")]),
        ("episode with a kind", vec![with("type", json!("episode"))]),
        (
            "project scope with no project_id",
            vec![with("scope", json!("project"))],
        ),
        (
            "user scope with a project_id",
            vec![with("project_id", json!("github.com/a/b"))],
        ),
        (
            "memory_id not a ULID",
            vec![with("memory_id", json!("not-a-ulid"))],
        ),
        (
            "hlc that does not parse",
            vec![with("hlc", json!("yesterday"))],
        ),
        (
            "writer of another device",
            vec![with("writer", json!(other.device_id))],
        ),
        ("hlc of another writer", vec![hlc_of_other]),
        (
            "same memory_id twice",
            vec![good(), fact(&id, 1_001, me, "again")],
        ),
    ];
    for (case, records) in cases {
        let (status, body) = push(&service, &laptop.token, &records).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {body}");
        assert!(body["error"].is_string(), "{case}: {body}");
        if case != "100 records" {
            let named = body["error"].as_str().unwrap();
            let expected = records[0]["memory_id"].as_str().unwrap();
            assert!(named.contains(expected), "{case}: {named}");
        }
    }
    let (status, _) = service.post_as(PUSH, &laptop.token, "not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    assert!(service.scan(&service.memories_table).await.is_empty());
    assert_eq!(counters(&service, &alice()).await, (0, 0));

    let longest = push_ok(
        &service,
        &laptop.token,
        &[with("text", json!("é".repeat(2_000)))],
    )
    .await;
    assert_eq!(longest, [id]);
}

#[tokio::test]
async fn service_sets_user_seq_and_evict_key() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let id = loremaster_core::new_ulid();
    let mut record = fact(&id, 1_000, &laptop.device_id, "mine");
    record["user_id"] = json!(format!("gh:{BOB_ID}"));
    record["seq"] = json!(99);
    record["evict_key"] = json!("x");
    push_ok(&service, &laptop.token, &[record]).await;
    let stored = memory(&service, &alice(), &id).await.unwrap();
    assert_eq!(seq(&stored), 1);
    assert_eq!(string(&stored, "evict_key"), "3#1761750000000");
    assert!(
        memory(&service, &format!("gh:{BOB_ID}"), &id)
            .await
            .is_none()
    );

    let gone = loremaster_core::new_ulid();
    let mut dead = tombstone(&gone, 2_000, &laptop.device_id);
    dead["text"] = json!("must not be stored");
    dead["kind"] = json!("preference");
    push_ok(&service, &laptop.token, &[dead]).await;
    let stored = memory(&service, &alice(), &gone).await.unwrap();
    assert!(!stored.contains_key("text") && !stored.contains_key("kind"));
    assert!(!stored.contains_key("evict_key"));
}

#[tokio::test]
async fn empty_push_and_storage_failure() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    assert!(push_ok(&service, &laptop.token, &[]).await.is_empty());
    assert_eq!(counters(&service, &alice()).await, (0, 0));

    let broken = Service::start().await;
    let token = device(&broken, ALICE).await;
    broken
        .dynamo
        .delete_table()
        .table_name(&broken.memories_table)
        .send()
        .await
        .unwrap();
    let records = [fact(
        &loremaster_core::new_ulid(),
        1_000,
        &token.device_id,
        "x",
    )];
    let (status, body) = push(&broken, &token.token, &records).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
}
