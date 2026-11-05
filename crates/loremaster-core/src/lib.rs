//! Types and clocks shared by the device client and the sync service.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Rejected hybrid logical clock string.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid hybrid logical clock: {0}")]
pub struct InvalidHlc(pub String);

const MAX_COUNTER: u32 = 99_999;

/// Hybrid logical clock. Its string form `<wall_ms:13>-<counter:5>-<writer>` sorts in clock order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hlc {
    wall_ms: u64,
    counter: u32,
    writer: String,
}

impl Hlc {
    /// Creates a clock for `writer` that has not ticked yet.
    pub fn new(writer: &str) -> Self {
        Self {
            wall_ms: 0,
            counter: 0,
            writer: writer.to_owned(),
        }
    }

    /// Parses the fixed-width string form.
    pub fn parse(value: &str) -> Result<Self, InvalidHlc> {
        let invalid = || InvalidHlc(value.to_owned());
        let mut parts = value.splitn(3, '-');
        let (wall, counter, writer) = (
            parts.next().ok_or_else(invalid)?,
            parts.next().ok_or_else(invalid)?,
            parts.next().ok_or_else(invalid)?,
        );
        let digits = |text: &str, width: usize| {
            text.len() == width && text.bytes().all(|byte| byte.is_ascii_digit())
        };
        if !digits(wall, 13) || !digits(counter, 5) || writer.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            wall_ms: wall.parse().map_err(|_| invalid())?,
            counter: counter.parse().map_err(|_| invalid())?,
            writer: writer.to_owned(),
        })
    }

    /// Advances for a local change at wall clock `now_ms` and returns the new stamp.
    pub fn tick(&mut self, now_ms: u64) -> String {
        if now_ms > self.wall_ms {
            self.wall_ms = now_ms;
            self.counter = 0;
        } else if self.counter == MAX_COUNTER {
            self.wall_ms += 1;
            self.counter = 0;
        } else {
            self.counter += 1;
        }
        format!("{:013}-{:05}-{}", self.wall_ms, self.counter, self.writer)
    }

    /// Moves the clock past a stamp seen from another writer, so later ticks sort after it.
    pub fn observe(&mut self, remote: &str) -> Result<(), InvalidHlc> {
        let remote = Self::parse(remote)?;
        if (remote.wall_ms, remote.counter) > (self.wall_ms, self.counter) {
            self.wall_ms = remote.wall_ms;
            self.counter = remote.counter;
        }
        Ok(())
    }
}

/// Generates a new time-ordered memory id. Ids made in sequence sort in sequence.
pub fn new_ulid() -> String {
    static GENERATOR: std::sync::Mutex<ulid::Generator> =
        std::sync::Mutex::new(ulid::Generator::new());
    let mut generator = GENERATOR.lock().unwrap_or_else(|e| e.into_inner());
    generator
        .generate()
        .unwrap_or_else(|overflow| overflow.commit_overflow_increment())
        .to_string()
}

/// Estimates tokens as `ceil(chars / 3)`. No tokenizer matches every agent, and English averages
/// about 4 characters per token, so this overcounts and token caps hold for all of them.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(3)
}

/// Fact or session summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordType {
    Fact,
    Episode,
}

impl RecordType {
    /// Lowercase name, as stored.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Episode => "episode",
        }
    }
}

/// What a fact is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Preference,
    Decision,
    Correction,
    Environment,
    Pitfall,
}

impl Kind {
    /// Lowercase name, as stored and as shown to agents.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preference => "preference",
            Self::Decision => "decision",
            Self::Correction => "correction",
            Self::Environment => "environment",
            Self::Pitfall => "pitfall",
        }
    }
}

/// Where a memory is recalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    User,
    Project,
}

impl Scope {
    /// Lowercase name, as stored.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// Latest state of one memory. A tombstone keeps only ids, `seq`, `hlc`, `writer` and `deleted`,
/// so every content field is optional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub memory_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    pub hlc: String,
    pub writer: String,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub record_type: Option<RecordType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evict_key: Option<String>,
}

/// Body of `POST /v1/devices`: a GitHub token that proves the account, and a name for the device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterDevice {
    pub github_token: String,
    pub name: String,
}

