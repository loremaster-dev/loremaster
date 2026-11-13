//! The service keeps each user at the live memory cap by tombstoning the oldest memories.

mod common;

use aws_sdk_dynamodb::types::{DeleteGlobalSecondaryIndexAction, GlobalSecondaryIndexUpdate};
use axum::http::StatusCode;
use common::{
    ALICE, Item, Service, alice, counters, device, fact, hlc, memory, push, push_ok, string,
};
use loremaster_core::{Hlc, PullResponse};
use serde_json::{Value, json};

/// A real `created_at`: the service orders memories by its fixed-width text.
const EPOCH_MS: i64 = 1_761_750_000_000;

/// A live fact made `created_at` ms after `EPOCH_MS`.
fn other_fact(writer: &str, created_at: i64) -> Value {
    let mut record = fact(&loremaster_core::new_ulid(), 1_000, writer, "fact");
    record["created_at"] = json!(EPOCH_MS + created_at);
    record
}

/// A fact that a newer fact replaced.
fn replaced_fact(writer: &str, created_at: i64) -> Value {
    let mut record = other_fact(writer, created_at);
    record["superseded_by"] = json!(loremaster_core::new_ulid());
    record
}

fn episode(writer: &str, created_at: i64) -> Value {
    json!({
        "memory_id": loremaster_core::new_ulid(),
        "hlc": hlc(1_000, writer),
        "writer": writer,
        "type": "episode",
        "scope": "user",
        "text": "episode",
        "created_at": EPOCH_MS + created_at,
    })
}

fn id(record: &Value) -> &str {
    record["memory_id"].as_str().unwrap()
}

fn is_tombstone(item: &Item) -> bool {
    item.get("deleted")
        .is_some_and(|deleted| deleted.as_bool() == Ok(&true))
}

async fn stored(service: &Service, record: &Value) -> Item {
    memory(service, &alice(), id(record)).await.unwrap()
}

