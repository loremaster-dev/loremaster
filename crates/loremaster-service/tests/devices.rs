//! `POST /v1/devices` against DynamoDB Local and a fake GitHub.

mod common;

use axum::http::StatusCode;
use common::{ALICE, ALICE_ID, BROKEN, SLOW, Service, string};
use sha2::{Digest, Sha256};

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

async fn assert_nothing_written(service: &Service) {
    assert!(service.scan(&service.users_table).await.is_empty());
    assert!(service.scan(&service.devices_table).await.is_empty());
}

#[tokio::test]
async fn registers_a_user_and_a_device() {
    let service = Service::start().await;
    let (status, body) = service.register(ALICE, "  work laptop ").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let device_id = body["device_id"].as_str().unwrap();
    let token = body["token"].as_str().unwrap();
    let ulid = device_id.strip_prefix("dev_").unwrap();
    assert_eq!(ulid.len(), 26);
    assert!(is_lower_hex(token, 64), "{token}");
    assert_eq!(body.as_object().unwrap().len(), 2);

    let users = service.scan(&service.users_table).await;
    assert_eq!(users.len(), 1);
    let user_id = format!("gh:{ALICE_ID}");
    assert_eq!(string(&users[0], "user_id"), user_id);
    assert_eq!(users[0]["next_seq"].as_n().unwrap(), "0");
    assert_eq!(users[0]["live_count"].as_n().unwrap(), "0");

    let devices = service.scan(&service.devices_table).await;
    assert_eq!(devices.len(), 1);
    let device = &devices[0];
    assert_eq!(string(device, "token_hash"), sha256_hex(token));
    assert_eq!(string(device, "user_id"), user_id);
    assert_eq!(string(device, "device_id"), device_id);
    assert_eq!(string(device, "name"), "work laptop");
    let created_at: u64 = device["created_at"].as_n().unwrap().parse().unwrap();
    assert!(created_at > 1_700_000_000_000);

    for item in users.iter().chain(&devices) {
        for value in item.values() {
            let text = format!("{value:?}");
            assert!(!text.contains(token), "raw token stored: {text}");
            assert!(!text.contains(ALICE), "GitHub token stored: {text}");
        }
    }
}

#[tokio::test]
async fn second_login_adds_a_device_to_the_same_user() {
    let service = Service::start().await;
    let (first_status, first) = service.register(ALICE, "laptop").await;
    let (second_status, second) = service.register(ALICE, "desktop").await;
    assert_eq!(first_status, StatusCode::CREATED);
    assert_eq!(second_status, StatusCode::CREATED);
    assert_ne!(first["device_id"], second["device_id"]);
    assert_ne!(first["token"], second["token"]);

    let users = service.scan(&service.users_table).await;
    assert_eq!(users.len(), 1);
    let devices = service.scan(&service.devices_table).await;
    assert_eq!(devices.len(), 2);
    for device in &devices {
        assert_eq!(string(device, "user_id"), format!("gh:{ALICE_ID}"));
    }
}

#[tokio::test]
async fn rejected_requests_write_nothing() {
    let service = Service::start().await;
    let long_name = "n".repeat(65);
    let bodies = [
        String::new(),
        "not json".to_owned(),
        r#"{"name":"laptop"}"#.to_owned(),
        format!(r#"{{"github_token":"{ALICE}"}}"#),
        format!(r#"{{"github_token":"{ALICE}","name":""}}"#),
        format!(r#"{{"github_token":"{ALICE}","name":"   "}}"#),
        format!(r#"{{"github_token":"{ALICE}","name":"{long_name}"}}"#),
    ];
    for body in bodies {
        let (status, reply) = service.post("/v1/devices", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(reply["error"].is_string(), "{body}: {reply}");
    }
    assert_nothing_written(&service).await;

    let (status, _) = service.register(ALICE, &"é".repeat(64)).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn bad_github_token_is_401() {
    let service = Service::start().await;
    let (status, body) = service.register("gho_unknown", "laptop").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string(), "{body}");
    assert_nothing_written(&service).await;
}

#[tokio::test]
async fn github_failure_is_502() {
    let service = Service::start().await;
    for token in [BROKEN, SLOW] {
        let (status, body) = service.register(token, "laptop").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{token}");
        assert!(body["error"].is_string(), "{body}");
    }
    assert_nothing_written(&service).await;
}

#[tokio::test]
async fn storage_failure_is_500() {
    let service = Service::without_tables().await;
    let (status, body) = service.register(ALICE, "laptop").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body["error"].is_string(), "{body}");
}
