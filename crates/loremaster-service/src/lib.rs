//! The sync service: one `axum` router that runs in a Lambda behind an HTTP API.

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::error::DisplayErrorContext;
use aws_sdk_dynamodb::operation::transact_get_items::TransactGetItemsError;
use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError;
use aws_sdk_dynamodb::types::{
    AttributeValue, CancellationReason, ConditionCheck, Delete, DeleteRequest, Get,
    KeysAndAttributes, Put, TransactGetItem, TransactWriteItem, Update, WriteRequest,
};
use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::InvocationType;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use lambda_http::request::LambdaRequest;
use loremaster_core::{
    Device, DeviceCredential, DeviceLimitReached, DeviceList, Hlc, PullResponse, PushRequest,
    PushResponse, Record, RecordType, RegisterDevice, Scope,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_DEVICE_NAME_CHARS: usize = 64;
const GITHUB_TIMEOUT: Duration = Duration::from_secs(5);
const DEVICE_LIMIT: u32 = 10;
const TRANSACTION_TRIES: u32 = 3;
const MAX_PUSH_RECORDS: usize = 99;
const MAX_TEXT_CHARS: usize = 2_000;
const BATCH_WRITE_ITEMS: usize = 25;
const UNPROCESSED_BACKOFF: Duration = Duration::from_millis(50);
const MAX_UNPROCESSED_BACKOFF: Duration = Duration::from_secs(2);

/// Where the service keeps its data and checks GitHub accounts.
pub struct Config {
    pub users_table: String,
    pub devices_table: String,
    pub memories_table: String,
    pub github_api_url: String,
}

/// A required environment variable is not set.
#[derive(Debug, Error)]
#[error("environment variable {0} is not set")]
pub struct MissingEnv(&'static str);

impl Config {
    /// Reads `USERS_TABLE`, `DEVICES_TABLE`, `MEMORIES_TABLE` and `GITHUB_API_URL` (default
    /// `https://api.github.com`).
    pub fn from_env() -> Result<Self, MissingEnv> {
        let required = |name| std::env::var(name).map_err(|_| MissingEnv(name));
        Ok(Self {
            users_table: required("USERS_TABLE")?,
            devices_table: required("DEVICES_TABLE")?,
            memories_table: required("MEMORIES_TABLE")?,
            github_api_url: std::env::var("GITHUB_API_URL")
                .unwrap_or_else(|_| "https://api.github.com".to_owned()),
        })
    }
}

/// Clients and config shared by every request.
#[derive(Clone)]
pub struct AppState {
    dynamo: Client,
    github: reqwest::Client,
    config: Arc<Config>,
    purger: Option<Purger>,
}

/// The Lambda function that runs account purges in the background: this one.
#[derive(Clone)]
struct Purger {
    lambda: aws_sdk_lambda::Client,
    function_name: String,
}

impl AppState {
    pub fn new(dynamo: Client, config: Config) -> Self {
        let github = reqwest::Client::builder()
            .timeout(GITHUB_TIMEOUT)
            .user_agent("loremaster-service")
            .build()
            .expect("TLS backend initializes");
        Self {
            dynamo,
            github,
            config: Arc::new(config),
            purger: None,
        }
    }

    /// Runs account purges as async invocations of the Lambda function `function_name`. Without
    /// this, `DELETE /v1/account` purges before it replies.
    pub fn with_purger(self, lambda: aws_sdk_lambda::Client, function_name: String) -> Self {
        Self {
            purger: Some(Purger {
                lambda,
                function_name,
            }),
            ..self
        }
    }
}

pub fn router(state: AppState) -> Router {
    let authenticated = Router::new()
        .route("/v1/devices", get(list_devices))
        .route("/v1/devices/{id}", delete(delete_device))
        .route("/v1/sync/push", post(push_records))
        .route("/v1/sync/pull", get(pull_records))
        .route("/v1/account", delete(delete_account))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/v1/devices", post(register_device))
        .merge(authenticated)
        .with_state(state)
}

#[derive(Debug, Error)]
enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("GitHub rejected the token")]
    BadGithubToken,
    #[error("GitHub is unavailable")]
    GithubUnavailable,
    #[error("missing or unknown device token")]
    Unauthorized,
    #[error("no such device")]
    DeviceNotFound,
    #[error("device limit reached")]
    DeviceLimit(Vec<Device>),
    #[error("retry")]
    Conflict,
    #[error("account deletion in progress")]
    DeletionInProgress,
    #[error("internal error")]
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let error = self.to_string();
        let status = match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::BadGithubToken => StatusCode::UNAUTHORIZED,
            Self::GithubUnavailable => StatusCode::BAD_GATEWAY,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::DeviceNotFound => StatusCode::NOT_FOUND,
            Self::DeviceLimit(devices) => {
                let body = DeviceLimitReached { error, devices };
                return (StatusCode::CONFLICT, Json(body)).into_response();
            }
            Self::Conflict | Self::DeletionInProgress => StatusCode::CONFLICT,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(serde_json::json!({ "error": error }))).into_response()
    }
}

