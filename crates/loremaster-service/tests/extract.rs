//! `POST /v1/extract` against DynamoDB Local and a fake Messages API.

mod common;

use aws_sdk_dynamodb::types::AttributeValue;
use axum::http::StatusCode;
use common::{
    ALICE, ALICE_ID, FAIL, FAKE_INPUT_TOKENS, GARBAGE, Item, MODEL_ID, MODEL_KEY, NO_TOOL,
    OVERLOADED, RATE_LIMITED, REPLY, SLOW_MODEL, Service,
};
use loremaster_core::{Candidate, DeviceCredential, ExtractResponse, Kind, Scope};
use loremaster_service::utc_day;
use serde_json::{Value, json};

const EXTRACT: &str = "/v1/extract";

async fn device(service: &Service) -> DeviceCredential {
    let (status, body) = service.register(ALICE, "laptop").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    serde_json::from_value(body).unwrap()
}

async fn extract(service: &Service, token: &str, turn: &str) -> (StatusCode, Value) {
    let body = json!({ "turn": turn });
    service.post_as(EXTRACT, token, body.to_string()).await
}

fn candidates(body: Value) -> Vec<Candidate> {
    serde_json::from_value::<ExtractResponse>(body)
        .unwrap()
        .candidates
}

#[tokio::test]
async fn turn_gives_candidates() {
    let service = Service::start().await;
    let device = device(&service).await;
    let turn = r#"{"prompt":"I prefer pnpm over npm","tool_calls":[],"reply":"Noted."}"#;
    let (status, body) = extract(&service, &device.token, turn).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        candidates(body),
        [Candidate {
            text: "Prefers pnpm over npm".to_owned(),
            scope: Scope::User,
            kind: Kind::Preference,
        }]
    );
    let (headers, request) = service.model.last_request();
    assert_eq!(headers["x-api-key"], MODEL_KEY);
    assert_eq!(headers["anthropic-version"], "2026-10-21");
    assert_eq!(request["model"], MODEL_ID);
    assert_eq!(
        request["messages"],
        json!([{"role": "user", "content": turn}])
    );
    assert!(!request["system"].as_str().unwrap().is_empty());
    assert_eq!(
        request["tool_choice"],
        json!({"type": "tool", "name": "record_candidates"})
    );
    assert_eq!(request["tools"][0]["name"], "record_candidates");
    assert_eq!(
        request["tools"][0]["input_schema"]["properties"]["candidates"]["maxItems"],
        3
    );
}

#[tokio::test]
async fn pleasantries_give_no_candidates() {
    let service = Service::start().await;
    let device = device(&service).await;
    let turn = format!("{REPLY}{}", json!({"candidates": []}));
    let (status, body) = extract(&service, &device.token, &turn).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(candidates(body).is_empty());
}

#[tokio::test]
async fn no_token_is_401() {
    let service = Service::start().await;
    let (status, _) = service
        .post(EXTRACT, json!({"turn": "hi"}).to_string())
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(service.model.calls(), 0);
}

#[tokio::test]
async fn bad_bodies_are_400() {
    let service = Service::start().await;
    let device = device(&service).await;
    for body in [
        "not json",
        r#"{"turn": ""}"#,
        r#"{"turn": " \n\t "}"#,
        r#"{"text": "hi"}"#,
    ] {
        let (status, _) = service.post_as(EXTRACT, &device.token, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(service.model.calls(), 0);
}

const DAY_MS: u64 = 86_400_000;

fn today() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    utc_day(now.as_millis() as u64)
}

fn yesterday() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    utc_day(now.as_millis() as u64 - DAY_MS)
}

async fn user(service: &Service) -> Item {
    service
        .dynamo
        .get_item()
        .table_name(&service.users_table)
        .key("user_id", AttributeValue::S(format!("gh:{ALICE_ID}")))
        .consistent_read(true)
        .send()
        .await
        .unwrap()
        .item
        .unwrap()
}

/// The user's token count as (day, tokens), or `None` before the first count.
async fn counter(service: &Service) -> Option<(String, u64)> {
    let user = user(service).await;
    let day = user.get("tokens_day")?.as_s().unwrap().clone();
    let used = user["tokens_used"].as_n().unwrap().parse().unwrap();
    Some((day, used))
}

