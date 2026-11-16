//! Claude on the Anthropic Messages API: the prompts live here, and devices send only their data.

use std::time::Duration;

use loremaster_core::{Candidate, Neighbour, ReconcileRequest, ReconcileResponse};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::MAX_TEXT_CHARS;

const EXTRACT_PROMPT: &str = include_str!("../prompts/extract.md");
const RECONCILE_PROMPT: &str = include_str!("../prompts/reconcile.md");
const RECORD_CANDIDATES: &str = "record_candidates";
const RECORD_DECISION: &str = "record_decision";
const MAX_CANDIDATES: usize = 3;
// Room for 3 facts of 2,000 characters each.
const MAX_OUTPUT_TOKENS: u32 = 4_096;
// Room for one fact of 2,000 characters.
const MAX_DECISION_TOKENS: u32 = 1_024;
const API_VERSION: &str = "2026-10-21";
// Anthropic's status for "overloaded"; it has no `reqwest::StatusCode` constant.
const OVERLOADED: u16 = 529;

/// An HTTP client that signs each Messages API call with `api_key` and gives up after `timeout`.
/// It does not retry: a failed call leaves the turn queued on the device, which tries again later.
pub fn model_client(api_key: &str, timeout: Duration) -> reqwest::Client {
    let mut key = HeaderValue::from_str(api_key).expect("API key is a valid header value");
    key.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", key);
    headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(timeout)
        .user_agent("loremaster-service")
        .build()
        .expect("TLS backend initializes")
}

#[derive(Debug)]
pub(crate) enum LlmError {
    /// The API refused the call because of a rate limit or overload.
    Throttled,
    /// The API failed, did not answer in time, or answered in an unknown shape.
    Failed,
}

/// What Claude answered. `tool_input` is absent when the reply has no call of the forced tool.
pub(crate) struct Reply {
    pub input_tokens: u64,
    pub tool_input: Option<Value>,
}

#[derive(Deserialize)]
struct Message {
    content: Vec<Block>,
    stop_reason: Option<String>,
    usage: Usage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    ToolUse {
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
}

/// A tool that Claude must call: its `input` is the structured answer.
struct Tool {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
}

/// Asks Claude which facts in the prepared turn are worth remembering.
pub(crate) async fn extract(
    client: &reqwest::Client,
    api_url: &str,
    model_id: &str,
    turn: &str,
) -> Result<Reply, LlmError> {
    let tool = Tool {
        name: RECORD_CANDIDATES,
        description: "Records the facts from the turn that are worth remembering.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "candidates": {
                    "type": "array",
                    "maxItems": MAX_CANDIDATES,
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string", "maxLength": MAX_TEXT_CHARS},
                            "scope": {"type": "string", "enum": ["user", "project"]},
                            "kind": {
                                "type": "string",
                                "enum": ["preference", "decision", "correction", "environment", "pitfall"],
                            },
                        },
                        "required": ["text", "scope", "kind"],
                    },
                },
            },
            "required": ["candidates"],
        }),
    };
    call_tool(
        client,
        api_url,
        model_id,
        EXTRACT_PROMPT,
        &tool,
        MAX_OUTPUT_TOKENS,
        turn,
    )
    .await
}

/// Asks Claude whether the candidate is new, already stored, or replaces one of its neighbours.
pub(crate) async fn reconcile(
    client: &reqwest::Client,
    api_url: &str,
    model_id: &str,
    request: &ReconcileRequest,
) -> Result<Reply, LlmError> {
    let tool = Tool {
        name: RECORD_DECISION,
        description: "Records what to do with the candidate fact.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "decision": {"type": "string", "enum": ["add", "supersede", "skip"]},
                "memory_id": {
                    "type": "string",
                    "description": "For supersede: the neighbour that the new text replaces.",
                },
                "text": {
                    "type": "string",
                    "maxLength": MAX_TEXT_CHARS,
                    "description": "For supersede: the fact to store in place of the neighbour.",
                },
            },
            "required": ["decision"],
        }),
    };
    let content = serde_json::to_string(request).expect("a request serializes");
    call_tool(
        client,
        api_url,
        model_id,
        RECONCILE_PROMPT,
        &tool,
        MAX_DECISION_TOKENS,
        &content,
    )
    .await
}