/// The ids of the user's tombstones.
async fn tombstones(service: &Service) -> Vec<String> {
    let mut ids: Vec<String> = service
        .scan(&service.memories_table)
        .await
        .iter()
        .filter(|item| is_tombstone(item))
        .map(|item| string(item, "memory_id"))
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn a_push_over_the_cap_evicts_the_oldest_replaced_fact_first() {
    let service = Service::start_with_cap(5).await;
    let laptop = device(&service, ALICE).await;
    let me = laptop.device_id.as_str();
    let replaced = replaced_fact(me, 900);
    let records = [
        other_fact(me, 100),
        other_fact(me, 200),
        episode(me, 300),
        replaced.clone(),
        replaced_fact(me, 950),
    ];
    push_ok(&service, &laptop.token, &records).await;
    push_ok(&service, &laptop.token, &[other_fact(me, 1_000)]).await;

    let evicted = stored(&service, &replaced).await;
    assert!(is_tombstone(&evicted));
    assert_eq!(string(&evicted, "writer"), "server");
    assert!(!evicted.contains_key("text") && !evicted.contains_key("evict_key"));
    assert_eq!(evicted["seq"].as_n().unwrap(), "7");
    assert_eq!(tombstones(&service).await, [id(&replaced)]);
    assert_eq!(counters(&service, &alice()).await, (7, 5));
}

#[tokio::test]
async fn with_no_replaced_facts_the_oldest_episode_goes_and_then_the_oldest_other_fact() {
    let service = Service::start_with_cap(5).await;
    let laptop = device(&service, ALICE).await;
    let me = laptop.device_id.as_str();
    let (late_episode, early_episode, oldest_fact) =
        (episode(me, 300), episode(me, 200), other_fact(me, 10));
    let records = [
        late_episode.clone(),
        early_episode.clone(),
        other_fact(me, 30),
        oldest_fact.clone(),
        other_fact(me, 20),
    ];
    push_ok(&service, &laptop.token, &records).await;

    let mut expected = Vec::new();
    for gone in [&early_episode, &late_episode, &oldest_fact] {
        push_ok(&service, &laptop.token, &[other_fact(me, 5_000)]).await;
        expected.push(id(gone).to_owned());
        expected.sort();
        assert_eq!(tombstones(&service).await, expected);
    }
    assert_eq!(counters(&service, &alice()).await.1, 5);
}

#[tokio::test]
async fn nothing_is_evicted_at_the_cap_and_only_the_excess_above_it() {
    let service = Service::start_with_cap(5).await;
    let laptop = device(&service, ALICE).await;
    let me = laptop.device_id.as_str();
    let first: Vec<Value> = (1..=4).map(|n| other_fact(me, n)).collect();
    push_ok(&service, &laptop.token, &first).await;
    push_ok(&service, &laptop.token, &[other_fact(me, 5)]).await;
    assert!(tombstones(&service).await.is_empty());
    assert_eq!(counters(&service, &alice()).await, (5, 5));

    let more: Vec<Value> = (6..=8).map(|n| other_fact(me, n)).collect();
    push_ok(&service, &laptop.token, &more).await;
    let mut oldest: Vec<String> = first[..3].iter().map(|r| id(r).to_owned()).collect();
    oldest.sort();
    assert_eq!(tombstones(&service).await, oldest);
    assert_eq!(counters(&service, &alice()).await, (11, 5));
}

#[tokio::test]
async fn an_eviction_sorts_after_a_stamp_from_the_future_and_reaches_a_pull() {
    let service = Service::start_with_cap(5).await;
    let laptop = device(&service, ALICE).await;
    let me = laptop.device_id.as_str();
    let mut ahead = other_fact(me, 1);
    let future = hlc(9_999_999_999_000, me);
    ahead["hlc"] = json!(future);
    let mut records: Vec<Value> = (2..=5).map(|n| other_fact(me, n)).collect();
    records.push(ahead.clone());
    push_ok(&service, &laptop.token, &records).await;
    let newest = other_fact(me, 6);
    push_ok(&service, &laptop.token, std::slice::from_ref(&newest)).await;

    let (status, body) = service.get("/v1/sync/pull?cursor=5", &laptop.token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page: PullResponse = serde_json::from_value(body).unwrap();
    let pulled: Vec<(&str, i64)> = page
        .records
        .iter()
        .map(|record| (record.memory_id.as_str(), record.seq.unwrap()))
        .collect();
    assert_eq!(pulled, [(id(&newest), 6), (id(&ahead), 7)]);
    let eviction = &page.records[1];
    assert!(eviction.deleted);
    assert_eq!(eviction.writer, "server");
    assert_eq!(Hlc::parse(&eviction.hlc).unwrap().writer(), "server");
    assert!(
        eviction.hlc > future,
        "{} is not after {future}",
        eviction.hlc
    );
    assert_eq!(page.total, 5);
}

#[tokio::test]
async fn a_failed_eviction_keeps_the_push() {
    let service = Service::start_with_cap(5).await;
    let laptop = device(&service, ALICE).await;
    let me = laptop.device_id.as_str();
    service
        .dynamo
        .update_table()
        .table_name(&service.memories_table)
        .global_secondary_index_updates(
            GlobalSecondaryIndexUpdate::builder()
                .delete(
                    DeleteGlobalSecondaryIndexAction::builder()
                        .index_name("by_evict_key")
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    let records: Vec<Value> = (1..=6).map(|n| other_fact(me, n)).collect();
    let applied = push_ok(&service, &laptop.token, &records).await;
    assert_eq!(applied.len(), 6);
    assert!(tombstones(&service).await.is_empty());
    assert_eq!(counters(&service, &alice()).await, (6, 6));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_pushes_over_the_cap_end_at_the_cap() {
    let service = Service::start_with_cap(5).await;
    let laptop = device(&service, ALICE).await;
    let desktop = device(&service, ALICE).await;
    let batch = |writer: &str, first: i64| -> Vec<Value> {
        (first..first + 5).map(|n| other_fact(writer, n)).collect()
    };
    push_ok(&service, &laptop.token, &batch(&laptop.device_id, 1)).await;
    let (a, b) = (batch(&laptop.device_id, 6), batch(&desktop.device_id, 11));
    let newest: Vec<String> = b.iter().map(|record| id(record).to_owned()).collect();
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

    assert_eq!(counters(&service, &alice()).await, (25, 5));
    let mut live: Vec<String> = service
        .scan(&service.memories_table)
        .await
        .iter()
        .filter(|item| !is_tombstone(item))
        .map(|item| string(item, "memory_id"))
        .collect();
    live.sort();
    let mut newest = newest;
    newest.sort();
    assert_eq!(live, newest);
}

#[tokio::test]
async fn a_backlog_over_the_cap_shrinks_by_a_full_batch_with_each_push() {
    let mut service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let me = laptop.device_id.as_str();
    let backlog: Vec<Value> = (1..=110).map(|n| other_fact(me, n)).collect();
    for chunk in backlog.chunks(99) {
        push_ok(&service, &laptop.token, chunk).await;
    }
    service.set_cap(5);

    push_ok(&service, &laptop.token, &[other_fact(me, 1_000)]).await;
    assert_eq!(tombstones(&service).await.len(), 99);
    assert_eq!(counters(&service, &alice()).await, (210, 12));
    push_ok(&service, &laptop.token, &[other_fact(me, 1_001)]).await;
    assert_eq!(counters(&service, &alice()).await, (219, 5));
    let oldest: Vec<String> = backlog[..107].iter().map(|r| id(r).to_owned()).collect();
    let mut oldest = oldest;
    oldest.sort();
    assert_eq!(tombstones(&service).await, oldest);
}
