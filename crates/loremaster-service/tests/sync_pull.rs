//! `GET /v1/sync/pull` against DynamoDB Local.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{ALICE, BOB, BOB_ID, Service, alice, device, fact, hlc, push_ok, tombstone};
use loremaster_core::{PullResponse, Record};
use serde_json::{Value, json};

async fn pull(service: &Service, token: &str, cursor: i64) -> PullResponse {
    let (status, body) = service
        .get(&format!("/v1/sync/pull?cursor={cursor}"), token)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).unwrap()
}

/// A pushed record as the service stores it: with the user, its `seq` and its eviction key.
fn stored(pushed: &Value, user_id: &str, seq: i64) -> Record {
    let mut record = pushed.clone();
    record["user_id"] = json!(user_id);
    record["seq"] = json!(seq);
    record["evict_key"] = json!(format!("3#{}", pushed["created_at"]));
    serde_json::from_value(record).unwrap()
}

#[tokio::test]
async fn pull_from_zero_gives_records_in_seq_order() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let facts: Vec<Value> = ["Prefers pnpm", "Uses UTC", "Runs tests first"]
        .iter()
        .enumerate()
        .map(|(n, text)| {
            fact(
                &loremaster_core::new_ulid(),
                1_000 + n as u64,
                &laptop.device_id,
                text,
            )
        })
        .collect();
    push_ok(&service, &laptop.token, &facts).await;

    let page = pull(&service, &laptop.token, 0).await;
    let expected: Vec<Record> = (1..)
        .zip(&facts)
        .map(|(seq, pushed)| stored(pushed, &alice(), seq))
        .collect();
    assert_eq!(page.records, expected);
    assert_eq!((page.next_cursor, page.total, page.more), (3, 3, false));
}

#[tokio::test]
async fn pull_from_last_cursor_is_empty() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let records = [
        fact(&loremaster_core::new_ulid(), 1_000, &laptop.device_id, "a"),
        fact(&loremaster_core::new_ulid(), 1_001, &laptop.device_id, "b"),
    ];
    push_ok(&service, &laptop.token, &records).await;
    let first = pull(&service, &laptop.token, 0).await;

    let page = pull(&service, &laptop.token, first.next_cursor).await;
    assert!(page.records.is_empty());
    assert_eq!((page.next_cursor, page.total, page.more), (2, 2, false));
}

/// Pages from `cursor=0` until `more` is false.
async fn pull_all(service: &Service, token: &str) -> (Vec<PullResponse>, Vec<Record>) {
    let mut pages = Vec::new();
    let mut cursor = 0;
    loop {
        let page = pull(service, token, cursor).await;
        cursor = page.next_cursor;
        let more = page.more;
        pages.push(page);
        if !more {
            let records = pages.iter().flat_map(|page| page.records.clone()).collect();
            return (pages, records);
        }
    }
}

#[tokio::test]
async fn pages_over_one_megabyte_give_each_record_once() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let text = "x".repeat(2_000);
    let facts: Vec<Value> = (0..600)
        .map(|n| {
            fact(
                &loremaster_core::new_ulid(),
                1_000 + n,
                &laptop.device_id,
                &text,
            )
        })
        .collect();
    for batch in facts.chunks(99) {
        push_ok(&service, &laptop.token, batch).await;
    }

    let (pages, records) = pull_all(&service, &laptop.token).await;
    assert!(
        pages[0].more,
        "600 records of 2,000 characters fill more than one page"
    );
    assert!(pages.len() >= 2);
    let seqs: Vec<i64> = records.iter().map(|record| record.seq.unwrap()).collect();
    assert_eq!(seqs, (1..=600).collect::<Vec<_>>());
    let mut ids: Vec<&str> = records
        .iter()
        .map(|record| record.memory_id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 600);
    assert!(pages.iter().all(|page| page.total == 600));
}

#[tokio::test]
async fn late_push_after_cursor_is_pulled() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let desktop = device(&service, ALICE).await;
    let (old, other) = (loremaster_core::new_ulid(), loremaster_core::new_ulid());
    push_ok(
        &service,
        &laptop.token,
        &[
            fact(&old, 1_000, &laptop.device_id, "Uses npm"),
            fact(&other, 1_001, &laptop.device_id, "Uses UTC"),
        ],
    )
    .await;
    let (_, seen) = pull_all(&service, &laptop.token).await;
    let cursor = seen.last().unwrap().seq.unwrap();

    let new = loremaster_core::new_ulid();
    let late = [
        fact(&old, 5_000, &desktop.device_id, "Uses pnpm"),
        fact(&new, 5_001, &desktop.device_id, "Runs tests first"),
    ];
    push_ok(&service, &desktop.token, &late).await;

    let page = pull(&service, &laptop.token, cursor).await;
    let expected: Vec<Record> = (cursor + 1..)
        .zip(&late)
        .map(|(seq, pushed)| stored(pushed, &alice(), seq))
        .collect();
    assert_eq!(page.records, expected);
    assert_eq!((page.next_cursor, page.total, page.more), (4, 3, false));
}

