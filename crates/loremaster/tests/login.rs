//! `loremaster login` against a fake GitHub device flow and a fake sync service.

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Form, Json, Router};
use loremaster_core::{Device, DeviceCredential, DeviceLimitReached, RegisterDevice};
use serde_json::{Value, json};

const GITHUB_TOKEN: &str = "gho_fake_github_token_for_login_tests";

/// What the fake replies, in order, and what it was sent.
#[derive(Default)]
struct Script {
    token_replies: VecDeque<Value>,
    device_replies: VecDeque<(StatusCode, Value)>,
    device_requests: Vec<RegisterDevice>,
    device_code_requests: usize,
    /// Replaces the device code reply with this error status.
    device_code_error: Option<StatusCode>,
    /// Seconds until the device code expires. `None` gives 900.
    expires_in: Option<u64>,
}

type Shared = Arc<Mutex<Script>>;

/// A scripted GitHub token reply, a scripted service reply, whether `api_url` is set, and the
/// text the error must contain.
type FailureCase = (
    Option<Value>,
    Option<(StatusCode, Value)>,
    bool,
    &'static str,
);

async fn device_code(
    State(script): State<Shared>,
    Form(form): Form<Vec<(String, String)>>,
) -> (StatusCode, Json<Value>) {
    assert!(
        form.iter()
            .any(|(key, value)| key == "client_id" && !value.is_empty())
    );
    let mut script = script.lock().unwrap();
    script.device_code_requests += 1;
    if let Some(status) = script.device_code_error {
        return (status, Json(json!({"error": "unavailable"})));
    }
    let body = json!({
        "device_code": "dc_1",
        "user_code": "ABCD-1234",
        "verification_uri": "https://github.com/login/device",
        "expires_in": script.expires_in.unwrap_or(900),
        "interval": 0,
    });
    (StatusCode::OK, Json(body))
}

async fn access_token(
    State(script): State<Shared>,
    Form(form): Form<Vec<(String, String)>>,
) -> Json<Value> {
    let field = |name: &str| {
        form.iter()
            .find(|(key, _)| key == name)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(field("device_code").as_deref(), Some("dc_1"));
    assert_eq!(
        field("grant_type").as_deref(),
        Some("urn:ietf:params:oauth:grant-type:device_code")
    );
    let reply = script.lock().unwrap().token_replies.pop_front();
    Json(reply.unwrap_or_else(|| json!({"access_token": GITHUB_TOKEN, "token_type": "bearer"})))
}

async fn register(
    State(script): State<Shared>,
    Json(request): Json<RegisterDevice>,
) -> (StatusCode, Json<Value>) {
    let mut script = script.lock().unwrap();
    script.device_requests.push(request);
    let (status, body) = script
        .device_replies
        .pop_front()
        .expect("an unscripted device request");
    (status, Json(body))
}

/// Serves GitHub's device flow and `POST /v1/devices` on one local port.
fn start_fake(script: Script) -> (String, Shared) {
    let shared = Arc::new(Mutex::new(script));
    let app = Router::new()
        .route("/login/device/code", post(device_code))
        .route("/login/oauth/access_token", post(access_token))
        .route("/v1/devices", post(register))
        .with_state(shared.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
    });
    (url, shared)
}

fn created(device_id: &str, token: &str) -> (StatusCode, Value) {
    (
        StatusCode::CREATED,
        json!({"device_id": device_id, "token": token}),
    )
}

