//! `POST /v1/reconcile` against DynamoDB Local and a fake Messages API.

mod common;

use axum::http::StatusCode;
use common::{
    ALICE, FAIL, FAKE_INPUT_TOKENS, GARBAGE, MODEL_ID, NO_TOOL, OVERLOADED, RATE_LIMITED,
    SLOW_MODEL, Service, counter, set_counter, today,
};
use serde_json::{Value, json};

const RECONCILE: &str = "/v1/reconcile";
const NPM: &str = "01KA0000000000000000000001";
const TABS: &str = "01KA0000000000000000000002";

fn request(candidate: &str, neighbours: &[(&str, &str)]) -> Value {
    json!({
        "candidate": {"text": candidate, "scope": "user", "kind": "preference"},
        "neighbours": neighbours
            .iter()
            .map(|(memory_id, text)| json!({"memory_id": memory_id, "text": text}))
            .collect::<Vec<_>>(),
    })
}

fn pnpm() -> Value {
    request(
        "Uses pnpm",
        &[(TABS, "Indents with tabs"), (NPM, "Uses npm")],
    )
}

async fn reconcile(service: &Service, token: &str, body: &Value) -> (StatusCode, Value) {
    service.post_as(RECONCILE, token, body.to_string()).await
}

#[tokio::test]
async fn model_decision_is_returned() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    let cases = [
        (json!({"decision": "add"}), json!({"decision": "add"})),
        (json!({"decision": "skip"}), json!({"decision": "skip"})),
        (
            json!({"decision": "supersede", "memory_id": NPM, "text": "  Uses pnpm, not npm \n"}),
            json!({"decision": "supersede", "memory_id": NPM, "text": "Uses pnpm, not npm"}),
        ),
    ];
    for (input, expected) in cases {
        service.model.reply_next(input);
        let (status, body) = reconcile(&service, &device.token, &pnpm()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, expected);
    }
}

#[tokio::test]
async fn request_carries_prompt_tool_and_facts() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    let (status, body) = reconcile(&service, &device.token, &pnpm()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, request) = service.model.last_request();
    assert_eq!(request["model"], MODEL_ID);
    assert!(
        request["system"]
            .as_str()
            .unwrap()
            .contains("record_decision")
    );
    assert_eq!(
        request["tool_choice"],
        json!({"type": "tool", "name": "record_decision"})
    );
    assert_eq!(request["tools"][0]["name"], "record_decision");
    assert_eq!(
        request["tools"][0]["input_schema"]["properties"]["decision"]["enum"],
        json!(["add", "supersede", "skip"])
    );
    let content: Value =
        serde_json::from_str(request["messages"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content, pnpm());
}

#[tokio::test]
async fn no_token_is_401() {
    let service = Service::start().await;
    let (status, _) = service.post(RECONCILE, pnpm().to_string()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(service.model.calls(), 0);
}

#[tokio::test]
async fn bad_bodies_are_400() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    let too_long = "x".repeat(2_001);
    let six: Vec<(String, String)> = (0..6)
        .map(|i| (format!("01KA000000000000000000000{i}"), format!("Fact {i}")))
        .collect();
    let six: Vec<(&str, &str)> = six
        .iter()
        .map(|(id, text)| (id.as_str(), text.as_str()))
        .collect();
    let mut no_scope = pnpm();
    no_scope["candidate"]
        .as_object_mut()
        .unwrap()
        .remove("scope");
    let bodies = [
        json!("not a request"),
        no_scope,
        request("", &[(NPM, "Uses npm")]),
        request(" \n ", &[(NPM, "Uses npm")]),
        request(&too_long, &[(NPM, "Uses npm")]),
        request("Uses pnpm", &[]),
        request("Uses pnpm", &six),
        request("Uses pnpm", &[(NPM, "")]),
        request("Uses pnpm", &[(NPM, &too_long)]),
        request("Uses pnpm", &[(NPM, "Uses npm"), (NPM, "Uses yarn")]),
        request("Uses pnpm", &[("01KA00000000000000000000011", "Uses npm")]),
        request("Uses pnpm", &[("npm", "Uses npm")]),
    ];
    for body in &bodies {
        let (status, _) = reconcile(&service, &device.token, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    let (status, _) = service.post_as(RECONCILE, &device.token, "not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(service.model.calls(), 0);

    let longest = "y".repeat(2_000);
    let five: Vec<(&str, &str)> = six[..5]
        .iter()
        .map(|&(id, _)| (id, longest.as_str()))
        .collect();
    let (status, body) = reconcile(&service, &device.token, &request(&longest, &five)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn user_over_cap_today_is_429() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    set_counter(&service, &today(), 5_000_000).await;
    let (status, _) = reconcile(&service, &device.token, &pnpm()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(service.model.calls(), 0);
}

#[tokio::test]
async fn counter_is_shared_with_extract() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    let (status, _) = reconcile(&service, &device.token, &pnpm()).await;
    assert_eq!(status, StatusCode::OK);
    let turn = json!({"turn": "I prefer pnpm over npm"});
    let (status, _) = service
        .post_as("/v1/extract", &device.token, turn.to_string())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        counter(&service).await,
        Some((today(), 2 * FAKE_INPUT_TOKENS))
    );
}

#[tokio::test]
async fn bad_replies_are_502() {
    let service = Service::start().await;
    let device = common::device(&service, ALICE).await;
    let supersede = |memory_id: &str, text: &str| json!({"decision": "supersede", "memory_id": memory_id, "text": text});
    let bad = [
        supersede("01KA0000000000000000000009", "Uses pnpm, not npm"),
        json!({"decision": "supersede", "text": "Uses pnpm, not npm"}),
        json!({"decision": "supersede", "memory_id": NPM}),
        supersede(NPM, ""),
        supersede(NPM, " \n\t "),
        supersede(NPM, &"x".repeat(2_001)),
        json!({"decision": "merge"}),
        json!({"memory_id": NPM, "text": "Uses pnpm"}),
    ];
    for input in &bad {
        service.model.reply_next(input.clone());
        let (status, _) = reconcile(&service, &device.token, &pnpm()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{input}");
    }
    let no_tool = request(&format!("Uses pnpm {NO_TOOL}"), &[(NPM, "Uses npm")]);
    let (status, _) = reconcile(&service, &device.token, &no_tool).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    // Each of these model calls still used tokens.
    assert_eq!(
        counter(&service).await,
        Some((today(), (bad.len() as u64 + 1) * FAKE_INPUT_TOKENS))
    );

    let longest = "y".repeat(2_000);
    service.model.reply_next(supersede(NPM, &longest));
    let (status, body) = reconcile(&service, &device.token, &pnpm()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["text"], longest);
    service
        .model
        .reply_next(json!({"decision": "add", "memory_id": NPM, "text": "Uses pnpm"}));
    let (status, body) = reconcile(&service, &device.token, &pnpm()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"decision": "add"}));
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
        let body = request(&format!("Uses pnpm {marker}"), &[(NPM, "Uses npm")]);
        let (status, _) = reconcile(&service, &device.token, &body).await;
        assert_eq!(status, expected, "{marker}");
    }
    assert_eq!(service.model.calls(), failures.len());
    assert_eq!(counter(&service).await, None);
}
