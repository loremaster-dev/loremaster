//! `POST /v1/episode` against DynamoDB Local and a fake Messages API.

mod common;

use axum::http::StatusCode;
use common::{
    ALICE, FAIL, FAKE_INPUT_TOKENS, GARBAGE, MODEL_ID, NO_TOOL, OVERLOADED, RATE_LIMITED,
    SLOW_MODEL, Service, counter, set_counter, today,
};
use serde_json::{Value, json};

const EPISODE: &str = "/v1/episode";
const PNPM: &str = "Attempted: Switch the project to pnpm\nWorked: pnpm import\nFailed: none";

fn request(turns: &[&str]) -> Value {
    json!({ "turns": turns })
}

fn session() -> Value {
    request(&[
        r#"{"prompt":"Switch to pnpm"}"#,
        r#"{"prompt":"Run the tests"}"#,
    ])
}

async fn episode(service: &Service, token: &str, body: &Value) -> (StatusCode, Value) {
    service.post_as(EPISODE, token, body.to_string()).await
}

#[tokio::test]
async fn model_episode_is_returned() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    let (status, body) = episode(&service, &device.token, &session()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "episode": PNPM }));
    assert_eq!(counter(&service).await, Some((today(), FAKE_INPUT_TOKENS)));

    service.model.reply_next(json!({
        "task_attempted": true,
        "attempted": " Fix the flaky sync test ",
        "worked": "",
        "failed": "A longer timeout",
    }));
    let (status, body) = episode(&service, &device.token, &session()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"episode": "Attempted: Fix the flaky sync test\nWorked: none\nFailed: A longer timeout"})
    );
    assert_eq!(
        counter(&service).await,
        Some((today(), 2 * FAKE_INPUT_TOKENS))
    );
}

#[tokio::test]
async fn session_with_no_task_has_a_null_episode() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    service.model.reply_next(json!({"task_attempted": false}));
    let (status, body) = episode(&service, &device.token, &request(&["thanks!"])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "episode": null }));
    assert_eq!(counter(&service).await, Some((today(), FAKE_INPUT_TOKENS)));
}

#[tokio::test]
async fn request_carries_prompt_tool_and_turns() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    let (status, body) = episode(&service, &device.token, &session()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, request) = service.model.last_request();
    assert_eq!(request["model"], MODEL_ID);
    assert_eq!(request["max_tokens"], 1_024);
    assert!(
        request["system"]
            .as_str()
            .unwrap()
            .contains("record_episode")
    );
    assert_eq!(
        request["tool_choice"],
        json!({"type": "tool", "name": "record_episode"})
    );
    let tool = &request["tools"][0];
    assert_eq!(tool["name"], "record_episode");
    assert_eq!(tool["input_schema"]["required"], json!(["task_attempted"]));
    let properties = &tool["input_schema"]["properties"];
    assert_eq!(properties["task_attempted"]["type"], "boolean");
    for part in ["attempted", "worked", "failed"] {
        assert_eq!(properties[part]["type"], "string", "{part}");
        assert_eq!(properties[part]["maxLength"], 600, "{part}");
    }
    let content: Value =
        serde_json::from_str(request["messages"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content, session());
}

#[tokio::test]
async fn no_token_is_401() {
    let service = Service::start().await;
    let (status, _) = service.post(EPISODE, session().to_string()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(service.model.calls(), 0);
}

#[tokio::test]
async fn bad_bodies_are_400() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    let bodies = [
        json!("not a request"),
        json!({}),
        json!({"turns": "one turn"}),
        json!({"turns": [1]}),
        request(&[]),
        request(&[""]),
        request(&["Switch to pnpm", " \n\t "]),
    ];
    for body in &bodies {
        let (status, _) = episode(&service, &device.token, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    let (status, _) = service.post_as(EPISODE, &device.token, "not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(service.model.calls(), 0);
}

#[tokio::test]
async fn turns_over_100k_tokens_are_413() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    // 75,000 characters are 25,000 tokens, so four of them are exactly 100,000.
    let quarter = "x".repeat(75_000);
    let over = format!("{quarter}y");
    let (status, _) = episode(
        &service,
        &device.token,
        &request(&[&quarter, &quarter, &quarter, &over]),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(service.model.calls(), 0);

    let (status, body) = episode(
        &service,
        &device.token,
        &request(&[&quarter, &quarter, &quarter, &quarter]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(service.model.calls(), 1);
}

#[tokio::test]
async fn user_over_cap_today_is_429() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    set_counter(&service, &today(), 5_000_000).await;
    let (status, _) = episode(&service, &device.token, &session()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(service.model.calls(), 0);
}

#[tokio::test]
async fn bad_replies_are_502() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    let bad = [
        json!({"task_attempted": true, "attempted": "", "worked": "pnpm import"}),
        json!({"task_attempted": true, "attempted": " \n "}),
        json!({"task_attempted": true}),
        json!({"attempted": "Switch to pnpm"}),
        json!({"task_attempted": true, "attempted": "x".repeat(2_000)}),
    ];
    for input in &bad {
        service.model.reply_next(input.clone());
        let (status, _) = episode(&service, &device.token, &session()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{input}");
    }
    let no_tool = request(&[&format!("Switch to pnpm {NO_TOOL}")]);
    let (status, _) = episode(&service, &device.token, &no_tool).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    // Each of these model calls still used tokens.
    assert_eq!(
        counter(&service).await,
        Some((today(), (bad.len() as u64 + 1) * FAKE_INPUT_TOKENS))
    );
}

#[tokio::test]
async fn model_failures() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    let failures = [
        (RATE_LIMITED, StatusCode::SERVICE_UNAVAILABLE),
        (OVERLOADED, StatusCode::SERVICE_UNAVAILABLE),
        (FAIL, StatusCode::BAD_GATEWAY),
        (GARBAGE, StatusCode::BAD_GATEWAY),
        (SLOW_MODEL, StatusCode::BAD_GATEWAY),
    ];
    for (marker, expected) in failures {
        let body = request(&["Switch to pnpm", &format!("Run the tests {marker}")]);
        let (status, _) = episode(&service, &device.token, &body).await;
        assert_eq!(status, expected, "{marker}");
    }
    assert_eq!(service.model.calls(), failures.len());
    assert_eq!(counter(&service).await, None);
}