/// One Messages call with `system` as the prompt, `content` as the user message and `tool` as
/// the forced tool.
async fn call_tool(
    client: &reqwest::Client,
    api_url: &str,
    model_id: &str,
    system: &str,
    tool: &Tool,
    max_tokens: u32,
    content: &str,
) -> Result<Reply, LlmError> {
    let request = json!({
        "model": model_id,
        "max_tokens": max_tokens,
        "system": system,
        "messages": [{"role": "user", "content": content}],
        "tools": [{
            "name": tool.name,
            "description": tool.description,
            "input_schema": tool.input_schema,
        }],
        "tool_choice": {"type": "tool", "name": tool.name},
    });
    let failed = |error: reqwest::Error| {
        tracing::error!(%error, "Messages API call failed");
        LlmError::Failed
    };
    let response = client
        .post(format!("{api_url}/v1/messages"))
        .json(&request)
        .send()
        .await
        .map_err(failed)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.as_u16() == OVERLOADED {
            tracing::warn!(%status, %body, "Messages API throttled");
            return Err(LlmError::Throttled);
        }
        tracing::error!(%status, %body, "Messages API refused the call");
        return Err(LlmError::Failed);
    }
    let message: Message = response.json().await.map_err(failed)?;
    let tool_input = message.content.into_iter().find_map(|block| match block {
        Block::ToolUse { name, input } if name == tool.name => Some(input),
        _ => None,
    });
    if tool_input.is_none() {
        // A safety refusal or a reply cut at `max_tokens` has no tool call.
        tracing::error!(stop_reason = ?message.stop_reason, "reply has no call of the tool");
    }
    Ok(Reply {
        input_tokens: message.usage.input_tokens,
        tool_input,
    })
}

/// The valid candidates in the tool input, at most 3. One bad candidate does not cost the device
/// the whole turn, so bad ones are dropped, not refused.
pub(crate) fn candidates(tool_input: Option<Value>) -> Result<Vec<Candidate>, LlmError> {
    let Some(Value::Array(items)) =
        tool_input.and_then(|mut input| input.get_mut("candidates").map(Value::take))
    else {
        tracing::error!("Claude did not record candidates");
        return Err(LlmError::Failed);
    };
    Ok(items
        .into_iter()
        .filter_map(
            |item| match serde_json::from_value::<Candidate>(item.clone()) {
                Ok(candidate) if valid_text(&candidate.text) => Some(Candidate {
                    text: candidate.text.trim().to_owned(),
                    ..candidate
                }),
                _ => {
                    tracing::warn!(%item, "dropped a bad candidate");
                    None
                }
            },
        )
        .take(MAX_CANDIDATES)
        .collect())
}

/// The decision in the tool input. A supersede must name one of the neighbours and carry a
/// valid text; anything else is refused, so the device asks again later.
pub(crate) fn decision(
    tool_input: Option<Value>,
    neighbours: &[Neighbour],
) -> Result<ReconcileResponse, LlmError> {
    let Some(input) = tool_input else {
        return Err(LlmError::Failed);
    };
    let decision = match serde_json::from_value(input.clone()) {
        Ok(ReconcileResponse::Supersede { memory_id, text })
            if valid_text(&text)
                && neighbours
                    .iter()
                    .any(|neighbour| neighbour.memory_id == memory_id) =>
        {
            ReconcileResponse::Supersede {
                memory_id,
                text: text.trim().to_owned(),
            }
        }
        Ok(decision @ (ReconcileResponse::Add | ReconcileResponse::Skip)) => decision,
        _ => {
            tracing::error!(%input, "Claude recorded a bad decision");
            return Err(LlmError::Failed);
        }
    };
    Ok(decision)
}

pub(crate) fn valid_text(text: &str) -> bool {
    (1..=MAX_TEXT_CHARS).contains(&text.trim().chars().count())
}