fn credential(device_id: &str, token: &str) -> DeviceCredential {
    DeviceCredential {
        device_id: device_id.to_owned(),
        token: token.to_owned(),
    }
}

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new(api_url: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        if let Some(url) = api_url {
            fs::write(
                dir.path().join("config.toml"),
                format!("api_url = \"{url}\"\n"),
            )
            .unwrap();
        }
        Self { dir }
    }

    fn home(&self) -> &Path {
        self.dir.path()
    }

    fn credentials(&self) -> Option<DeviceCredential> {
        let text = fs::read_to_string(self.home().join("credentials")).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }

    fn save(&self, saved: &DeviceCredential) {
        fs::write(
            self.home().join("credentials"),
            serde_json::to_string(saved).unwrap(),
        )
        .unwrap();
    }

    fn login(&self, github_url: &str, stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_loremaster"))
            .arg("login")
            .env("LOREMASTER_HOME", self.home())
            .env("LOREMASTER_GITHUB_URL", github_url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn ten_devices() -> Vec<Device> {
    (1..=10)
        .map(|n| Device {
            device_id: format!("dev_{n:02}"),
            name: format!("box {n}"),
            created_at: 1_793_000_000_000 + n,
        })
        .collect()
}

fn full_account() -> (StatusCode, Value) {
    let body = DeviceLimitReached {
        error: "device limit reached".to_owned(),
        devices: ten_devices(),
    };
    (StatusCode::CONFLICT, serde_json::to_value(body).unwrap())
}

#[test]
fn login_saves_a_private_credential() {
    let (url, script) = start_fake(Script {
        device_replies: [created("dev_new", &"ab".repeat(32))].into(),
        ..Script::default()
    });
    let sandbox = Sandbox::new(Some(&url));
    let output = sandbox.login(&url, "");
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    assert!(output.status.success(), "{stdout}{stderr}");
    assert!(
        stdout.contains("Open https://github.com/login/device and enter code ABCD-1234"),
        "{stdout}"
    );
    assert!(
        stdout.contains("After you approve, check this terminal for the result."),
        "{stdout}"
    );
    assert!(stdout.contains("(dev_new)"), "{stdout}");

    assert_eq!(
        sandbox.credentials(),
        Some(credential("dev_new", &"ab".repeat(32)))
    );
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&sandbox.home().join("credentials")), 0o600);

    let script = script.lock().unwrap();
    let sent = &script.device_requests[0];
    assert_eq!(sent.github_token, GITHUB_TOKEN);
    assert_eq!(sent.replace_device_id, None);
    assert!(
        !sent.name.is_empty() && sent.name.chars().count() <= 64,
        "{:?}",
        sent.name
    );

    assert!(!stdout.contains(GITHUB_TOKEN) && !stderr.contains(GITHUB_TOKEN));
    assert!(!stdout.contains(&"ab".repeat(32)), "device token printed");
    for entry in fs::read_dir(sandbox.home()).unwrap() {
        let path = entry.unwrap().path();
        let content = fs::read_to_string(&path).unwrap_or_default();
        assert!(
            !content.contains("gho_"),
            "GitHub token in {}",
            path.display()
        );
    }
}

#[test]
fn api_url_with_a_trailing_slash_works() {
    let (url, script) = start_fake(Script {
        device_replies: [created("dev_new", "t1")].into(),
        ..Script::default()
    });
    let sandbox = Sandbox::new(Some(&format!("{url}/")));
    let output = sandbox.login(&url, "");
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(script.lock().unwrap().device_requests.len(), 1);
    assert_eq!(sandbox.credentials(), Some(credential("dev_new", "t1")));
}

#[test]
fn waits_while_authorization_is_pending() {
    let pending = json!({"error": "authorization_pending"});
    let (url, _) = start_fake(Script {
        token_replies: [pending.clone(), pending].into(),
        device_replies: [created("dev_new", "t1")].into(),
        ..Script::default()
    });
    let sandbox = Sandbox::new(Some(&url));
    let output = sandbox.login(&url, "");
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(sandbox.credentials(), Some(credential("dev_new", "t1")));
}

#[test]
fn failed_logins_write_nothing() {
    let cases: [FailureCase; 5] = [
        (None, None, false, "no api_url"),
        (
            Some(json!({"error": "access_denied"})),
            None,
            true,
            "access_denied",
        ),
        (
            Some(json!({"error": "expired_token"})),
            None,
            true,
            "expired",
        ),
        (
            None,
            Some((
                StatusCode::UNAUTHORIZED,
                json!({"error": "GitHub rejected the token"}),
            )),
            true,
            "GitHub rejected the token",
        ),
        (
            None,
            Some((
                StatusCode::BAD_GATEWAY,
                json!({"error": "GitHub is unavailable"}),
            )),
            true,
            "GitHub is unavailable",
        ),
    ];
    for (token_reply, device_reply, with_api_url, expected) in cases {
        let (url, _) = start_fake(Script {
            token_replies: token_reply.into_iter().collect(),
            device_replies: device_reply.into_iter().collect(),
            ..Script::default()
        });
        let sandbox = Sandbox::new(with_api_url.then_some(url.as_str()));
        let output = sandbox.login(&url, "");
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{expected}: {stderr}");
        assert!(stderr.starts_with("error: "), "{expected}: {stderr}");
        assert!(stderr.contains(expected), "{expected}: {stderr}");
        assert_eq!(stderr.trim().lines().count(), 1, "{stderr}");
        assert_eq!(sandbox.credentials(), None, "{expected}");
    }
}

