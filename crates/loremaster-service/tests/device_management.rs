//! `GET /v1/devices` and `DELETE /v1/devices/{id}` with device tokens.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{ALICE, ALICE_ID, BOB, Service, string};
use loremaster_core::{DeviceCredential, DeviceList};

async fn register(service: &Service, github_token: &str, name: &str) -> DeviceCredential {
    let (status, body) = service.register(github_token, name).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    serde_json::from_value(body).unwrap()
}

async fn list(service: &Service, token: &str) -> DeviceList {
    let (status, body) = service.get("/v1/devices", token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).unwrap()
}

fn device_path(device_id: &str) -> String {
    format!("/v1/devices/{device_id}")
}

#[tokio::test]
async fn lists_only_this_users_devices() {
    let service = Service::start().await;
    let laptop = register(&service, ALICE, "laptop").await;
    let bob_box = register(&service, BOB, "bob box").await;
    let desktop = register(&service, ALICE, " desktop ").await;

    let devices = list(&service, &desktop.token).await.devices;
    let ids: Vec<_> = devices.iter().map(|d| d.device_id.as_str()).collect();
    assert_eq!(ids, [laptop.device_id.as_str(), desktop.device_id.as_str()]);
    let names: Vec<_> = devices.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["laptop", "desktop"]);
    assert!(devices[0].created_at <= devices[1].created_at);

    let stored = service.scan(&service.devices_table).await;
    let laptop_item = stored
        .iter()
        .find(|item| string(item, "device_id") == laptop.device_id)
        .unwrap();
    let stored_at: u64 = laptop_item["created_at"].as_n().unwrap().parse().unwrap();
    assert_eq!(devices[0].created_at, stored_at);

    let bob_devices = list(&service, &bob_box.token).await.devices;
    assert_eq!(bob_devices.len(), 1);
    assert_eq!(bob_devices[0].device_id, bob_box.device_id);
}

#[tokio::test]
async fn requests_without_a_valid_token_are_401() {
    let service = Service::start().await;
    let laptop = register(&service, ALICE, "laptop").await;
    let unknown = "ab".repeat(32);
    let headers = [
        None,
        Some("Basic YWxpY2U6c2VjcmV0".to_owned()),
        Some("Bearer".to_owned()),
        Some("Bearer ".to_owned()),
        Some(format!("Bearer {unknown}")),
        Some(laptop.token.clone()),
    ];
    for value in headers {
        for request in [
            Request::get("/v1/devices"),
            Request::delete(device_path(&laptop.device_id)),
        ] {
            let request = match &value {
                Some(value) => request.header(header::AUTHORIZATION, value),
                None => request,
            };
            let (status, body) = service.send(request.body(Body::empty()).unwrap()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{value:?}");
            assert!(body["error"].is_string(), "{value:?}: {body}");
        }
    }
    assert_eq!(list(&service, &laptop.token).await.devices.len(), 1);
}

#[tokio::test]
async fn deleting_a_device_revokes_its_token() {
    let service = Service::start().await;
    let laptop = register(&service, ALICE, "laptop").await;
    let desktop = register(&service, ALICE, "desktop").await;

    let (status, body) = service
        .delete(&device_path(&laptop.device_id), &desktop.token)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_null(), "{body}");

    let (status, body) = service.get("/v1/devices", &laptop.token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string(), "{body}");

    let devices = list(&service, &desktop.token).await.devices;
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, desktop.device_id);

    let stored = service.scan(&service.devices_table).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(string(&stored[0], "user_id"), format!("gh:{ALICE_ID}"));
    assert_eq!(string(&stored[0], "device_id"), desktop.device_id);
}

#[tokio::test]
async fn a_device_can_delete_itself() {
    let service = Service::start().await;
    let laptop = register(&service, ALICE, "laptop").await;

    let (status, _) = service
        .delete(&device_path(&laptop.device_id), &laptop.token)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = service.get("/v1/devices", &laptop.token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(service.scan(&service.devices_table).await.is_empty());
}

#[tokio::test]
async fn deleting_another_users_device_is_404() {
    let service = Service::start().await;
    let laptop = register(&service, ALICE, "laptop").await;
    let bob_box = register(&service, BOB, "bob box").await;
    let missing = format!("dev_{}", loremaster_core::new_ulid());

    for device_id in [bob_box.device_id.as_str(), missing.as_str()] {
        let (status, body) = service.delete(&device_path(device_id), &laptop.token).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{device_id}");
        assert!(body["error"].is_string(), "{device_id}: {body}");
    }

    let bob_devices = list(&service, &bob_box.token).await.devices;
    assert_eq!(bob_devices.len(), 1);
    assert_eq!(bob_devices[0].device_id, bob_box.device_id);
    assert_eq!(service.scan(&service.devices_table).await.len(), 2);
}

#[tokio::test]
async fn storage_failure_on_device_routes_is_500() {
    let service = Service::without_tables().await;
    let token = "ab".repeat(32);
    for (status, body) in [
        service.get("/v1/devices", &token).await,
        service.delete(&device_path("dev_x"), &token).await,
    ] {
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body["error"].is_string(), "{body}");
    }
}

#[tokio::test]
async fn deleting_a_device_frees_a_slot() {
    let service = Service::start().await;
    let mut devices = Vec::new();
    for n in 0..10 {
        devices.push(register(&service, ALICE, &format!("device {n}")).await);
    }
    let (status, _) = service.register(ALICE, "one too many").await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = service
        .delete(&device_path(&devices[0].device_id), &devices[1].token)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let users = service.scan(&service.users_table).await;
    assert_eq!(users[0]["device_count"].as_n().unwrap(), "9");

    register(&service, ALICE, "fits again").await;
    let users = service.scan(&service.users_table).await;
    assert_eq!(users[0]["device_count"].as_n().unwrap(), "10");
}
