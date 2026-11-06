mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use aws_sdk_dynamodb::types::AttributeValue;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use common::{ALICE, ALICE_ID, BOB, BOB_ID, Service, string};
use loremaster_service::{AppState, Config, purge_account, router};

fn alice() -> String {
    format!("gh:{ALICE_ID}")
}

fn bob() -> String {
    format!("gh:{BOB_ID}")
}

/// A state on the same tables as the service, to call the purge directly.
fn state(service: &Service) -> AppState {
    let config = Config {
        users_table: service.users_table.clone(),
        devices_table: service.devices_table.clone(),
        memories_table: service.memories_table.clone(),
        github_api_url: "http://127.0.0.1:9".to_owned(),
    };
    AppState::new(service.dynamo.clone(), config)
}

async fn register(service: &Service, github_token: &str, name: &str) -> String {
    let (status, body) = service.register(github_token, name).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["token"].as_str().unwrap().to_owned()
}

async fn seed_memories(service: &Service, user_id: &str, count: usize) {
    for seq in 1..=count {
        service
            .dynamo
            .put_item()
            .table_name(&service.memories_table)
            .item("user_id", AttributeValue::S(user_id.to_owned()))
            .item("memory_id", AttributeValue::S(loremaster_core::new_ulid()))
            .item("seq", AttributeValue::N(seq.to_string()))
            .item("text", AttributeValue::S(format!("memory {seq}")))
            .send()
            .await
            .unwrap();
    }
}

/// The user's items in one table, each with its attributes in name order, sorted.
async fn user_items(
    service: &Service,
    table: &str,
    user_id: &str,
) -> Vec<BTreeMap<String, AttributeValue>> {
    let mut items: Vec<BTreeMap<_, _>> = service
        .scan(table)
        .await
        .into_iter()
        .filter(|item| string(item, "user_id") == user_id)
        .map(|item| item.into_iter().collect())
        .collect();
    items.sort_by_key(|item| format!("{item:?}"));
    items
}

/// Alice with 2 devices and 30 memories, bob with 1 device and 2 memories.
async fn two_users(service: &Service) -> (String, String, String) {
    let alice_1 = register(service, ALICE, "laptop").await;
    let alice_2 = register(service, ALICE, "desktop").await;
    let bob_1 = register(service, BOB, "laptop").await;
    seed_memories(service, &alice(), 30).await;
    seed_memories(service, &bob(), 2).await;
    (alice_1, alice_2, bob_1)
}

async fn assert_no_rows(service: &Service, user_id: &str) {
    for table in [
        &service.users_table,
        &service.devices_table,
        &service.memories_table,
    ] {
        assert!(
            user_items(service, table, user_id).await.is_empty(),
            "{table} still has items of {user_id}"
        );
    }
}

#[tokio::test]
async fn purge_deletes_only_this_users_rows() {
    let service = Service::start().await;
    two_users(&service).await;
    let mut bob_before = Vec::new();
    for table in [
        &service.users_table,
        &service.devices_table,
        &service.memories_table,
    ] {
        bob_before.push(user_items(&service, table, &bob()).await);
    }
    assert_eq!(bob_before[2].len(), 2);

    purge_account(&state(&service), &alice()).await.unwrap();

    assert_no_rows(&service, &alice()).await;
    let mut bob_after = Vec::new();
    for table in [
        &service.users_table,
        &service.devices_table,
        &service.memories_table,
    ] {
        bob_after.push(user_items(&service, table, &bob()).await);
    }
    assert_eq!(bob_after, bob_before);
}

#[tokio::test]
async fn purge_twice_gives_no_error() {
    let service = Service::start().await;
    two_users(&service).await;
    let state = state(&service);

    purge_account(&state, &alice()).await.unwrap();
    purge_account(&state, &alice()).await.unwrap();
    purge_account(&state, "gh:404").await.unwrap();

    assert_no_rows(&service, &alice()).await;
}

#[tokio::test]
async fn purge_with_storage_failure_is_an_error() {
    let service = Service::without_tables().await;
    assert!(purge_account(&state(&service), &alice()).await.is_err());
}

#[tokio::test]
async fn delete_account_rejects_every_token() {
    let service = Service::start().await;
    let (alice_1, alice_2, bob_1) = two_users(&service).await;

    let (status, body) = service.delete("/v1/account", &alice_1).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body, serde_json::Value::Null);

    for token in [&alice_1, &alice_2] {
        let (status, _) = service.get("/v1/devices", token).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, body) = service.get("/v1/devices", &bob_1).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["devices"].as_array().unwrap().len(), 1);
    // With no function to invoke, the purge runs before the reply.
    assert_no_rows(&service, &alice()).await;
    assert_eq!(
        user_items(&service, &service.memories_table, &bob())
            .await
            .len(),
        2
    );
}