#[tokio::test]
async fn tombstone_is_pulled_and_total_counts_live_records() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    let ids: Vec<String> = (0..3).map(|_| loremaster_core::new_ulid()).collect();
    let facts: Vec<Value> = ids
        .iter()
        .map(|id| fact(id, 1_000, &laptop.device_id, "x"))
        .collect();
    push_ok(&service, &laptop.token, &facts).await;
    push_ok(
        &service,
        &laptop.token,
        &[tombstone(&ids[1], 2_000, &laptop.device_id)],
    )
    .await;

    let page = pull(&service, &laptop.token, 0).await;
    let expected = Record {
        user_id: Some(alice()),
        memory_id: ids[1].clone(),
        seq: Some(4),
        hlc: hlc(2_000, &laptop.device_id),
        writer: laptop.device_id.clone(),
        record_type: None,
        kind: None,
        scope: None,
        project_id: None,
        text: None,
        created_at: None,
        superseded_by: None,
        deleted: true,
        evict_key: None,
    };
    assert_eq!(page.records.len(), 3);
    assert_eq!(page.records.last(), Some(&expected));
    assert_eq!((page.next_cursor, page.total), (4, 2));
}

#[tokio::test]
async fn bad_cursor_gives_400() {
    let service = Service::start().await;
    let laptop = device(&service, ALICE).await;
    for query in [
        "",
        "?cursor=",
        "?cursor=abc",
        "?cursor=-1",
        "?cursor=1.5",
        "?from=0",
    ] {
        let (status, body) = service
            .get(&format!("/v1/sync/pull{query}"), &laptop.token)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert!(body["error"].is_string(), "{query}: {body}");
    }
}

#[tokio::test]
async fn users_do_not_see_each_others_records() {
    let service = Service::start().await;
    let alice_laptop = device(&service, ALICE).await;
    let bob_laptop = device(&service, BOB).await;
    let alice_facts: Vec<Value> = (0..2)
        .map(|n| {
            fact(
                &loremaster_core::new_ulid(),
                1_000 + n,
                &alice_laptop.device_id,
                "alice",
            )
        })
        .collect();
    push_ok(&service, &alice_laptop.token, &alice_facts).await;
    let bob_fact = fact(
        &loremaster_core::new_ulid(),
        1_000,
        &bob_laptop.device_id,
        "bob",
    );
    push_ok(&service, &bob_laptop.token, std::slice::from_ref(&bob_fact)).await;

    let alice_page = pull(&service, &alice_laptop.token, 0).await;
    assert_eq!(alice_page.records.len(), 2);
    assert!(
        alice_page
            .records
            .iter()
            .all(|record| record.user_id == Some(alice()))
    );
    assert_eq!(alice_page.total, 2);
    let bob_page = pull(&service, &bob_laptop.token, 0).await;
    assert_eq!(
        bob_page.records,
        [stored(&bob_fact, &format!("gh:{BOB_ID}"), 1)]
    );
    assert_eq!(bob_page.total, 1);
}

#[tokio::test]
async fn pull_without_token_and_storage_failure() {
    let service = Service::start().await;
    let request = Request::get("/v1/sync/pull?cursor=0")
        .body(Body::empty())
        .unwrap();
    let (status, _) = service.send(request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = service.get("/v1/sync/pull?cursor=0", "not-a-token").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    for table in ["memories", "users"] {
        let broken = Service::start().await;
        let laptop = device(&broken, ALICE).await;
        let name = match table {
            "memories" => &broken.memories_table,
            _ => &broken.users_table,
        };
        broken
            .dynamo
            .delete_table()
            .table_name(name)
            .send()
            .await
            .unwrap();
        let (status, body) = broken.get("/v1/sync/pull?cursor=0", &laptop.token).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{table}: {body}");
        assert_eq!(body["error"], "internal error", "{table}");
    }
}
