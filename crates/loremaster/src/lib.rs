//! Device client for Loremaster: captures agent turns, keeps local memory, answers recall.

pub mod config;
#[cfg(feature = "dev")]
pub mod dev;
pub mod embed;
pub mod enable;
pub mod hook;
pub mod index;
pub mod login;
pub mod project;
pub mod recall;
pub mod redact;
pub mod server;
pub mod spool;
pub mod store;

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The four things a hook asks of the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    SessionStart,
    Recall,
    Capture,
    SessionEnd,
}

/// Wall clock as epoch milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}