#[test]
fn second_login_replaces_this_machines_device() {
    let (url, script) = start_fake(Script {
        device_replies: [created("dev_new", "t2")].into(),
        ..Script::default()
    });
    let sandbox = Sandbox::new(Some(&url));
    sandbox.save(&credential("dev_old", "t1"));
    let output = sandbox.login(&url, "");
    assert!(output.status.success(), "{}", text(&output.stderr));
    let sent = &script.lock().unwrap().device_requests;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].replace_device_id.as_deref(), Some("dev_old"));
    assert_eq!(sandbox.credentials(), Some(credential("dev_new", "t2")));
}

#[test]
fn deleted_device_registers_again() {
    let (url, script) = start_fake(Script {
        device_replies: [
            (StatusCode::NOT_FOUND, json!({"error": "no such device"})),
            created("dev_new", "t2"),
        ]
        .into(),
        ..Script::default()
    });
    let sandbox = Sandbox::new(Some(&url));
    sandbox.save(&credential("dev_gone", "t1"));
    let output = sandbox.login(&url, "");
    assert!(output.status.success(), "{}", text(&output.stderr));
    let script = script.lock().unwrap();
    let replaced: Vec<_> = script
        .device_requests
        .iter()
        .map(|r| r.replace_device_id.as_deref())
        .collect();
    assert_eq!(replaced, [Some("dev_gone"), None]);
    assert_eq!(
        script.device_code_requests, 1,
        "one GitHub login for both requests"
    );
    assert_eq!(sandbox.credentials(), Some(credential("dev_new", "t2")));
}

#[test]
fn full_account_replaces_the_chosen_device() {
    let (url, script) = start_fake(Script {
        device_replies: [full_account(), created("dev_new", "t2")].into(),
        ..Script::default()
    });
    let sandbox = Sandbox::new(Some(&url));
    let output = sandbox.login(&url, "3\n");
    let stdout = text(&output.stdout);
    assert!(output.status.success(), "{stdout}{}", text(&output.stderr));
    for n in 1..=10 {
        assert!(stdout.contains(&format!("box {n}")), "{stdout}");
    }
    assert!(stdout.contains("2026-10-26"), "UTC date added: {stdout}");
    assert!(stdout.contains("Replace which device? [1-10]:"), "{stdout}");

    let script = script.lock().unwrap();
    let sent = &script.device_requests;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].replace_device_id.as_deref(), Some("dev_03"));
    assert_eq!(sent[1].github_token, GITHUB_TOKEN);
    assert_eq!(sandbox.credentials(), Some(credential("dev_new", "t2")));
}

#[test]
fn full_account_with_no_valid_choice_changes_nothing() {
    for answer in ["", "\n", "0\n", "11\n", "abc\n"] {
        let (url, script) = start_fake(Script {
            device_replies: [full_account()].into(),
            ..Script::default()
        });
        let sandbox = Sandbox::new(Some(&url));
        let output = sandbox.login(&url, answer);
        assert_eq!(output.status.code(), Some(1), "{answer:?}");
        assert!(text(&output.stderr).starts_with("error: "), "{answer:?}");
        assert_eq!(
            script.lock().unwrap().device_requests.len(),
            1,
            "{answer:?}"
        );
        assert_eq!(sandbox.credentials(), None, "{answer:?}");
    }
}

#[test]
fn broken_replies_write_nothing() {
    let cases = [
        (
            Script {
                device_code_error: Some(StatusCode::SERVICE_UNAVAILABLE),
                ..Script::default()
            },
            "HTTP 503",
        ),
        (
            Script {
                expires_in: Some(0),
                ..Script::default()
            },
            "expired",
        ),
        (
            Script {
                device_replies: [(StatusCode::CREATED, json!({"unexpected": true}))].into(),
                ..Script::default()
            },
            "unexpected reply",
        ),
        (
            Script {
                device_replies: [(StatusCode::NOT_FOUND, json!({"error": "no such device"}))]
                    .into(),
                ..Script::default()
            },
            "404 for a new device",
        ),
    ];
    for (script, expected) in cases {
        let (url, _) = start_fake(script);
        let sandbox = Sandbox::new(Some(&url));
        let output = sandbox.login(&url, "");
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{expected}: {stderr}");
        assert!(
            stderr.starts_with("error: ") && stderr.contains(expected),
            "{expected}: {stderr}"
        );
        assert_eq!(sandbox.credentials(), None, "{expected}");
    }

    let (github_url, _) = start_fake(Script::default());
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unreachable = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let sandbox = Sandbox::new(Some(&unreachable));
    let output = sandbox.login(&github_url, "");
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("cannot reach"), "{stderr}");
    assert_eq!(sandbox.credentials(), None);
}
