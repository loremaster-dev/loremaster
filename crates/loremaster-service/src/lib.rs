//! The sync service: one `axum` router that runs in a Lambda behind an HTTP API.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::{AttributeValue, Put, TransactWriteItem, Update};
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use loremaster_core::{DeviceCredential, RegisterDevice};
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
    Router::new()
        .route("/v1/devices", post(register_device))
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
    #[error("internal error")]
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::BadGithubToken => StatusCode::UNAUTHORIZED,
            Self::GithubUnavailable => StatusCode::BAD_GATEWAY,
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
    let internal = |error: &dyn std::fmt::Display| {
        tracing::error!(%error, "device write failed");
        ApiError::Internal
    };
    let s = |value: &str| AttributeValue::S(value.to_owned());
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| internal(&error))?
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
        .map_err(|error| internal(&error))?;
    let device = Put::builder()
        .table_name(&state.config.devices_table)
        .item("token_hash", s(&hex(&Sha256::digest(&credential.token))))
        .item("user_id", s(user_id))
        .item("device_id", s(&credential.device_id))
        .item("name", s(name))
        .item("created_at", AttributeValue::N(created_at.to_string()))
        .condition_expression("attribute_not_exists(token_hash)")
        .build()
        .map_err(|error| internal(&error))?;
    state
        .dynamo
        .transact_write_items()
        .transact_items(TransactWriteItem::builder().update(user).build())
        .transact_items(TransactWriteItem::builder().put(device).build())
        .send()
        .await
        .map_err(|error| internal(&aws_sdk_dynamodb::error::DisplayErrorContext(error)))?;
    Ok(())
}