/// Reply to a device registration. `token` is shown once; the service keeps only its hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCredential {
    pub device_id: String,
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_in_one_millisecond_increase_the_counter() {
        let mut hlc = Hlc::new("dev_a1b2");
        assert_eq!(hlc.tick(1761750000123), "1761750000123-00000-dev_a1b2");
        assert_eq!(hlc.tick(1761750000123), "1761750000123-00001-dev_a1b2");
    }

    #[test]
    fn tick_after_observe_sorts_after_the_remote_stamp() {
        let mut hlc = Hlc::new("dev_a1b2");
        hlc.tick(1761750000123);
        hlc.observe("1761750000999-00003-dev_x9").unwrap();
        assert_eq!(hlc.tick(1761750000123), "1761750000999-00004-dev_a1b2");
    }

    #[test]
    fn observing_an_older_stamp_changes_nothing() {
        let mut hlc = Hlc::new("dev_a1b2");
        hlc.tick(1761750000999);
        hlc.observe("1761750000123-00003-dev_x9").unwrap();
        assert_eq!(hlc.tick(1761750000999), "1761750000999-00001-dev_a1b2");
    }

    #[test]
    fn clock_moving_back_never_lowers_the_stamp() {
        let mut hlc = Hlc::new("dev_a1b2");
        hlc.observe("1761750000999-00004-dev_x9").unwrap();
        assert_eq!(hlc.tick(1761749000000), "1761750000999-00005-dev_a1b2");
    }

    #[test]
    fn counter_overflow_moves_to_the_next_millisecond() {
        let mut hlc = Hlc::new("w");
        hlc.observe("1761750000999-99999-x").unwrap();
        assert_eq!(hlc.tick(1761750000999), "1761750001000-00000-w");
    }

    #[test]
    fn string_order_is_clock_order() {
        assert!("1761750000999-00004-dev_a1b2" > "1761750000123-00001-dev_a1b2");
        assert!("1761750000123-00010-dev_a" > "1761750000123-00009-dev_b");
    }

    #[test]
    fn parse_rejects_malformed_stamps() {
        for bad in [
            "garbage",
            "123-1-dev_a",
            "1761750000123-00001",
            "1761750000123-00001-",
            "176175000012x-00001-w",
            "+176175000012-00001-w",
            "1761750000123-+0001-w",
            "1761750000123-0000x-w",
        ] {
            assert_eq!(Hlc::parse(bad), Err(InvalidHlc(bad.to_owned())), "{bad}");
        }
    }

    #[test]
    fn observe_rejects_a_malformed_stamp() {
        assert!(Hlc::new("w").observe("garbage").is_err());
    }

    #[test]
    fn writer_may_contain_a_dash() {
        let parsed = Hlc::parse("1761750000123-00001-dev-a").unwrap();
        assert_eq!(parsed.writer, "dev-a");
    }

    #[test]
    fn ulids_are_crockford_and_sort_in_sequence() {
        let (first, second) = (new_ulid(), new_ulid());
        assert_eq!(first.len(), 26);
        assert!(
            first
                .chars()
                .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c))
        );
        assert!(second > first);
    }

    #[test]
    fn token_estimate_rounds_up_by_characters() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 2);
        assert_eq!(estimate_tokens(&"x".repeat(300)), 100);
        assert_eq!(estimate_tokens("héllo"), 2);
    }

    #[test]
    fn names_match_their_serialized_form() {
        for (value, name) in [
            (
                serde_json::to_value(RecordType::Fact),
                RecordType::Fact.as_str(),
            ),
            (
                serde_json::to_value(RecordType::Episode),
                RecordType::Episode.as_str(),
            ),
            (serde_json::to_value(Scope::User), Scope::User.as_str()),
            (
                serde_json::to_value(Scope::Project),
                Scope::Project.as_str(),
            ),
        ] {
            assert_eq!(value.unwrap(), name);
        }

        for kind in [
            Kind::Preference,
            Kind::Decision,
            Kind::Correction,
            Kind::Environment,
            Kind::Pitfall,
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap().as_str().unwrap(),
                kind.as_str()
            );
        }
    }

    const STORED_EXAMPLE: &str = r#"{
        "user_id": "u_7f3c...",
        "memory_id": "01JBQ8Z6K4M2T9X0R5N7C3V1HD",
        "seq": 4182,
        "hlc": "1761750000123-00002-dev_a1b2",
        "writer": "dev_a1b2",
        "type": "fact",
        "kind": "preference",
        "scope": "user",
        "text": "Prefers pnpm over npm",
        "created_at": 1761750000123,
        "evict_key": "3#1761750000123"
    }"#;

    #[test]
    fn record_round_trips() {
        let record: Record = serde_json::from_str(STORED_EXAMPLE).unwrap();
        assert_eq!(record.kind, Some(Kind::Preference));
        assert_eq!(record.record_type, Some(RecordType::Fact));
        assert_eq!(record.scope, Some(Scope::User));
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), record);
    }

    #[test]
    fn tombstone_json_omits_content_fields() {
        let tombstone: Record = serde_json::from_str(
            r#"{"memory_id":"m1","seq":9,"hlc":"1761750000123-00002-w","writer":"w","deleted":true}"#,
        )
        .unwrap();
        let value = serde_json::to_value(&tombstone).unwrap();
        for absent in ["text", "kind", "scope", "project_id", "superseded_by"] {
            assert!(value.get(absent).is_none(), "{absent}");
        }
        assert_eq!(value["deleted"], true);
    }

    #[test]
    fn record_rejects_unknown_type_and_missing_id() {
        let unknown_type = STORED_EXAMPLE.replace(r#""type": "fact""#, r#""type": "note""#);
        assert!(serde_json::from_str::<Record>(&unknown_type).is_err());
        assert!(
            serde_json::from_str::<Record>(r#"{"hlc":"1761750000123-00002-w","writer":"w"}"#)
                .is_err()
        );
    }

    #[test]
    fn device_api_bodies_use_the_wire_field_names() {
        let request: RegisterDevice =
            serde_json::from_str(r#"{"github_token":"gho_x","name":"laptop"}"#).unwrap();
        assert_eq!(
            request,
            RegisterDevice {
                github_token: "gho_x".to_owned(),
                name: "laptop".to_owned(),
            }
        );
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({"github_token": "gho_x", "name": "laptop"})
        );
        let credential = DeviceCredential {
            device_id: "dev_01JBQ8Z6K4M2T9X0R5N7C3V1HD".to_owned(),
            token: "ab".repeat(32),
        };
        let json = serde_json::to_value(&credential).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"device_id": "dev_01JBQ8Z6K4M2T9X0R5N7C3V1HD", "token": "ab".repeat(32)})
        );
        assert_eq!(
            serde_json::from_value::<DeviceCredential>(json).unwrap(),
            credential
        );
    }
}
