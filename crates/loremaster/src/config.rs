//! File locations and user configuration.

use std::ffi::OsString;
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use loremaster_core::DeviceCredential;
pub use loremaster_core::estimate_tokens;
use serde::Deserialize;
use thiserror::Error;

/// Why configuration could not be resolved or read.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("neither LOREMASTER_HOME nor HOME is set")]
    NoHomeDir,
    #[error("config.toml line {line}: {message}")]
    ConfigParse { line: usize, message: String },
    #[error("cannot read config.toml: {0}")]
    Read(#[from] io::Error),
}

/// Locations of everything Loremaster keeps on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    home: PathBuf,
}

impl Paths {
    /// Uses `home` as the Loremaster home directory.
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// Resolves the home from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::resolve(
            std::env::var_os("LOREMASTER_HOME"),
            std::env::var_os("HOME"),
        )
    }

    /// `LOREMASTER_HOME` when set, so tests can run in a sandbox, else `$HOME/.loremaster`.
    pub fn resolve(
        loremaster_home: Option<OsString>,
        home: Option<OsString>,
    ) -> Result<Self, ConfigError> {
        let home = loremaster_home
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.map(|h| Path::new(&h).join(".loremaster")))
            .ok_or(ConfigError::NoHomeDir)?;
        // Hook commands run from any directory, so a relative home would point somewhere else.
        Ok(Self::new(std::path::absolute(home)?))
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn sock(&self) -> PathBuf {
        self.home.join("sock")
    }

    pub fn store_db(&self) -> PathBuf {
        self.home.join("store.db")
    }

    pub fn spool(&self) -> PathBuf {
        self.home.join("spool")
    }

    pub fn config_toml(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    pub fn models(&self) -> PathBuf {
        self.home.join("models")
    }

    pub fn logs(&self) -> PathBuf {
        self.home.join("logs")
    }

    /// This device's service credential, readable only by its owner.
    pub fn credentials(&self) -> PathBuf {
        self.home.join("credentials")
    }
}

/// Creates a directory only its owner can read, with any missing parents. Memories and turns live
/// under it, so other local users must not see them.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

#[derive(Default, Deserialize)]
struct ConfigFile {
    api_url: Option<String>,
    #[serde(default)]
    redact: RedactSection,
}

#[derive(Default, Deserialize)]
struct RedactSection {
    #[serde(default)]
    patterns: Vec<String>,
}

/// Reads the user's secret patterns. A missing file means none.
pub fn load_patterns(path: &Path) -> Result<Vec<String>, ConfigError> {
    Ok(load(path)?.redact.patterns)
}

/// Reads the sync service URL, if the user set one. A missing file means none.
pub fn load_api_url(path: &Path) -> Result<Option<String>, ConfigError> {
    Ok(load(path)?.api_url)
}

/// The sync service URL with no trailing slash: `api_url` from `path`, else the one this binary
/// was built with.
pub fn service_url(path: &Path) -> Result<Option<String>, ConfigError> {
    Ok(load_api_url(path)?
        .or_else(|| option_env!("LOREMASTER_API_URL").map(str::to_owned))
        .map(|url| url.trim_end_matches('/').to_owned()))
}

