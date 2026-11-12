//! `loremaster status`: whether sync is on and how much memory this device can recall.

use thiserror::Error;

use crate::config::{self, ConfigError, Paths};
use crate::embed::MODEL_ID;
use crate::store::{Progress, Store, StoreError};

#[derive(Debug, Error)]
pub enum StatusError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The status report, one line for sync, memories and the outbox. Reads the store directly, so it
/// works while the daemon is stopped.
pub fn report(paths: &Paths) -> Result<String, StatusError> {
    let sync = match (
        config::service_url(&paths.config_toml())?,
        config::load_credential(&paths.credentials()),
    ) {
        (None, _) => "off: no api_url".to_owned(),
        (Some(_), None) => "off: no login".to_owned(),
        (Some(_), Some(credential)) => format!("on, device {}", credential.device_id),
    };
    // A home that has never run keeps nothing yet; opening the store would create it.
    let progress = if paths.store_db().exists() {
        Store::open(&paths.store_db())?.progress(MODEL_ID)?
    } else {
        Progress {
            recallable: 0,
            total: None,
            outbox: 0,
        }
    };
    let memories = match progress.total {
        Some(total) => format!(
            "{} of {} recallable",
            thousands(progress.recallable),
            thousands(total)
        ),
        None => format!("{} recallable", thousands(progress.recallable)),
    };
    Ok(format!(
        "Sync: {sync}\nMemories: {memories}\nOutbox: {} waiting",
        thousands(progress.outbox)
    ))
}

/// Writes a count with a comma between each group of three digits.
fn thousands(count: i64) -> String {
    let digits = count.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if count < 0 {
        grouped.push('-');
    }
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn counts_have_a_comma_between_groups_of_three() {
        for (count, text) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (50_000, "50,000"),
            (1_234_567, "1,234,567"),
            (-1_234, "-1,234"),
        ] {
            assert_eq!(thousands(count), text);
        }
    }

    fn home() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        (dir, paths)
    }

    fn sync_line(paths: &Paths) -> String {
        report(paths).unwrap().lines().next().unwrap().to_owned()
    }

    #[test]
    fn sync_line_names_the_device_or_what_is_missing() {
        let (_dir, paths) = home();
        assert_eq!(sync_line(&paths), "Sync: off: no api_url");
        fs::write(
            paths.credentials(),
            r#"{"device_id":"dev_01ABC","token":"t"}"#,
        )
        .unwrap();
        assert_eq!(sync_line(&paths), "Sync: off: no api_url");
        fs::write(paths.config_toml(), "api_url = \"https://api.example\"\n").unwrap();
        assert_eq!(sync_line(&paths), "Sync: on, device dev_01ABC");
        fs::remove_file(paths.credentials()).unwrap();
        assert_eq!(sync_line(&paths), "Sync: off: no login");
    }

    #[test]
    fn a_home_that_never_ran_has_nothing_and_keeps_no_store() {
        let (_dir, paths) = home();
        assert_eq!(
            report(&paths).unwrap(),
            "Sync: off: no api_url\nMemories: 0 recallable\nOutbox: 0 waiting"
        );
        assert!(!paths.store_db().exists());
    }

    #[test]
    fn a_bad_config_is_an_error() {
        let (_dir, paths) = home();
        fs::write(paths.config_toml(), "api_url = 1\n").unwrap();
        assert!(matches!(report(&paths), Err(StatusError::Config(_))));
    }
}
