//! A service wired to DynamoDB Local and a fake GitHub, one set of tables per test.
// Each test binary uses only some of these helpers.
#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Duration;

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, GlobalSecondaryIndex, KeySchemaElement,
    KeyType, LocalSecondaryIndex, Projection, ProjectionType, ScalarAttributeType,
};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::get;
use http_body_util::BodyExt;
use loremaster_core::{DeviceCredential, PushResponse};
use loremaster_service::{AppState, Config, router};
use serde_json::{Value, json};
use tower::ServiceExt;

pub const ALICE: &str = "gho_alice";
pub const ALICE_ID: u64 = 1001;
pub const BOB: &str = "gho_bob";
pub const BOB_ID: u64 = 1002;
pub const BROKEN: &str = "gho_broken";
pub const SLOW: &str = "gho_slow";

pub type Item = HashMap<String, AttributeValue>;

pub struct Service {
    pub router: Router,
    pub dynamo: Client,
    pub users_table: String,
    pub devices_table: String,
    pub memories_table: String,
}

impl Service {
    pub async fn start() -> Self {
        Self::start_with_tables(true).await
    }

    /// A service whose tables were never created, so every write fails.
    pub async fn without_tables() -> Self {
        Self::start_with_tables(false).await
    }

    async fn start_with_tables(create: bool) -> Self {
        let endpoint = std::env::var("DYNAMODB_ENDPOINT")
            .unwrap_or_else(|_| "http://localhost:8000".to_owned());
        let dynamo = local_dynamo(&endpoint).await;
        let suffix = loremaster_core::new_ulid();
        let users_table = format!("users_{suffix}");
        let devices_table = format!("devices_{suffix}");
        let memories_table = format!("memories_{suffix}");
        if create {
            create_tables(&dynamo, &users_table, &devices_table, &memories_table).await;
        }
        let config = Config {
            users_table: users_table.clone(),
            devices_table: devices_table.clone(),
            memories_table: memories_table.clone(),
            github_api_url: fake_github().await,
        };
        Self {
            router: router(AppState::new(dynamo.clone(), config)),
            dynamo,
            users_table,
            devices_table,
            memories_table,
        }
    }

    pub async fn post(&self, path: &str, body: impl Into<Body>) -> (StatusCode, serde_json::Value) {
        let request = Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.into())
            .unwrap();
        self.send(request).await
    }

    pub async fn register(
        &self,
        github_token: &str,
        name: &str,
    ) -> (StatusCode, serde_json::Value) {
        let body = serde_json::json!({"github_token": github_token, "name": name});
        self.post("/v1/devices", body.to_string()).await
    }

    pub async fn post_as(
        &self,
        path: &str,
        token: &str,
        body: impl Into<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(body.into())
            .unwrap();
        self.send(request).await
    }

    pub async fn get(&self, path: &str, token: &str) -> (StatusCode, serde_json::Value) {
        self.send(with_bearer(Request::get(path), token)).await
    }

    pub async fn delete(&self, path: &str, token: &str) -> (StatusCode, serde_json::Value) {
        self.send(with_bearer(Request::delete(path), token)).await
    }

    /// The reply status and JSON body. An empty body is `Null`.
    pub async fn send(&self, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        if bytes.is_empty() {
            return (status, serde_json::Value::Null);
        }
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("reply is not JSON: {}", String::from_utf8_lossy(&bytes)));
        (status, body)
    }

    pub async fn scan(&self, table: &str) -> Vec<Item> {
        let output = self.dynamo.scan().table_name(table).send().await.unwrap();
        output.items.unwrap_or_default()
    }
}