async fn set_counter(service: &Service, day: &str, used: u64) {
    service
        .dynamo
        .update_item()
        .table_name(&service.users_table)
        .key("user_id", AttributeValue::S(format!("gh:{ALICE_ID}")))
        .update_expression("SET tokens_day = :day, tokens_used = :used")
        .expression_attribute_values(":day", AttributeValue::S(day.to_owned()))
        .expression_attribute_values(":used", AttributeValue::N(used.to_string()))
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn turn_over_32k_tokens_is_413() {
    let service = Service::start().await;
    let device = device(&service).await;
    let (status, _) = extract(&service, &device.token, &"x".repeat(96_003)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(service.model.calls(), 0);
    let (status, body) = extract(&service, &device.token, &"x".repeat(96_000)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(service.model.calls(), 1);
}

#[tokio::test]
async fn user_over_cap_today_is_429() {
    let service = Service::start().await;
    let device = device(&service).await;
    set_counter(&service, &today(), 5_000_000).await;
    let (status, _) = extract(&service, &device.token, "turn").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(service.model.calls(), 0);
    assert_eq!(counter(&service).await, Some((today(), 5_000_000)));

    set_counter(&service, &today(), 4_999_999).await;
    let (status, body) = extract(&service, &device.token, "turn").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn new_day_resets_counter() {
    let service = Service::start().await;
    let device = device(&service).await;
    set_counter(&service, &yesterday(), 5_000_000).await;
    let (status, body) = extract(&service, &device.token, "turn").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(counter(&service).await, Some((today(), FAKE_INPUT_TOKENS)));
}

#[tokio::test]
async fn counter_adds_input_tokens() {
    let service = Service::start().await;
    let device = device(&service).await;
    assert_eq!(counter(&service).await, None);
    for _ in 0..2 {
        let (status, body) = extract(&service, &device.token, "turn").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(
        counter(&service).await,
        Some((today(), 2 * FAKE_INPUT_TOKENS))
    );
}

#[tokio::test]
async fn parallel_calls_on_a_new_day_count_every_token() {
    let service = Service::start().await;
    let device = device(&service).await;
    set_counter(&service, &yesterday(), 7).await;
    let call = || extract(&service, &device.token, "turn");
    let replies = tokio::join!(call(), call(), call(), call());
    for (status, body) in [replies.0, replies.1, replies.2, replies.3] {
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(
        counter(&service).await,
        Some((today(), 4 * FAKE_INPUT_TOKENS))
    );
}

#[tokio::test]
async fn model_failures() {
    let service = Service::start().await;
    let device = device(&service).await;
    let failures = [
        (RATE_LIMITED, StatusCode::SERVICE_UNAVAILABLE),
        (OVERLOADED, StatusCode::SERVICE_UNAVAILABLE),
        (FAIL, StatusCode::BAD_GATEWAY),
        (GARBAGE, StatusCode::BAD_GATEWAY),
        (SLOW_MODEL, StatusCode::BAD_GATEWAY),
    ];
    for (turn, expected) in failures {
        let (status, _) = extract(&service, &device.token, turn).await;
        assert_eq!(status, expected, "{turn}");
    }
    assert_eq!(service.model.calls(), failures.len());
    assert_eq!(counter(&service).await, None);
}

#[tokio::test]
async fn reply_without_tool_use_is_502() {
    let service = Service::start().await;
    let device = device(&service).await;
    let (status, _) = extract(&service, &device.token, NO_TOOL).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    // The model call still used tokens.
    assert_eq!(counter(&service).await, Some((today(), FAKE_INPUT_TOKENS)));
}

#[tokio::test]
async fn bad_candidates_are_dropped() {
    let service = Service::start().await;
    let device = device(&service).await;
    let fact = |text: &str| json!({"text": text, "scope": "project", "kind": "decision"});
    let longest = "y".repeat(2_000);
    let cases = [
        (
            json!({"candidates": [
                fact(""),
                fact("   "),
                fact(&"x".repeat(2_001)),
                {"text": "Uses tabs", "scope": "user", "kind": "habit"},
                {"text": "Uses tabs", "scope": "team", "kind": "preference"},
                {"text": "Uses tabs", "scope": "user"},
                fact(&longest),
                fact("  Uses SQLite for the queue  "),
                fact("Deploys with SAM"),
                fact("Fourth valid fact"),
            ]}),
            vec![
                longest.as_str(),
                "Uses SQLite for the queue",
                "Deploys with SAM",
            ],
        ),
        (json!({"candidates": [fact("")]}), vec![]),
    ];
    for (input, expected) in cases {
        let (status, body) = extract(&service, &device.token, &format!("{REPLY}{input}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let texts: Vec<String> = candidates(body).into_iter().map(|c| c.text).collect();
        assert_eq!(texts, expected);
    }
    // Tokens count also when no candidate is left.
    assert_eq!(
        counter(&service).await,
        Some((today(), 2 * FAKE_INPUT_TOKENS))
    );
}

#[tokio::test]
async fn tool_input_without_candidates_is_502() {
    let service = Service::start().await;
    let device = device(&service).await;
    for input in [json!({}), json!({"candidates": "none"})] {
        let (status, _) = extract(&service, &device.token, &format!("{REPLY}{input}")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{input}");
    }
}
