//! A sync service in memory: per-user `seq`, newer `hlc` wins, pages of a fixed size. Replies can
//! be scripted to test failures.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use loremaster_core::{DeviceCredential, PullResponse, PushRequest, PushResponse, Record};
use serde_json::{Value, json};

/// A sync route, for scripted replies and request counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Route {
    Push,
    Pull,
}

#[derive(Default)]
struct UserData {
    next_seq: i64,
    records: HashMap<String, Record>,
}

#[derive(Default)]
struct FakeState {
    /// Token to (user, device id).
    devices: HashMap<String, (String, String)>,
    users: HashMap<String, UserData>,
    page_size: usize,
    scripted: HashMap<Route, VecDeque<(StatusCode, Value)>>,
    requests: HashMap<Route, usize>,
    pushes: Vec<Value>,
}

type Shared = Arc<Mutex<FakeState>>;

pub struct FakeService {
    pub url: String,
    state: Shared,
}

impl FakeService {
    /// Serves on a local port, `page_size` records to a pull page.
    pub fn start(page_size: usize) -> Self {
        let state = Arc::new(Mutex::new(FakeState {
            page_size,
            ..FakeState::default()
        }));
        let app = Router::new()
            .route("/v1/sync/push", post(push))
            .route("/v1/sync/pull", get(pull))
            .with_state(state.clone());
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
        Self { url, state }
    }

    /// Registers a device of `user` and returns its credential.
    pub fn add_device(&self, user: &str) -> DeviceCredential {
        let credential = DeviceCredential {
            device_id: format!("dev_{}", loremaster_core::new_ulid()),
            token: loremaster_core::new_ulid(),
        };
        self.state.lock().unwrap().devices.insert(
            credential.token.clone(),
            (user.to_owned(), credential.device_id.clone()),
        );
        credential
    }

    /// Gives an existing device a new token, as a second login of the same device does.
    pub fn new_token(&self, credential: &DeviceCredential) -> DeviceCredential {
        let mut state = self.state.lock().unwrap();
        let owner = state.devices[&credential.token].clone();
        let renewed = DeviceCredential {
            device_id: credential.device_id.clone(),
            token: loremaster_core::new_ulid(),
        };
        state.devices.insert(renewed.token.clone(), owner);
        renewed
    }

    /// Makes the next request to `route` get this reply instead of the normal one.
    pub fn script(&self, route: Route, status: StatusCode, body: Value) {
        let mut state = self.state.lock().unwrap();
        state
            .scripted
            .entry(route)
            .or_default()
            .push_back((status, body));
    }

    /// The user's stored records, in `seq` order.
    pub fn records(&self, user: &str) -> Vec<Record> {
        let state = self.state.lock().unwrap();
        let mut records: Vec<Record> = state
            .users
            .get(user)
            .map(|data| data.records.values().cloned().collect())
            .unwrap_or_default();
        records.sort_by_key(|record| record.seq);
        records
    }

    /// Requests made to `route`, scripted ones included.
    pub fn requests(&self, route: Route) -> usize {
        self.state
            .lock()
            .unwrap()
            .requests
            .get(&route)
            .copied()
            .unwrap_or(0)
    }

    /// Push bodies as sent.
    pub fn pushes(&self) -> Vec<Value> {
        self.state.lock().unwrap().pushes.clone()
    }

    /// Stores records as if a device of `user` had pushed them.
    pub fn insert(&self, user: &str, records: &[Record]) {
        let mut state = self.state.lock().unwrap();
        let data = state.users.entry(user.to_owned()).or_default();
        for record in records {
            store(data, user, record.clone());
        }
    }
}

fn store(data: &mut UserData, user: &str, mut record: Record) -> bool {
    if let Some(stored) = data.records.get(&record.memory_id)
        && stored.hlc >= record.hlc
    {
        return false;
    }
    data.next_seq += 1;
    record.seq = Some(data.next_seq);
    record.user_id = Some(user.to_owned());
    data.records.insert(record.memory_id.clone(), record);
    true
}

/// Counts the request, checks the token and gives a scripted reply if there is one.
fn begin(
    state: &mut FakeState,
    route: Route,
    headers: &HeaderMap,
) -> Result<(String, String), (StatusCode, Json<Value>)> {
    *state.requests.entry(route).or_default() += 1;
    if let Some((status, body)) = state.scripted.get_mut(&route).and_then(VecDeque::pop_front) {
        return Err((status, Json(body)));
    }
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    token
        .and_then(|token| state.devices.get(token).cloned())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "missing or unknown device token"})),
        ))
}

async fn push(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    let (user, device_id) = match begin(&mut state, Route::Push, &headers) {
        Ok(caller) => caller,
        Err(reply) => return reply,
    };
    state.pushes.push(body.clone());
    let request: PushRequest = serde_json::from_value(body).unwrap();
    assert!(request.records.len() <= 99, "a push has at most 99 records");
    if let Some(bad) = request.records.iter().find(|r| r.writer != device_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("record {}: writer is not this device", bad.memory_id)})),
        );
    }
    let data = state.users.entry(user.clone()).or_default();
    let applied = request
        .records
        .into_iter()
        .filter_map(|record| {
            let id = record.memory_id.clone();
            store(data, &user, record).then_some(id)
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::to_value(PushResponse { applied }).unwrap()),
    )
}

async fn pull(
    State(state): State<Shared>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    let (user, _) = match begin(&mut state, Route::Pull, &headers) {
        Ok(caller) => caller,
        Err(reply) => return reply,
    };
    let cursor: i64 = query
        .as_deref()
        .and_then(|q| q.strip_prefix("cursor="))
        .and_then(|value| value.parse().ok())
        .expect("a pull has a cursor");
    let page_size = state.page_size;
    let data = state.users.entry(user).or_default();
    let mut after: Vec<&Record> = data
        .records
        .values()
        .filter(|record| record.seq > Some(cursor))
        .collect();
    after.sort_by_key(|record| record.seq);
    let more = after.len() > page_size;
    let records: Vec<Record> = after.into_iter().take(page_size).cloned().collect();
    let page = PullResponse {
        next_cursor: records.last().and_then(|r| r.seq).unwrap_or(cursor),
        total: data.records.values().filter(|r| !r.deleted).count() as i64,
        more,
        records,
    };
    (StatusCode::OK, Json(serde_json::to_value(page).unwrap()))
}
