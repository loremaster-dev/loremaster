//! The sync service: one `axum` router that runs in a Lambda behind an HTTP API.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::collections::HashMap;
use std::fmt::Display;

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::error::DisplayErrorContext;
use aws_sdk_dynamodb::operation::delete_item::DeleteItemError;
use aws_sdk_dynamodb::types::{AttributeValue, Put, TransactWriteItem, Update};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use loremaster_core::{Device, DeviceCredential, DeviceList, RegisterDevice};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_DEVICE_NAME_CHARS: usize = 64;
const GITHUB_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the service keeps its data and checks GitHub accounts.
pub struct Config {
    pub users_table: String,
    pub devices_table: String,
    pub github_api_url: String,
}

/// A required environment variable is not set.
#[derive(Debug, Error)]
#[error("environment variable {0} is not set")]
pub struct MissingEnv(&'static str);

impl Config {
    /// Reads `USERS_TABLE`, `DEVICES_TABLE` and `GITHUB_API_URL` (default `https://api.github.com`).
    pub fn from_env() -> Result<Self, MissingEnv> {
        let required = |name| std::env::var(name).map_err(|_| MissingEnv(name));
        Ok(Self {
            users_table: required("USERS_TABLE")?,
            devices_table: required("DEVICES_TABLE")?,
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
        }
    }
}

pub fn router(state: AppState) -> Router {
    let authenticated = Router::new()
        .route("/v1/devices", get(list_devices))
        .route("/v1/devices/{id}", delete(delete_device))
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
    #[error("internal error")]
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::BadGithubToken => StatusCode::UNAUTHORIZED,
            Self::GithubUnavailable => StatusCode::BAD_GATEWAY,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::DeviceNotFound => StatusCode::NOT_FOUND,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = serde_json::json!({"error": self.to_string()});
        (status, Json(body)).into_response()
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
    store_device(&state, &user_id, name, &credential).await?;
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

/// Creates the user if new and the device, in one transaction, so a user never exists without one.
async fn store_device(
    state: &AppState,
    user_id: &str,
    name: &str,
    credential: &DeviceCredential,
) -> Result<(), ApiError> {
    let s = |value: &str| AttributeValue::S(value.to_owned());
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_millis();
    let user = Update::builder()
        .table_name(&state.config.users_table)
        .key("user_id", s(user_id))
        .update_expression(
            "SET next_seq = if_not_exists(next_seq, :zero), \
             live_count = if_not_exists(live_count, :zero)",
        )
        .expression_attribute_values(":zero", AttributeValue::N("0".to_owned()))
        .build()
        .map_err(internal)?;
    let device = Put::builder()
        .table_name(&state.config.devices_table)
        .item("token_hash", s(&token_hash(&credential.token)))
        .item("user_id", s(user_id))
        .item("device_id", s(&credential.device_id))
        .item("name", s(name))
        .item("created_at", AttributeValue::N(created_at.to_string()))
        .condition_expression("attribute_not_exists(token_hash)")
        .build()
        .map_err(internal)?;
    state
        .dynamo
        .transact_write_items()
        .transact_items(TransactWriteItem::builder().update(user).build())
        .transact_items(TransactWriteItem::builder().put(device).build())
        .send()
        .await
        .map_err(|error| internal(DisplayErrorContext(error)))?;
    Ok(())
}

fn internal(error: impl Display) -> ApiError {
    tracing::error!(%error, "storage request failed");
    ApiError::Internal
}

/// The account that the request's device token belongs to.
#[derive(Clone)]
struct UserId(String);

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
    let user_id = attribute_s(&device, "user_id")?.to_owned();
    request.extensions_mut().insert(UserId(user_id));
    Ok(next.run(request).await)
}

async fn list_devices(
    State(state): State<AppState>,
    Extension(UserId(user_id)): Extension<UserId>,
) -> Result<Json<DeviceList>, ApiError> {
    let mut devices = user_devices(&state, &user_id)
        .await?
        .iter()
        .map(device_from_item)
        .collect::<Result<Vec<_>, _>>()?;
    devices.sort_by_key(|device| device.created_at);
    Ok(Json(DeviceList { devices }))
}

async fn delete_device(
    State(state): State<AppState>,
    Extension(UserId(user_id)): Extension<UserId>,
    Path(device_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let items = user_devices(&state, &user_id).await?;
    let item = items
        .iter()
        .find(|item| item.get("device_id").and_then(|id| id.as_s().ok()) == Some(&device_id))
        .ok_or(ApiError::DeviceNotFound)?;
    let token_hash = attribute_s(item, "token_hash")?;
    let result = state
        .dynamo
        .delete_item()
        .table_name(&state.config.devices_table)
        .key("token_hash", AttributeValue::S(token_hash.to_owned()))
        .condition_expression("attribute_exists(token_hash) AND user_id = :user_id")
        .expression_attribute_values(":user_id", AttributeValue::S(user_id))
        .send()
        .await;
    match result {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(error)
            if matches!(
                error.as_service_error(),
                Some(DeleteItemError::ConditionalCheckFailedException(_))
            ) =>
        {
            Err(ApiError::DeviceNotFound)
        }
        Err(error) => Err(internal(DisplayErrorContext(error))),
    }
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
        .ok_or_else(|| internal(format!("device item has no string {name}")))
}
