//! A service wired to DynamoDB Local, a fake GitHub and a fake Messages API, one set of tables
//! per test.
// Each test binary uses only some of these helpers.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, GlobalSecondaryIndex, KeySchemaElement,
    KeyType, LocalSecondaryIndex, Projection, ProjectionType, ScalarAttributeType,
};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::{get, post};
use http_body_util::BodyExt;
use loremaster_service::{AppState, Config, model_client, router};
use tower::ServiceExt;

pub const ALICE: &str = "gho_alice";
pub const ALICE_ID: u64 = 1001;
pub const BOB: &str = "gho_bob";
pub const BOB_ID: u64 = 1002;
pub const BROKEN: &str = "gho_broken";
pub const SLOW: &str = "gho_slow";

pub type Item = HashMap<String, AttributeValue>;

/// Input tokens that the fake Messages API reports for every call.
pub const FAKE_INPUT_TOKENS: u64 = 1_200;
/// A turn with this prefix makes the fake record the JSON after it as the tool input.
pub const REPLY: &str = "reply:";
/// Turns with these markers make the fake fail in one way.
pub const NO_TOOL: &str = "[no-tool]";
pub const RATE_LIMITED: &str = "[rate-limited]";
pub const OVERLOADED: &str = "[overloaded]";
pub const FAIL: &str = "[fail]";
pub const GARBAGE: &str = "[garbage]";
pub const SLOW_MODEL: &str = "[slow]";

pub struct Service {
    pub router: Router,
    pub dynamo: Client,
    pub users_table: String,
    pub devices_table: String,
    pub memories_table: String,
    pub model: Arc<FakeModel>,
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
        let model = Arc::new(FakeModel::default());
        let config = Config {
            users_table: users_table.clone(),
            devices_table: devices_table.clone(),
            memories_table: memories_table.clone(),
            github_api_url: fake_github().await,
            anthropic_api_url: fake_model(model.clone()).await,
            model_id: MODEL_ID.to_owned(),
        };
        // A short timeout, so the slow fake shows at once.
        let model_client = model_client(MODEL_KEY, Duration::from_secs(1));
        Self {
            router: router(AppState::new(dynamo.clone(), model_client, config)),
            dynamo,
            users_table,
            devices_table,
            memories_table,
            model,
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

pub const MODEL_ID: &str = "test-model";
/// The API key that the service under test sends, and the only one the fake accepts.
pub const MODEL_KEY: &str = "sk-ant-test";

/// What the fake Messages API saw: the number of calls and the last request.
#[derive(Default)]
pub struct FakeModel {
    requests: Mutex<Vec<(HeaderMap, serde_json::Value)>>,
}

impl FakeModel {
    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// The headers and JSON body of the last Messages request.
    pub fn last_request(&self) -> (HeaderMap, serde_json::Value) {
        self.requests.lock().unwrap().last().cloned().unwrap()
    }
}

/// Serves `POST /v1/messages`. The turn picks the reply: a prefix or a marker from above, or
/// else one preference fact.
async fn fake_model(seen: Arc<FakeModel>) -> String {
    async fn messages(
        State(seen): State<Arc<FakeModel>>,
        headers: HeaderMap,
        axum::Json(request): axum::Json<serde_json::Value>,
    ) -> (StatusCode, String) {
        let turn = request["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let authorized = headers.get("x-api-key").is_some_and(|key| key == MODEL_KEY);
        let model = request["model"].clone();
        seen.requests.lock().unwrap().push((headers, request));
        let error = |status: u16, error_type: &str| {
            let body = serde_json::json!({
                "type": "error",
                "error": {"type": error_type, "message": "fake failure"},
            });
            (StatusCode::from_u16(status).unwrap(), body.to_string())
        };
        if !authorized {
            return error(401, "authentication_error");
        }
        if turn.contains(RATE_LIMITED) {
            return error(429, "rate_limit_error");
        }
        if turn.contains(OVERLOADED) {
            return error(529, "overloaded_error");
        }
        if turn.contains(FAIL) {
            return error(500, "api_error");
        }
        if turn.contains(GARBAGE) {
            return (StatusCode::OK, "not json".to_owned());
        }
        if turn.contains(SLOW_MODEL) {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let tool_use = |input| serde_json::json!({"type": "tool_use", "id": "toolu_1", "name": "record_candidates", "input": input});
        let (content, stop_reason) = if turn.contains(NO_TOOL) {
            (serde_json::json!([]), "refusal")
        } else {
            let input = match turn.strip_prefix(REPLY) {
                Some(json) => serde_json::from_str(json).unwrap(),
                None => serde_json::json!({"candidates": [
                    {"text": "Prefers pnpm over npm", "scope": "user", "kind": "preference"}
                ]}),
            };
            // A block type the service does not know comes first, as new API features can add.
            let content = serde_json::json!([{"type": "unknown_block"}, tool_use(input)]);
            (content, "tool_use")
        };
        let body = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": content,
            "stop_reason": stop_reason,
            "usage": {"input_tokens": FAKE_INPUT_TOKENS, "output_tokens": 40},
        });
        (StatusCode::OK, body.to_string())
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/messages", post(messages))
        .with_state(seen);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}