fn with_bearer(request: axum::http::request::Builder, token: &str) -> Request<Body> {
    request
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

async fn local_dynamo(endpoint: &str) -> Client {
    let config = aws_sdk_dynamodb::Config::builder()
        .behavior_version(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-2"))
        .credentials_provider(aws_sdk_dynamodb::config::Credentials::for_tests())
        .endpoint_url(endpoint)
        .build();
    let client = Client::from_conf(config);
    if let Err(error) = client.list_tables().limit(1).send().await {
        panic!(
            "DynamoDB Local is not reachable at {endpoint} ({error}). Start it with infra/local.sh."
        );
    }
    client
}

async fn create_tables(dynamo: &Client, users: &str, devices: &str, memories: &str) {
    let string = |name: &str| {
        AttributeDefinition::builder()
            .attribute_name(name)
            .attribute_type(ScalarAttributeType::S)
            .build()
            .unwrap()
    };
    let hash_key = |name: &str| {
        KeySchemaElement::builder()
            .attribute_name(name)
            .key_type(KeyType::Hash)
            .build()
            .unwrap()
    };
    dynamo
        .create_table()
        .table_name(users)
        .billing_mode(BillingMode::PayPerRequest)
        .attribute_definitions(string("user_id"))
        .key_schema(hash_key("user_id"))
        .send()
        .await
        .unwrap();
    let by_user = GlobalSecondaryIndex::builder()
        .index_name("by_user")
        .key_schema(hash_key("user_id"))
        .projection(
            Projection::builder()
                .projection_type(ProjectionType::All)
                .build(),
        )
        .build()
        .unwrap();
    dynamo
        .create_table()
        .table_name(devices)
        .billing_mode(BillingMode::PayPerRequest)
        .attribute_definitions(string("token_hash"))
        .attribute_definitions(string("user_id"))
        .key_schema(hash_key("token_hash"))
        .global_secondary_indexes(by_user)
        .send()
        .await
        .unwrap();
    let by_seq = LocalSecondaryIndex::builder()
        .index_name("by_seq")
        .key_schema(hash_key("user_id"))
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("seq")
                .key_type(KeyType::Range)
                .build()
                .unwrap(),
        )
        .projection(
            Projection::builder()
                .projection_type(ProjectionType::All)
                .build(),
        )
        .build()
        .unwrap();
    dynamo
        .create_table()
        .table_name(memories)
        .billing_mode(BillingMode::PayPerRequest)
        .attribute_definitions(string("user_id"))
        .attribute_definitions(string("memory_id"))
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("seq")
                .attribute_type(ScalarAttributeType::N)
                .build()
                .unwrap(),
        )
        .key_schema(hash_key("user_id"))
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("memory_id")
                .key_type(KeyType::Range)
                .build()
                .unwrap(),
        )
        .local_secondary_indexes(by_seq)
        .send()
        .await
        .unwrap();
}

/// Serves `GET /user` like GitHub: the account for a known token, 401 for any other.
async fn fake_github() -> String {
    async fn user(headers: HeaderMap) -> (StatusCode, String) {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        match token {
            Some(ALICE) => (
                StatusCode::OK,
                format!(r#"{{"id":{ALICE_ID},"login":"alice"}}"#),
            ),
            Some(BOB) => (
                StatusCode::OK,
                format!(r#"{{"id":{BOB_ID},"login":"bob"}}"#),
            ),
            Some(BROKEN) => (StatusCode::INTERNAL_SERVER_ERROR, String::new()),
            Some(SLOW) => {
                tokio::time::sleep(Duration::from_secs(6)).await;
                (StatusCode::OK, r#"{"id":1003,"login":"slow"}"#.to_owned())
            }
            _ => (
                StatusCode::UNAUTHORIZED,
                r#"{"message":"Bad credentials"}"#.to_owned(),
            ),
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/user", get(user)))
            .await
            .unwrap();
    });
    url
}

pub fn string(item: &Item, name: &str) -> String {
    item[name].as_s().unwrap().clone()
}

pub const PUSH: &str = "/v1/sync/push";

pub fn alice() -> String {
    format!("gh:{ALICE_ID}")
}

pub async fn device(service: &Service, github_token: &str) -> DeviceCredential {
    let (status, body) = service.register(github_token, "laptop").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    serde_json::from_value(body).unwrap()
}

pub fn hlc(wall_ms: u64, writer: &str) -> String {
    format!("{wall_ms:013}-00000-{writer}")
}

pub fn fact(memory_id: &str, wall_ms: u64, writer: &str, text: &str) -> Value {
    json!({
        "memory_id": memory_id,
        "hlc": hlc(wall_ms, writer),
        "writer": writer,
        "type": "fact",
        "kind": "preference",
        "scope": "user",
        "text": text,
        "created_at": 1_761_750_000_000_u64,
    })
}

pub fn tombstone(memory_id: &str, wall_ms: u64, writer: &str) -> Value {
    json!({"memory_id": memory_id, "hlc": hlc(wall_ms, writer), "writer": writer, "deleted": true})
}

pub async fn push(service: &Service, token: &str, records: &[Value]) -> (StatusCode, Value) {
    let body = json!({ "records": records });
    service.post_as(PUSH, token, body.to_string()).await
}

pub async fn push_ok(service: &Service, token: &str, records: &[Value]) -> Vec<String> {
    let (status, body) = push(service, token, records).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value::<PushResponse>(body)
        .unwrap()
        .applied
}