async fn register_device(
    State(state): State<AppState>,
    body: Result<Json<RegisterDevice>, JsonRejection>,
) -> Result<(StatusCode, Json<DeviceCredential>), ApiError> {
    let Json(request) = body.map_err(|rejection| ApiError::BadRequest(rejection.body_text()))?;
    let name = request.name.trim();
    if !(1..=MAX_DEVICE_NAME_CHARS).contains(&name.chars().count()) {
        return Err(ApiError::BadRequest(format!(
            "name must be 1 to {MAX_DEVICE_NAME_CHARS} characters"
        )));
    }
    let user_id = format!(
        "gh:{}",
        github_user_id(&state, &request.github_token).await?
    );
    let credential = DeviceCredential {
        device_id: format!("dev_{}", loremaster_core::new_ulid()),
        token: new_token(),
    };
    let device = new_device(&state, &user_id, name, &credential)?;
    match request.replace_device_id {
        None => add_device(&state, &user_id, device).await?,
        Some(old_device_id) => replace_device(&state, &user_id, &old_device_id, device).await?,
    }
    Ok((StatusCode::CREATED, Json(credential)))
}

#[derive(Deserialize)]
struct GithubUser {
    id: u64,
}

async fn github_user_id(state: &AppState, github_token: &str) -> Result<u64, ApiError> {
    let unavailable = |error: reqwest::Error| {
        tracing::warn!(%error, "GitHub user lookup failed");
        ApiError::GithubUnavailable
    };
    let response = state
        .github
        .get(format!("{}/user", state.config.github_api_url))
        .bearer_auth(github_token)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(unavailable)?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ApiError::BadGithubToken);
    }
    let user: GithubUser = response
        .error_for_status()
        .map_err(unavailable)?
        .json()
        .await
        .map_err(unavailable)?;
    Ok(user.id)
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source is available");
    hex(&bytes)
}