/// Reads the device credential saved by login. Missing or unreadable means none.
pub fn load_credential(path: &Path) -> Option<DeviceCredential> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn load(path: &Path) -> Result<ConfigFile, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ConfigFile::default()),
        Err(e) => return Err(e.into()),
    };
    toml::from_str(&text).map_err(|e| {
        let start = e.span().map_or(0, |span| span.start);
        ConfigError::ConfigParse {
            line: text[..start].matches('\n').count() + 1,
            message: e.message().to_owned(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_defaults_under_the_user_home() {
        let paths = Paths::resolve(None, Some("/Users/x".into())).unwrap();
        assert_eq!(paths.home(), Path::new("/Users/x/.loremaster"));
        assert_eq!(paths.sock(), Path::new("/Users/x/.loremaster/sock"));
    }

    #[test]
    fn loremaster_home_overrides_the_user_home() {
        let paths = Paths::resolve(Some("/tmp/lm".into()), Some("/Users/x".into())).unwrap();
        assert_eq!(paths.store_db(), Path::new("/tmp/lm/store.db"));
        assert_eq!(paths.spool(), Path::new("/tmp/lm/spool"));
        assert_eq!(paths.config_toml(), Path::new("/tmp/lm/config.toml"));
        assert_eq!(paths.models(), Path::new("/tmp/lm/models"));
        assert_eq!(paths.logs(), Path::new("/tmp/lm/logs"));
        assert_eq!(paths.credentials(), Path::new("/tmp/lm/credentials"));
    }

    #[test]
    fn empty_loremaster_home_counts_as_unset() {
        let paths = Paths::resolve(Some("".into()), Some("/Users/x".into())).unwrap();
        assert_eq!(paths.home(), Path::new("/Users/x/.loremaster"));
    }

    #[test]
    fn relative_home_becomes_absolute() {
        let paths = Paths::resolve(Some("rel-home".into()), None).unwrap();
        assert!(paths.home().is_absolute());
        assert!(paths.home().ends_with("rel-home"));
    }

    #[test]
    fn no_home_at_all_is_an_error() {
        assert!(matches!(
            Paths::resolve(None, None),
            Err(ConfigError::NoHomeDir)
        ));
    }

    #[test]
    fn from_env_resolves_this_process() {
        let expected = Paths::resolve(
            std::env::var_os("LOREMASTER_HOME"),
            std::env::var_os("HOME"),
        )
        .unwrap();
        assert_eq!(Paths::from_env().unwrap(), expected);
    }

    fn patterns_of(text: &str) -> Result<Vec<String>, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        load_patterns(&path)
    }

    #[test]
    fn reads_redact_patterns() {
        assert_eq!(
            patterns_of("[redact]\npatterns = [\"ACME-[0-9]{6}\"]").unwrap(),
            ["ACME-[0-9]{6}"]
        );
    }

    #[test]
    fn missing_file_or_section_means_no_patterns() {
        assert!(
            load_patterns(Path::new("/nonexistent/config.toml"))
                .unwrap()
                .is_empty()
        );
        assert!(patterns_of("").unwrap().is_empty());
        assert!(patterns_of("[redact]").unwrap().is_empty());
    }

    #[test]
    fn invalid_toml_reports_its_line() {
        assert!(matches!(
            patterns_of("[redact\n"),
            Err(ConfigError::ConfigParse { line: 1, .. })
        ));
    }

    #[test]
    fn wrong_type_is_a_parse_error() {
        assert!(matches!(
            patterns_of("[redact]\npatterns = \"x\""),
            Err(ConfigError::ConfigParse { line: 2, .. })
        ));
    }

    fn api_url_of(text: &str) -> Result<Option<String>, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        load_api_url(&path)
    }

    #[test]
    fn reads_the_api_url_next_to_redact_patterns() {
        let text = "api_url = \"https://api.example\"\n[redact]\npatterns = [\"x\"]";
        assert_eq!(
            api_url_of(text).unwrap().as_deref(),
            Some("https://api.example")
        );
        assert_eq!(patterns_of(text).unwrap(), ["x"]);
    }

    #[test]
    fn missing_file_or_key_means_no_api_url() {
        assert_eq!(
            load_api_url(Path::new("/nonexistent/config.toml")).unwrap(),
            None
        );
        assert_eq!(api_url_of("[redact]").unwrap(), None);
        assert!(matches!(
            api_url_of("api_url = 1"),
            Err(ConfigError::ConfigParse { line: 1, .. })
        ));
    }

    #[test]
    fn service_url_has_no_trailing_slash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "api_url = \"https://api.example/\"").unwrap();
        assert_eq!(
            service_url(&path).unwrap().as_deref(),
            Some("https://api.example")
        );
    }

    #[test]
    fn credential_is_read_and_a_bad_file_means_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        assert_eq!(load_credential(&path), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_credential(&path), None);
        std::fs::write(&path, r#"{"device_id":"dev_a","token":"t"}"#).unwrap();
        assert_eq!(
            load_credential(&path),
            Some(DeviceCredential {
                device_id: "dev_a".into(),
                token: "t".into()
            })
        );
    }

    #[test]
    fn unreadable_path_is_a_read_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_patterns(dir.path()),
            Err(ConfigError::Read(_))
        ));
    }
}
