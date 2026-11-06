//! `POST /v1/devices` against DynamoDB Local and a fake GitHub.

mod common;

use axum::http::StatusCode;
use common::{ALICE, ALICE_ID, BOB, BROKEN, SLOW, Service, string};
use loremaster_core::{DeviceCredential, DeviceLimitReached, DeviceList};
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

const DEVICE_LIMIT: usize = 10;

async fn register_ok(service: &Service, github_token: &str, name: &str) -> DeviceCredential {
    let (status, body) = service.register(github_token, name).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    serde_json::from_value(body).unwrap()
}

async fn register_devices(service: &Service, count: usize) -> Vec<DeviceCredential> {
    let mut devices = Vec::new();
    for n in 0..count {
        devices.push(register_ok(service, ALICE, &format!("device {n}")).await);
    }
    devices
}

async fn replace(
    service: &Service,
    replace_device_id: &str,
    name: &str,
) -> (StatusCode, serde_json::Value) {
    let body = serde_json::json!({
        "github_token": ALICE,
        "name": name,
        "replace_device_id": replace_device_id,
    });
    service.post("/v1/devices", body.to_string()).await
}

async fn alice_device_count(service: &Service) -> u64 {
    let users = service.scan(&service.users_table).await;
    let alice = users
        .iter()
        .find(|user| string(user, "user_id") == format!("gh:{ALICE_ID}"))
        .unwrap();
    alice["device_count"].as_n().unwrap().parse().unwrap()
}

async fn alice_device_ids(service: &Service) -> Vec<String> {
    let mut ids: Vec<_> = service
        .scan(&service.devices_table)
        .await
        .iter()
        .filter(|item| string(item, "user_id") == format!("gh:{ALICE_ID}"))
        .map(|item| string(item, "device_id"))
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn eleventh_device_is_409_with_the_list() {
    let service = Service::start().await;
    let devices = register_devices(&service, DEVICE_LIMIT).await;
    assert_eq!(alice_device_count(&service).await, DEVICE_LIMIT as u64);

    let (status, body) = service.register(ALICE, "one too many").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let refused: DeviceLimitReached = serde_json::from_value(body).unwrap();
    assert!(!refused.error.is_empty());
    let listed: Vec<_> = refused
        .devices
        .iter()
        .map(|d| d.device_id.clone())
        .collect();
    let registered: Vec<_> = devices.iter().map(|d| d.device_id.clone()).collect();
    assert_eq!(listed, registered);
    let names: Vec<_> = refused.devices.iter().map(|d| d.name.clone()).collect();
    assert_eq!(names[0], "device 0");

    assert_eq!(alice_device_ids(&service).await.len(), DEVICE_LIMIT);
    assert_eq!(alice_device_count(&service).await, DEVICE_LIMIT as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_logins_stop_at_ten() {
    let service = Service::start().await;
    register_devices(&service, DEVICE_LIMIT - 1).await;

    let (first, second) = tokio::join!(
        service.register(ALICE, "racer a"),
        service.register(ALICE, "racer b")
    );
    let mut statuses = [first.0, second.0];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::CREATED, StatusCode::CONFLICT],
        "{} / {}",
        first.1,
        second.1
    );
    assert_eq!(alice_device_ids(&service).await.len(), DEVICE_LIMIT);
    assert_eq!(alice_device_count(&service).await, DEVICE_LIMIT as u64);
}

#[tokio::test]
async fn replacing_a_device_keeps_ten() {
    let service = Service::start().await;
    let devices = register_devices(&service, DEVICE_LIMIT).await;
    let old = &devices[3];

    let (status, body) = replace(&service, &old.device_id, "new laptop").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let new: DeviceCredential = serde_json::from_value(body).unwrap();

    let (status, _) = service.get("/v1/devices", &old.token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = service.get("/v1/devices", &new.token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let list: DeviceList = serde_json::from_value(body).unwrap();
    assert_eq!(list.devices.len(), DEVICE_LIMIT);
    assert!(list.devices.iter().all(|d| d.device_id != old.device_id));
    let added = list
        .devices
        .iter()
        .find(|d| d.device_id == new.device_id)
        .unwrap();
    assert_eq!(added.name, "new laptop");

    assert_eq!(alice_device_ids(&service).await.len(), DEVICE_LIMIT);
    assert_eq!(alice_device_count(&service).await, DEVICE_LIMIT as u64);
}

#[tokio::test]
async fn replacing_a_device_that_is_not_the_users_is_404() {
    let service = Service::start().await;
    register_devices(&service, 2).await;
    let bob_box = register_ok(&service, BOB, "bob box").await;
    let before = alice_device_ids(&service).await;
    let missing = format!("dev_{}", loremaster_core::new_ulid());

    for device_id in [bob_box.device_id.as_str(), missing.as_str()] {
        let (status, body) = replace(&service, device_id, "intruder").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{device_id}: {body}");
        assert!(body["error"].is_string(), "{body}");
    }

    assert_eq!(alice_device_ids(&service).await, before);
    assert_eq!(alice_device_count(&service).await, 2);
    let (status, body) = service.get("/v1/devices", &bob_box.token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["devices"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn replacing_below_the_limit_works() {
    let service = Service::start().await;
    let devices = register_devices(&service, 2).await;

    let (status, body) = replace(&service, &devices[0].device_id, "replacement").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let new: DeviceCredential = serde_json::from_value(body).unwrap();

    let mut expected = vec![devices[1].device_id.clone(), new.device_id];
    expected.sort();
    assert_eq!(alice_device_ids(&service).await, expected);
    assert_eq!(alice_device_count(&service).await, 2);
}