fn token_hash(token: &str) -> String {
    hex(&Sha256::digest(token))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn new_device(
    state: &AppState,
    user_id: &str,
    name: &str,
    credential: &DeviceCredential,
) -> Result<TransactWriteItem, ApiError> {
    let s = |value: &str| AttributeValue::S(value.to_owned());
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_millis();
    let put = Put::builder()
        .table_name(&state.config.devices_table)
        .item("token_hash", s(&token_hash(&credential.token)))
        .item("user_id", s(user_id))
        .item("device_id", s(&credential.device_id))
        .item("name", s(name))
        .item("created_at", AttributeValue::N(created_at.to_string()))
        .condition_expression("attribute_not_exists(token_hash)")
        .build()
        .map_err(internal)?;
    Ok(TransactWriteItem::builder().put(put).build())
}

/// Creates the user if new and takes one of its device slots, in the same transaction as the
/// device, so concurrent logins cannot pass the limit and a user never exists without a device.
async fn add_device(
    state: &AppState,
    user_id: &str,
    device: TransactWriteItem,
) -> Result<(), ApiError> {
    let user = Update::builder()
        .table_name(&state.config.users_table)
        .key("user_id", AttributeValue::S(user_id.to_owned()))
        .update_expression(
            "SET next_seq = if_not_exists(next_seq, :zero), \
             live_count = if_not_exists(live_count, :zero), \
             device_count = if_not_exists(device_count, :zero) + :one",
        )
        .condition_expression(
            "(attribute_not_exists(device_count) OR device_count < :limit) \
             AND attribute_not_exists(deleting)",
        )
        .expression_attribute_values(":zero", number(0))
        .expression_attribute_values(":one", number(1))
        .expression_attribute_values(":limit", number(DEVICE_LIMIT))
        .build()
        .map_err(internal)?;
    let user = TransactWriteItem::builder().update(user).build();
    match transact(state, vec![user, device]).await {
        Ok(()) => Ok(()),
        Err(TransactError::ConditionFailed(0)) if is_deleting(state, user_id).await? => {
            Err(ApiError::DeletionInProgress)
        }
        Err(TransactError::ConditionFailed(0)) => {
            Err(ApiError::DeviceLimit(sorted_devices(state, user_id).await?))
        }
        Err(TransactError::ConditionFailed(_)) => Err(internal("new device token already exists")),
        Err(TransactError::Storage(error)) => Err(error),
    }
}

/// Removes one of the user's devices and adds the new one in one transaction, so the user's
/// device count stays the same and a failed write never leaves the user a device short.
async fn replace_device(
    state: &AppState,
    user_id: &str,
    old_device_id: &str,
    device: TransactWriteItem,
) -> Result<(), ApiError> {
    let old = remove_own_device(state, user_id, old_device_id).await?;
    let not_deleting = ConditionCheck::builder()
        .table_name(&state.config.users_table)
        .key("user_id", AttributeValue::S(user_id.to_owned()))
        .condition_expression("attribute_not_exists(deleting)")
        .build()
        .map_err(internal)?;
    let not_deleting = TransactWriteItem::builder()
        .condition_check(not_deleting)
        .build();
    match transact(state, vec![old, device, not_deleting]).await {
        Ok(()) => Ok(()),
        Err(TransactError::ConditionFailed(0)) => Err(ApiError::DeviceNotFound),
        Err(TransactError::ConditionFailed(1)) => Err(internal("new device token already exists")),
        Err(TransactError::ConditionFailed(_)) => Err(ApiError::DeletionInProgress),
        Err(TransactError::Storage(error)) => Err(error),
    }
}

/// Whether the user's account is being deleted.
async fn is_deleting(state: &AppState, user_id: &str) -> Result<bool, ApiError> {
    let user = state
        .dynamo
        .get_item()
        .table_name(&state.config.users_table)
        .key("user_id", AttributeValue::S(user_id.to_owned()))
        .consistent_read(true)
        .projection_expression("deleting")
        .send()
        .await
        .map_err(|error| internal(DisplayErrorContext(error)))?;
    Ok(user.item.is_some_and(|item| item.contains_key("deleting")))
}

fn number(value: u32) -> AttributeValue {
    AttributeValue::N(value.to_string())
}

enum TransactError {
    /// The condition on the item at this index in the transaction failed.
    ConditionFailed(usize),
    Storage(ApiError),
}

/// Runs a transaction. Tries again when another transaction on the same items ran at the same
/// moment, because the SDK does not retry a cancelled transaction.
async fn transact(state: &AppState, items: Vec<TransactWriteItem>) -> Result<(), TransactError> {
    let mut tries = 1;
    loop {
        let result = state
            .dynamo
            .transact_write_items()
            .set_transact_items(Some(items.clone()))
            .send()
            .await;
        let Err(error) = result else {
            return Ok(());
        };
        let next = match error.as_service_error() {
            Some(TransactWriteItemsError::TransactionCanceledException(cancelled)) => {
                on_cancel(cancelled.cancellation_reasons(), tries)
            }
            _ => Cancelled::Fail,
        };
        match next {
            Cancelled::Retry => tries += 1,
            Cancelled::ConditionFailed(index) => return Err(TransactError::ConditionFailed(index)),
            Cancelled::Fail => {
                return Err(TransactError::Storage(internal(DisplayErrorContext(error))));
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Cancelled {
    Retry,
    ConditionFailed(usize),
    Fail,
}

/// What to do after a cancelled transaction, from its reason for each item. A failed condition
/// wins over a conflict, since a retry cannot make that condition pass.
fn on_cancel(reasons: &[CancellationReason], tries: u32) -> Cancelled {
    let has = |code| {
        reasons
            .iter()
            .position(|reason| reason.code() == Some(code))
    };
    if let Some(index) = has("ConditionalCheckFailed") {
        Cancelled::ConditionFailed(index)
    } else if has("TransactionConflict").is_some() && tries < TRANSACTION_TRIES {
        Cancelled::Retry
    } else {
        Cancelled::Fail
    }
}

fn internal(error: impl Display) -> ApiError {
    tracing::error!(%error, "storage request failed");
    ApiError::Internal
}

/// The account and device that the request's device token belongs to.
#[derive(Clone)]
struct Caller {
    user_id: String,
    device_id: String,
}

/// Lets a request through only with the bearer token of a device that still exists.
async fn authenticate(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer") && !token.is_empty())
        .map(|(_, token)| token)
        .ok_or(ApiError::Unauthorized)?;
    // A strongly consistent read, so a deleted device's token fails on the very next request.
    let device = state
        .dynamo
        .get_item()
        .table_name(&state.config.devices_table)
        .key("token_hash", AttributeValue::S(token_hash(token)))
        .consistent_read(true)
        .send()
        .await
        .map_err(|error| internal(DisplayErrorContext(error)))?
        .item
        .ok_or(ApiError::Unauthorized)?;
    let caller = Caller {
        user_id: attribute_s(&device, "user_id")?.to_owned(),
        device_id: attribute_s(&device, "device_id")?.to_owned(),
    };
    request.extensions_mut().insert(caller);
    Ok(next.run(request).await)
}

async fn list_devices(
    State(state): State<AppState>,
    Extension(Caller { user_id, .. }): Extension<Caller>,
) -> Result<Json<DeviceList>, ApiError> {
    let devices = sorted_devices(&state, &user_id).await?;
    Ok(Json(DeviceList { devices }))
}

/// The user's devices, oldest first. Device ids are time-ordered, so they break a tie.
async fn sorted_devices(state: &AppState, user_id: &str) -> Result<Vec<Device>, ApiError> {
    let mut devices = user_devices(state, user_id)
        .await?
        .iter()
        .map(device_from_item)
        .collect::<Result<Vec<_>, _>>()?;
    devices.sort_by(|a, b| (a.created_at, &a.device_id).cmp(&(b.created_at, &b.device_id)));
    Ok(devices)
}

async fn delete_device(
    State(state): State<AppState>,
    Extension(Caller { user_id, .. }): Extension<Caller>,
    Path(device_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let device = remove_own_device(&state, &user_id, &device_id).await?;
    let user = Update::builder()
        .table_name(&state.config.users_table)
        .key("user_id", AttributeValue::S(user_id))
        .update_expression("SET device_count = device_count - :one")
        .condition_expression("device_count > :zero")
        .expression_attribute_values(":zero", number(0))
        .expression_attribute_values(":one", number(1))
        .build()
        .map_err(internal)?;
    let user = TransactWriteItem::builder().update(user).build();
    match transact(&state, vec![device, user]).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(TransactError::ConditionFailed(0)) => Err(ApiError::DeviceNotFound),
        Err(TransactError::ConditionFailed(_)) => Err(internal("device count is already 0")),
        Err(TransactError::Storage(error)) => Err(error),
    }
}

/// A transaction item that deletes one of the user's devices. Its condition fails when the device
/// is gone or belongs to another user, which the `by_user` index can still show for a moment.
async fn remove_own_device(
    state: &AppState,
    user_id: &str,
    device_id: &str,
) -> Result<TransactWriteItem, ApiError> {
    let items = user_devices(state, user_id).await?;
    let item = items
        .iter()
        .find(|item| {
            item.get("device_id")
                .and_then(|id| id.as_s().ok())
                .map(String::as_str)
                == Some(device_id)
        })
        .ok_or(ApiError::DeviceNotFound)?;
    let delete = Delete::builder()
        .table_name(&state.config.devices_table)
        .key(
            "token_hash",
            AttributeValue::S(attribute_s(item, "token_hash")?.to_owned()),
        )
        .condition_expression("attribute_exists(token_hash) AND user_id = :user_id")
        .expression_attribute_values(":user_id", AttributeValue::S(user_id.to_owned()))
        .build()
        .map_err(internal)?;
    Ok(TransactWriteItem::builder().delete(delete).build())
}

type Item = HashMap<String, AttributeValue>;

/// All of the user's device items, from the `by_user` index. The index is eventually
/// consistent, so a device made or deleted a moment ago can be missing or still present.
async fn user_devices(state: &AppState, user_id: &str) -> Result<Vec<Item>, ApiError> {
    state
        .dynamo
        .query()
        .table_name(&state.config.devices_table)
        .index_name("by_user")
        .key_condition_expression("user_id = :user_id")
        .expression_attribute_values(":user_id", AttributeValue::S(user_id.to_owned()))
        .into_paginator()
        .items()
        .send()
        .try_collect()
        .await
        .map_err(|error| internal(DisplayErrorContext(error)))
}

fn device_from_item(item: &Item) -> Result<Device, ApiError> {
    let created_at = item
        .get("created_at")
        .and_then(|value| value.as_n().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| internal("device item has no valid created_at"))?;
    Ok(Device {
        device_id: attribute_s(item, "device_id")?.to_owned(),
        name: attribute_s(item, "name")?.to_owned(),
        created_at,
    })
}

fn attribute_s<'a>(item: &'a Item, name: &str) -> Result<&'a str, ApiError> {
    item.get(name)
        .and_then(|value| value.as_s().ok())
        .map(String::as_str)
        .ok_or_else(|| internal(format!("item has no string {name}")))
}

/// Stores the records that are newer than the stored versions, each with the user's next `seq`.
/// The user item's `next_seq` is a condition of the write, so pushes of one user apply one at a
/// time and a `seq` a reader has seen is never followed by a lower one.
async fn push_records(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Result<Json<PushRequest>, JsonRejection>,
) -> Result<Json<PushResponse>, ApiError> {
    let Json(PushRequest { records }) =
        body.map_err(|rejection| ApiError::BadRequest(rejection.body_text()))?;
    check_push(&records, &caller.device_id)?;
    if records.is_empty() {
        return Ok(Json(PushResponse {
            applied: Vec::new(),
        }));
    }
    let next_seq = next_seq(&state, &caller.user_id).await?;
    let stored = stored_versions(&state, &caller.user_id, &records).await?;
    let newer: Vec<&Record> = records
        .iter()
        .filter(|record| {
            stored
                .get(&record.memory_id)
                .is_none_or(|version| version.hlc < record.hlc)
        })
        .collect();
    if newer.is_empty() {
        return Ok(Json(PushResponse {
            applied: Vec::new(),
        }));
    }
    let mut live_delta = 0_i64;
    let mut items = Vec::with_capacity(newer.len() + 1);
    for (seq, record) in (next_seq + 1..).zip(&newer) {
        let read = stored.get(&record.memory_id);
        let was_live = read.is_some_and(|version| !version.deleted);
        live_delta += match (was_live, record.deleted) {
            (false, false) => 1,
            (true, true) => -1,
            _ => 0,
        };
        items.push(memory_put(&state, &caller.user_id, seq, record, read)?);
    }
    let new_next_seq = next_seq + newer.len() as i64;
    let user = Update::builder()
        .table_name(&state.config.users_table)
        .key("user_id", AttributeValue::S(caller.user_id.clone()))
        .update_expression("SET next_seq = :new, live_count = live_count + :delta")
        // A push that passed auth just before an account deletion must not write rows that the
        // purge has already listed.
        .condition_expression("next_seq = :old AND attribute_not_exists(deleting)")
        .expression_attribute_values(":old", AttributeValue::N(next_seq.to_string()))
        .expression_attribute_values(":new", AttributeValue::N(new_next_seq.to_string()))
        .expression_attribute_values(":delta", AttributeValue::N(live_delta.to_string()))
        .build()
        .map_err(internal)?;
    items.insert(0, TransactWriteItem::builder().update(user).build());
    match transact(&state, items).await {
        Ok(()) => Ok(Json(PushResponse {
            applied: newer
                .iter()
                .map(|record| record.memory_id.clone())
                .collect(),
        })),
        // Another push for this user wrote between the reads and this write, or the account is
        // being deleted.
        Err(TransactError::ConditionFailed(_)) => Err(ApiError::Conflict),
        Err(TransactError::Storage(error)) => Err(error),
    }
}

fn check_push(records: &[Record], device_id: &str) -> Result<(), ApiError> {
    if records.len() > MAX_PUSH_RECORDS {
        return Err(ApiError::BadRequest(format!(
            "a push has at most {MAX_PUSH_RECORDS} records"
        )));
    }
    let mut seen = HashSet::new();
    for record in records {
        let reason = if seen.insert(record.memory_id.as_str()) {
            check_record(record, device_id).err()
        } else {
            Some("memory_id appears twice")
        };
        if let Some(reason) = reason {
            return Err(ApiError::BadRequest(format!(
                "record {}: {reason}",
                record.memory_id
            )));
        }
    }
    Ok(())
}

fn check_record(record: &Record, device_id: &str) -> Result<(), &'static str> {
    if !loremaster_core::is_ulid(&record.memory_id) {
        return Err("memory_id is not a ULID");
    }
    let clock = Hlc::parse(&record.hlc).map_err(|_| "hlc does not parse")?;
    if record.writer != device_id {
        return Err("writer is not this device");
    }
    if clock.writer() != record.writer {
        return Err("hlc writer is not writer");
    }
    if record.deleted {
        return Ok(());
    }
    let text = record.text.as_deref().ok_or("text is missing")?;
    if !(1..=MAX_TEXT_CHARS).contains(&text.chars().count()) {
        return Err("text must be 1 to 2,000 characters");
    }
    if record.created_at.is_none() {
        return Err("created_at is missing");
    }
    match (record.record_type.ok_or("type is missing")?, record.kind) {
        (RecordType::Fact, None) => return Err("a fact needs a kind"),
        (RecordType::Episode, Some(_)) => return Err("an episode has no kind"),
        _ => {}
    }
    match (record.scope.ok_or("scope is missing")?, &record.project_id) {
        (Scope::Project, None) => Err("project scope needs a project_id"),
        (Scope::User, Some(_)) => Err("user scope has no project_id"),
        _ => Ok(()),
    }
}

struct StoredVersion {
    hlc: String,
    deleted: bool,
}

/// The stored `hlc` of each pushed memory that the user already has.
async fn stored_versions(
    state: &AppState,
    user_id: &str,
    records: &[Record],
) -> Result<HashMap<String, StoredVersion>, ApiError> {
    let table = &state.config.memories_table;
    let mut versions = HashMap::new();
    let mut keys = KeysAndAttributes::builder()
        .consistent_read(true)
        .projection_expression("memory_id, hlc, deleted")
        .set_keys(Some(
            records
                .iter()
                .map(|record| {
                    HashMap::from([
                        ("user_id".to_owned(), AttributeValue::S(user_id.to_owned())),
                        (
                            "memory_id".to_owned(),
                            AttributeValue::S(record.memory_id.clone()),
                        ),
                    ])
                })
                .collect(),
        ))
        .build()
        .map_err(internal)?;
    loop {
        let output = state
            .dynamo
            .batch_get_item()
            .request_items(table, keys)
            .send()
            .await
            .map_err(|error| internal(DisplayErrorContext(error)))?;
        for item in output
            .responses
            .and_then(|mut tables| tables.remove(table))
            .into_iter()
            .flatten()
        {
            let deleted = item
                .get("deleted")
                .is_some_and(|value| value.as_bool() == Ok(&true));
            versions.insert(
                attribute_s(&item, "memory_id")?.to_owned(),
                StoredVersion {
                    hlc: attribute_s(&item, "hlc")?.to_owned(),
                    deleted,
                },
            );
        }
        // DynamoDB can return part of a batch; the rest comes back as unprocessed keys.
        match output
            .unprocessed_keys
            .and_then(|mut tables| tables.remove(table))
        {
            Some(rest) if !rest.keys().is_empty() => keys = rest,
            _ => return Ok(versions),
        }
    }
}

async fn next_seq(state: &AppState, user_id: &str) -> Result<i64, ApiError> {
    let user = state
        .dynamo
        .get_item()
        .table_name(&state.config.users_table)
        .key("user_id", AttributeValue::S(user_id.to_owned()))
        .consistent_read(true)
        .projection_expression("next_seq")
        .send()
        .await
        .map_err(|error| internal(DisplayErrorContext(error)))?
        .item
        .ok_or_else(|| internal("authenticated user has no user item"))?;
    user_number(&user, "next_seq")
}

fn user_number(user: &Item, name: &str) -> Result<i64, ApiError> {
    user.get(name)
        .and_then(|value| value.as_n().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| internal(format!("user item has no valid {name}")))
}

/// The stored form of a record. A tombstone keeps only its ids, `seq`, `hlc` and `writer`, so
/// deleted text leaves the service.
///
/// The put only replaces the exact version that `read` saw. A batch read gives no order across
/// items, so another push can commit between the reads; the live count delta is then wrong, and
/// this condition fails the push instead of storing it.
fn memory_put(
    state: &AppState,
    user_id: &str,
    seq: i64,
    record: &Record,
    read: Option<&StoredVersion>,
) -> Result<TransactWriteItem, ApiError> {
    let s = |value: &str| AttributeValue::S(value.to_owned());
    let mut item = HashMap::from([
        ("user_id".to_owned(), s(user_id)),
        ("memory_id".to_owned(), s(&record.memory_id)),
        ("seq".to_owned(), AttributeValue::N(seq.to_string())),
        ("hlc".to_owned(), s(&record.hlc)),
        ("writer".to_owned(), s(&record.writer)),
    ]);
    if record.deleted {
        item.insert("deleted".to_owned(), AttributeValue::Bool(true));
    } else {
        let (Some(record_type), Some(scope), Some(text), Some(created_at)) = (
            record.record_type,
            record.scope,
            &record.text,
            record.created_at,
        ) else {
            return Err(internal("live record passed its checks without content"));
        };
        // Cap eviction removes replaced facts first, then episodes, then other facts.
        let class = match (record_type, &record.superseded_by) {
            (RecordType::Fact, Some(_)) => 1,
            (RecordType::Episode, _) => 2,
            (RecordType::Fact, None) => 3,
        };
        item.extend([
            ("type".to_owned(), s(record_type.as_str())),
            ("scope".to_owned(), s(scope.as_str())),
            ("text".to_owned(), s(text)),
            (
                "created_at".to_owned(),
                AttributeValue::N(created_at.to_string()),
            ),
            ("evict_key".to_owned(), s(&format!("{class}#{created_at}"))),
        ]);
        let optional = [
            ("kind", record.kind.map(|kind| kind.as_str().to_owned())),
            ("project_id", record.project_id.clone()),
            ("superseded_by", record.superseded_by.clone()),
        ];
        for (name, value) in optional {
            if let Some(value) = value {
                item.insert(name.to_owned(), AttributeValue::S(value));
            }
        }
    }
    let put = Put::builder()
        .table_name(&state.config.memories_table)
        .set_item(Some(item));
    let put = match read {
        None => put.condition_expression("attribute_not_exists(memory_id)"),
        Some(version) => put
            .condition_expression("hlc = :read_hlc")
            .expression_attribute_values(":read_hlc", s(&version.hlc)),
    };
    let put = put.build().map_err(internal)?;
    Ok(TransactWriteItem::builder().put(put).build())
}

/// One page of the user's records with a `seq` above the cursor, in `seq` order. The query is
/// strongly consistent: an eventual read can show a `seq` before a lower one, and a reader whose
/// cursor moves past the lower one never gets it.
async fn pull_records(
    State(state): State<AppState>,
    Extension(Caller { user_id, .. }): Extension<Caller>,
    RawQuery(query): RawQuery,
) -> Result<Json<PullResponse>, ApiError> {
    let cursor = query
        .iter()
        .flat_map(|query| query.split('&'))
        .find_map(|pair| pair.strip_prefix("cursor="))
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|cursor| *cursor >= 0)
        .ok_or_else(|| {
            ApiError::BadRequest("cursor must be a whole number, 0 or more".to_owned())
        })?;
    let (next_seq, total) = committed_counters(&state, &user_id).await?;
    let page = state
        .dynamo
        .query()
        .table_name(&state.config.memories_table)
        .index_name("by_seq")
        .key_condition_expression("user_id = :user AND seq > :cursor")
        .expression_attribute_values(":user", AttributeValue::S(user_id))
        .expression_attribute_values(":cursor", AttributeValue::N(cursor.to_string()))
        .consistent_read(true)
        .send()
        .await
        .map_err(|error| internal(DisplayErrorContext(error)))?;
    let records = page
        .items()
        .iter()
        .map(record_from_item)
        .collect::<Result<Vec<_>, _>>()?;
    // The query stops at 1 MB and then gives the key to go on from.
    let more = page.last_evaluated_key.is_some();
    Ok(Json(pull_page(records, cursor, next_seq, more, total)))
}

/// The user's `next_seq` and `live_count`, read serializable against pushes. A query is only
/// read-committed against a transaction, so it can show part of a push in progress. This read
/// instead fails while a push on the user item is in progress, so every push up to the
/// `next_seq` it gives is complete.
async fn committed_counters(state: &AppState, user_id: &str) -> Result<(i64, i64), ApiError> {
    let get = Get::builder()
        .table_name(&state.config.users_table)
        .key("user_id", AttributeValue::S(user_id.to_owned()))
        .projection_expression("next_seq, live_count")
        .build()
        .map_err(internal)?;
    let get = TransactGetItem::builder().get(get).build();
    for _ in 0..TRANSACTION_TRIES {
        let result = state
            .dynamo
            .transact_get_items()
            .transact_items(get.clone())
            .send()
            .await;
        let error = match result {
            Ok(output) => {
                let user = output
                    .responses()
                    .first()
                    .and_then(|response| response.item())
                    .ok_or_else(|| internal("authenticated user has no user item"))?;
                return Ok((
                    user_number(user, "next_seq")?,
                    user_number(user, "live_count")?,
                ));
            }
            Err(error) => error,
        };
        let in_progress = match error.as_service_error() {
            Some(TransactGetItemsError::TransactionCanceledException(cancelled)) => cancelled
                .cancellation_reasons()
                .iter()
                .any(|reason| reason.code() == Some("TransactionConflict")),
            _ => false,
        };
        if !in_progress {
            return Err(internal(DisplayErrorContext(error)));
        }
    }
    Err(ApiError::Conflict)
}

/// Keeps the records up to `next_seq`, which only complete pushes reach. A cut page has more
/// after it.
fn pull_page(
    mut records: Vec<Record>,
    cursor: i64,
    next_seq: i64,
    more: bool,
    total: i64,
) -> PullResponse {
    let complete = records.partition_point(|record| record.seq.is_some_and(|seq| seq <= next_seq));
    let cut = complete < records.len();
    records.truncate(complete);
    PullResponse {
        next_cursor: records
            .last()
            .and_then(|record| record.seq)
            .unwrap_or(cursor),
        records,
        total,
        more: more || cut,
    }
}

/// A stored memory as a record, with every attribute it has.
fn record_from_item(item: &Item) -> Result<Record, ApiError> {
    let fields = item
        .iter()
        .map(|(name, value)| {
            let field = match value {
                AttributeValue::S(text) => serde_json::Value::from(text.as_str()),
                AttributeValue::N(number) => number
                    .parse::<i64>()
                    .map_err(|_| internal(format!("memory item has a bad number {name}")))?
                    .into(),
                AttributeValue::Bool(flag) => (*flag).into(),
                _ => {
                    return Err(internal(format!(
                        "memory item has an unknown type of {name}"
                    )));
                }
            };
            Ok((name.clone(), field))
        })
        .collect::<Result<serde_json::Map<_, _>, _>>()?;
    serde_json::from_value(fields.into()).map_err(internal)
}

/// Marks the account as being deleted, so no device can sign up for it, then revokes every device
/// token. The rows go in a purge after the reply. If that purge never ends, the marker stays and
/// shows the account.
async fn delete_account(
    State(state): State<AppState>,
    Extension(Caller { user_id, .. }): Extension<Caller>,
) -> Result<StatusCode, ApiError> {
    state
        .dynamo
        .update_item()
        .table_name(&state.config.users_table)
        .key("user_id", AttributeValue::S(user_id.clone()))
        .update_expression("SET deleting = :true")
        .expression_attribute_values(":true", AttributeValue::Bool(true))
        .send()
        .await
        .map_err(|error| internal(DisplayErrorContext(error)))?;
    let devices = device_keys(&state, &user_id).await?;
    batch_delete(&state, &state.config.devices_table, devices).await?;
    match &state.purger {
        Some(Purger {
            lambda,
            function_name,
        }) => {
            let event = serde_json::json!({ "purge_user_id": user_id });
            lambda
                .invoke()
                .function_name(function_name)
                .invocation_type(InvocationType::Event)
                .payload(Blob::new(event.to_string()))
                .send()
                .await
                .map_err(|error| internal(DisplayErrorContext(error)))?;
        }
        None => purge(&state, &user_id).await?,
    }
    Ok(StatusCode::ACCEPTED)
}

/// One invocation of the Lambda function: a request from the HTTP API, or an account purge that
/// `DELETE /v1/account` sent to the function itself.
#[derive(Debug)]
pub enum Event {
    Purge { user_id: String },
    Http(Box<LambdaRequest>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PurgeEvent {
    purge_user_id: String,
}

impl<'de> Deserialize<'de> for Event {
    /// Reads the payload once, because `LambdaRequest` only reads raw JSON and so cannot be a
    /// variant of an untagged enum.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        if let Ok(PurgeEvent { purge_user_id }) = serde_json::from_str(raw.get()) {
            return Ok(Self::Purge {
                user_id: purge_user_id,
            });
        }
        serde_json::from_str(raw.get())
            .map(|request| Self::Http(Box::new(request)))
            .map_err(serde::de::Error::custom)
    }
}

/// An account purge stopped on a storage error. The error is in the log.
#[derive(Debug, Error)]
#[error("account purge failed")]
pub struct PurgeFailed;

/// Deletes all of the user's memories and devices, then the user item. Deleting rows that are
/// already gone is not an error, so a purge can run again after a failure.
pub async fn purge_account(state: &AppState, user_id: &str) -> Result<(), PurgeFailed> {
    purge(state, user_id).await.map_err(|_| PurgeFailed)
}

async fn purge(state: &AppState, user_id: &str) -> Result<(), ApiError> {
    let user = || AttributeValue::S(user_id.to_owned());
    // A consistent read, so a push that committed just before the deletion marker is not missed.
    // One page at a time, so the largest account does not hold all its keys in memory.
    let mut pages = state
        .dynamo
        .query()
        .table_name(&state.config.memories_table)
        .key_condition_expression("user_id = :user_id")
        .expression_attribute_values(":user_id", user())
        .projection_expression("user_id, memory_id")
        .consistent_read(true)
        .into_paginator()
        .send();
    while let Some(page) = pages.next().await {
        let page = page.map_err(|error| internal(DisplayErrorContext(error)))?;
        batch_delete(
            state,
            &state.config.memories_table,
            page.items.unwrap_or_default(),
        )
        .await?;
    }
    let devices = device_keys(state, user_id).await?;
    batch_delete(state, &state.config.devices_table, devices).await?;
    // Last, so a failed purge leaves the user item and its deletion marker behind.
    state
        .dynamo
        .delete_item()
        .table_name(&state.config.users_table)
        .key("user_id", user())
        .send()
        .await
        .map_err(|error| internal(DisplayErrorContext(error)))?;
    Ok(())
}

/// The table keys of the user's devices.
async fn device_keys(state: &AppState, user_id: &str) -> Result<Vec<Item>, ApiError> {
    user_devices(state, user_id)
        .await?
        .iter()
        .map(|item| {
            let hash = attribute_s(item, "token_hash")?.to_owned();
            Ok(HashMap::from([(
                "token_hash".to_owned(),
                AttributeValue::S(hash),
            )]))
        })
        .collect()
}

/// Deletes the items with these keys, 25 to a request. One request after the other: about
/// 2,000 requests for the largest account. Send them concurrently if that becomes too slow.
async fn batch_delete(state: &AppState, table: &str, keys: Vec<Item>) -> Result<(), ApiError> {
    let mut requests = keys
        .into_iter()
        .map(|key| {
            let delete = DeleteRequest::builder()
                .set_key(Some(key))
                .build()
                .map_err(internal)?;
            Ok(WriteRequest::builder().delete_request(delete).build())
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    while !requests.is_empty() {
        let mut batch: Vec<_> = requests
            .drain(..requests.len().min(BATCH_WRITE_ITEMS))
            .collect();
        let mut backoff = UNPROCESSED_BACKOFF;
        loop {
            let unprocessed = state
                .dynamo
                .batch_write_item()
                .request_items(table, batch)
                .send()
                .await
                .map_err(|error| internal(DisplayErrorContext(error)))?
                .unprocessed_items
                .and_then(|mut tables| tables.remove(table))
                .unwrap_or_default();
            if unprocessed.is_empty() {
                break;
            }
            // DynamoDB returns the writes it could not do now, mostly under throttling.
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_UNPROCESSED_BACKOFF);
            batch = unprocessed;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reasons(codes: &[&str]) -> Vec<CancellationReason> {
        codes
            .iter()
            .map(|code| CancellationReason::builder().code(*code).build())
            .collect()
    }

    fn record_with_seq(seq: i64) -> Record {
        Record {
            user_id: None,
            memory_id: format!("m{seq}"),
            seq: Some(seq),
            hlc: "1761750000123-00000-dev_a".to_owned(),
            writer: "dev_a".to_owned(),
            record_type: None,
            kind: None,
            scope: None,
            project_id: None,
            text: None,
            created_at: None,
            superseded_by: None,
            deleted: true,
            evict_key: None,
        }
    }

    #[test]
    fn a_pull_page_stops_at_the_last_complete_push() {
        let records = || (4..=7).map(record_with_seq).collect::<Vec<_>>();
        let seqs = |page: &PullResponse| page.records.iter().map(|r| r.seq).collect::<Vec<_>>();

        let cut = pull_page(records(), 3, 5, false, 9);
        assert_eq!(seqs(&cut), [Some(4), Some(5)]);
        assert_eq!((cut.next_cursor, cut.total, cut.more), (5, 9, true));

        let whole = pull_page(records(), 3, 7, false, 9);
        assert_eq!(whole.records.len(), 4);
        assert_eq!((whole.next_cursor, whole.more), (7, false));
        assert!(pull_page(records(), 3, 7, true, 9).more);

        let nothing_complete = pull_page(records(), 3, 3, false, 9);
        assert!(nothing_complete.records.is_empty());
        assert_eq!(
            (nothing_complete.next_cursor, nothing_complete.more),
            (3, true)
        );
    }

    #[test]
    fn an_invocation_is_a_purge_or_an_http_request() {
        let purge: Event = serde_json::from_str(r#"{"purge_user_id": "gh:1"}"#).unwrap();
        assert!(matches!(purge, Event::Purge { user_id } if user_id == "gh:1"));

        let http = serde_json::json!({
            "version": "2.0",
            "routeKey": "$default",
            "rawPath": "/v1/devices",
            "rawQueryString": "",
            "headers": {"authorization": "Bearer abc", "host": "api.example.com"},
            "requestContext": {
                "accountId": "123456789012",
                "apiId": "api",
                "domainName": "api.example.com",
                "domainPrefix": "api",
                "http": {
                    "method": "GET",
                    "path": "/v1/devices",
                    "protocol": "HTTP/1.1",
                    "sourceIp": "192.0.2.1",
                    "userAgent": "loremaster"
                },
                "requestId": "id",
                "routeKey": "$default",
                "stage": "$default",
                "time": "06/Nov/2026:12:00:00 +0000",
                "timeEpoch": 1793966400000_i64
            },
            "isBase64Encoded": false
        });
        let http: Event = serde_json::from_str(&http.to_string()).unwrap();
        assert!(matches!(http, Event::Http(_)));

        for not_an_event in [r#"{"purge_user_id": 1}"#, r#"{"other": "gh:1"}"#, "[]"] {
            assert!(
                serde_json::from_str::<Event>(not_an_event).is_err(),
                "{not_an_event}"
            );
        }
    }

    #[test]
    fn a_cancelled_transaction_is_retried_only_for_a_conflict_and_at_most_three_times() {
        let cases = [
            (
                &["None", "ConditionalCheckFailed"][..],
                1,
                Cancelled::ConditionFailed(1),
            ),
            (
                &["ConditionalCheckFailed", "None"],
                3,
                Cancelled::ConditionFailed(0),
            ),
            (&["TransactionConflict", "None"], 1, Cancelled::Retry),
            (&["None", "TransactionConflict"], 2, Cancelled::Retry),
            (&["TransactionConflict", "None"], 3, Cancelled::Fail),
            (
                &["TransactionConflict", "ConditionalCheckFailed"],
                1,
                Cancelled::ConditionFailed(1),
            ),
            (&["ValidationError", "None"], 1, Cancelled::Fail),
            (&[], 1, Cancelled::Fail),
        ];
        for (codes, tries, expected) in cases {
            assert_eq!(
                on_cancel(&reasons(codes), tries),
                expected,
                "{codes:?} on try {tries}"
            );
        }
    }
}