#[tokio::test]
async fn delete_account_without_token_and_storage_failure() {
    let service = Service::start().await;
    let (status, _) = service.delete("/v1/account", "unknown").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let service = Service::without_tables().await;
    let (status, _) = service.delete("/v1/account", "unknown").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

async fn mark_deleting(service: &Service, user_id: &str) {
    service
        .dynamo
        .update_item()
        .table_name(&service.users_table)
        .key("user_id", AttributeValue::S(user_id.to_owned()))
        .update_expression("SET deleting = :true")
        .expression_attribute_values(":true", AttributeValue::Bool(true))
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn register_while_deleting_is_409() {
    let service = Service::start().await;
    let (alice_1, _, _) = two_users(&service).await;
    let (_, list) = service.get("/v1/devices", &alice_1).await;
    let old_device_id = list["devices"][0]["device_id"].as_str().unwrap().to_owned();
    mark_deleting(&service, &alice()).await;
    let devices_before = user_items(&service, &service.devices_table, &alice()).await;

    let new = serde_json::json!({"github_token": ALICE, "name": "new"});
    let replace = serde_json::json!({
        "github_token": ALICE,
        "name": "new",
        "replace_device_id": old_device_id,
    });
    for body in [new, replace] {
        let (status, reply) = service.post("/v1/devices", body.to_string()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(
            reply,
            serde_json::json!({"error": "account deletion in progress"})
        );
    }
    assert_eq!(
        user_items(&service, &service.devices_table, &alice()).await,
        devices_before
    );

    purge_account(&state(&service), &alice()).await.unwrap();
    register(&service, ALICE, "laptop").await;
    let users = user_items(&service, &service.users_table, &alice()).await;
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["device_count"], AttributeValue::N("1".to_owned()));
    assert!(!users[0].contains_key("deleting"));
}

/// One call that the fake Lambda API got: function name, invocation type, payload.
type Invocation = (String, String, serde_json::Value);

/// A Lambda API that records each invocation and replies with `status`.
async fn fake_lambda(status: StatusCode) -> (aws_sdk_lambda::Client, Arc<Mutex<Vec<Invocation>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let invoke =
        async |State((calls, status)): State<(Arc<Mutex<Vec<Invocation>>>, StatusCode)>,
               Path(name): Path<String>,
               headers: HeaderMap,
               body: String| {
            let invocation_type = headers["x-amz-invocation-type"]
                .to_str()
                .unwrap()
                .to_owned();
            let payload = serde_json::from_str(&body).unwrap();
            calls.lock().unwrap().push((name, invocation_type, payload));
            (status, r#"{"message":"fake"}"#)
        };
    let app = Router::new()
        .route("/2015-03-31/functions/{name}/invocations", post(invoke))
        .with_state((calls.clone(), status));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = aws_sdk_lambda::Config::builder()
        .behavior_version(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-2"))
        .credentials_provider(aws_sdk_lambda::config::Credentials::for_tests())
        .endpoint_url(url)
        .build();
    (aws_sdk_lambda::Client::from_conf(config), calls)
}

/// The same service, but account purges go to this Lambda client.
fn with_purger(service: Service, lambda: aws_sdk_lambda::Client) -> Service {
    let state = state(&service).with_purger(lambda, "loremaster-dev-service".to_owned());
    Service {
        router: router(state),
        ..service
    }
}

#[tokio::test]
async fn delete_account_starts_an_async_purge_of_this_user() {
    let (lambda, calls) = fake_lambda(StatusCode::ACCEPTED).await;
    let service = Service::start().await;
    let (alice_1, alice_2, _) = two_users(&service).await;
    let service = with_purger(service, lambda);

    let (status, _) = service.delete("/v1/account", &alice_1).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let expected = (
        "loremaster-dev-service".to_owned(),
        "Event".to_owned(),
        serde_json::json!({"purge_user_id": alice()}),
    );
    assert_eq!(*calls.lock().unwrap(), vec![expected]);
    for token in [&alice_1, &alice_2] {
        let (status, _) = service.get("/v1/devices", token).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    // The purge runs later, in its own invocation.
    let users = user_items(&service, &service.users_table, &alice()).await;
    assert_eq!(users[0]["deleting"], AttributeValue::Bool(true));
    assert_eq!(
        user_items(&service, &service.memories_table, &alice())
            .await
            .len(),
        30
    );
}

#[tokio::test]
async fn failed_purge_start_is_500_and_keeps_the_marker() {
    let (lambda, calls) = fake_lambda(StatusCode::BAD_REQUEST).await;
    let service = Service::start().await;
    let (alice_1, _, _) = two_users(&service).await;
    let service = with_purger(service, lambda);

    let (status, _) = service.delete("/v1/account", &alice_1).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(
        user_items(&service, &service.devices_table, &alice())
            .await
            .is_empty()
    );
    let users = user_items(&service, &service.users_table, &alice()).await;
    assert_eq!(users[0]["deleting"], AttributeValue::Bool(true));
}

#[tokio::test]
async fn push_while_deleting_is_409_and_writes_nothing() {
    let service = Service::start().await;
    let (status, credential) = service.register(ALICE, "laptop").await;
    assert_eq!(status, StatusCode::CREATED, "{credential}");
    let token = credential["token"].as_str().unwrap();
    let writer = credential["device_id"].as_str().unwrap();
    mark_deleting(&service, &alice()).await;

    let push = serde_json::json!({"records": [{
        "memory_id": loremaster_core::new_ulid(),
        "hlc": format!("1761750000000-00000-{writer}"),
        "writer": writer,
        "type": "fact",
        "kind": "preference",
        "scope": "user",
        "text": "Prefers pnpm over npm",
        "created_at": 1_761_750_000_000_u64,
    }]});
    let (status, body) = service
        .post_as("/v1/sync/push", token, push.to_string())
        .await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        user_items(&service, &service.memories_table, &alice())
            .await
            .is_empty()
    );
}
