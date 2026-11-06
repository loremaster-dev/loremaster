//! `loremaster login`: GitHub's device flow, then one device registration with the sync service.
//! The device flow needs no browser on this machine, so it works over SSH.

use std::io::{self, BufRead, Write};
use std::process::Command;
use std::time::{Duration, Instant};

use loremaster_core::{Device, DeviceCredential, DeviceLimitReached, RegisterDevice};
use serde::Deserialize;
use thiserror::Error;

use crate::config::{self, ConfigError, Paths};

/// The Loremaster GitHub OAuth App. A device-flow client ID is public.
const GITHUB_CLIENT_ID: &str = "Ov23lisZK2tW63axnphp";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
const MAX_NAME_CHARS: usize = 64;

#[derive(Debug, Error)]
pub enum LoginError {
    #[error("no api_url: set it in ~/.loremaster/config.toml")]
    NoApiUrl,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot reach {url}: {reason}")]
    Http { url: String, reason: String },
    #[error("unexpected reply from {url}: {reason}")]
    BadReply { url: String, reason: String },
    #[error("GitHub login failed: {0}")]
    Github(String),
    #[error("GitHub login expired before it was approved")]
    Expired,
    #[error("service refused the login ({status}): {message}")]
    Service { status: u16, message: String },
    #[error("no device chosen; nothing changed")]
    NoChoice,
    #[error("cannot save credentials: {0}")]
    Save(#[from] io::Error),
}

/// Signs this machine in and saves its device credential. Prompts go to `out`; a choice of
/// device to replace is read from `input`.
pub fn run(
    paths: &Paths,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<(), LoginError> {
    let api_url = config::load_api_url(&paths.config_toml())?
        .or_else(|| option_env!("LOREMASTER_API_URL").map(str::to_owned))
        .ok_or(LoginError::NoApiUrl)?;
    let api_url = api_url.trim_end_matches('/');
    let github_url =
        std::env::var("LOREMASTER_GITHUB_URL").unwrap_or_else(|_| "https://github.com".to_owned());
    let http = Http::new();
    let github_token = github_login(&http, &github_url, out)?;
    let name = device_name();
    let mut request = RegisterDevice {
        github_token,
        name,
        replace_device_id: saved_device_id(paths),
    };
    let credential = loop {
        match register(&http, api_url, &request)? {
            Registered::Created(credential) => break credential,
            Registered::NotFound if request.replace_device_id.is_some() => {
                request.replace_device_id = None;
            }
            Registered::NotFound => {
                return Err(LoginError::BadReply {
                    url: api_url.to_owned(),
                    reason: "404 for a new device".to_owned(),
                });
            }
            Registered::Full(devices) => {
                request.replace_device_id = Some(choose_device(&devices, input, out)?);
            }
        }
    };
    save_credential(paths, &credential)?;
    writeln!(
        out,
        "Logged in as device {} ({}).",
        request.name, credential.device_id
    )?;
    Ok(())
}

struct Http {
    agent: ureq::Agent,
}

struct Reply {
    url: String,
    status: u16,
    body: String,
}

impl Reply {
    fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, LoginError> {
        serde_json::from_str(&self.body).map_err(|e| LoginError::BadReply {
            url: self.url.clone(),
            reason: e.to_string(),
        })
    }
}

impl Http {
    fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent }
    }

    fn post_form(&self, url: String, form: &[(&str, &str)]) -> Result<Reply, LoginError> {
        let request = self.agent.post(&url).header("Accept", "application/json");
        Self::read(url.clone(), request.send_form(form.iter().copied()))
    }

    fn post_json(&self, url: String, body: &impl serde::Serialize) -> Result<Reply, LoginError> {
        let body = serde_json::to_string(body).expect("request types always serialize");
        let request = self
            .agent
            .post(&url)
            .header("Content-Type", "application/json");
        Self::read(url.clone(), request.send(body))
    }

    fn read(
        url: String,
        sent: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Reply, LoginError> {
        let failed = |e: ureq::Error| LoginError::Http {
            url: url.clone(),
            reason: e.to_string(),
        };
        let mut response = sent.map_err(failed)?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().map_err(failed)?;
        Ok(Reply { url, status, body })
    }
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// What to do after one poll of GitHub's token endpoint.
#[derive(Debug, PartialEq, Eq)]
enum Poll {
    Token(String),
    Wait(Duration),
    Failed(String),
    Expired,
}

fn next_poll(reply: TokenReply, interval: Duration) -> Poll {
    match (reply.access_token, reply.error.as_deref()) {
        (Some(token), None) => Poll::Token(token),
        (_, Some("authorization_pending")) => Poll::Wait(interval),
        (_, Some("slow_down")) => Poll::Wait(interval + SLOW_DOWN_STEP),
        (_, Some("expired_token")) => Poll::Expired,
        (_, Some(error)) => {
            Poll::Failed(reply.error_description.unwrap_or_else(|| error.to_owned()))
        }
        (None, None) => Poll::Failed("no token in the reply".to_owned()),
    }
}

/// Shows the code to enter on GitHub and waits until the user approves it elsewhere.
fn github_login(http: &Http, github_url: &str, out: &mut impl Write) -> Result<String, LoginError> {
    let reply = http.post_form(
        format!("{github_url}/login/device/code"),
        &[("client_id", GITHUB_CLIENT_ID)],
    )?;
    if reply.status != 200 {
        return Err(LoginError::Github(format!(
            "HTTP {}: {}",
            reply.status, reply.body
        )));
    }
    let code: DeviceCode = reply.json()?;
    writeln!(
        out,
        "Open {} and enter code {}",
        code.verification_uri, code.user_code
    )?;
    // GitHub can show an error page after an approval that worked, so point to the real result.
    writeln!(
        out,
        "After you approve, check this terminal for the result."
    )?;
    out.flush()?;
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    let mut interval = Duration::from_secs(code.interval);
    loop {
        std::thread::sleep(interval);
        if Instant::now() > deadline {
            return Err(LoginError::Expired);
        }
        let reply = http.post_form(
            format!("{github_url}/login/oauth/access_token"),
            &[
                ("client_id", GITHUB_CLIENT_ID),
                ("device_code", &code.device_code),
                ("grant_type", DEVICE_GRANT),
            ],
        )?;
        match next_poll(reply.json()?, interval) {
            Poll::Token(token) => return Ok(token),
            Poll::Wait(next) => interval = next,
            Poll::Failed(reason) => return Err(LoginError::Github(reason)),
            Poll::Expired => return Err(LoginError::Expired),
        }
    }
}

enum Registered {
    Created(DeviceCredential),
    NotFound,
    Full(Vec<Device>),
}

fn register(
    http: &Http,
    api_url: &str,
    request: &RegisterDevice,
) -> Result<Registered, LoginError> {
    let reply = http.post_json(format!("{api_url}/v1/devices"), request)?;
    match reply.status {
        201 => Ok(Registered::Created(reply.json()?)),
        404 => Ok(Registered::NotFound),
        409 => Ok(Registered::Full(
            reply.json::<DeviceLimitReached>()?.devices,
        )),
        status => {
            #[derive(Deserialize)]
            struct ErrorBody {
                error: String,
            }
            let message = reply
                .json::<ErrorBody>()
                .map_or(reply.body, |body| body.error);
            Err(LoginError::Service { status, message })
        }
    }
}

/// The device this machine saved at its last login. A missing or unreadable file means none, so a
/// new login is always the way to repair it.
fn saved_device_id(paths: &Paths) -> Option<String> {
    let text = std::fs::read_to_string(paths.credentials()).ok()?;
    serde_json::from_str::<DeviceCredential>(&text)
        .ok()
        .map(|credential| credential.device_id)
}

/// Lists the account's devices and reads which one to replace.
fn choose_device(
    devices: &[Device],
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<String, LoginError> {
    writeln!(
        out,
        "This account has {} devices, the most it can have:",
        devices.len()
    )?;
    for (n, device) in devices.iter().enumerate() {
        writeln!(
            out,
            "  {}. {}  (added {})",
            n + 1,
            device.name,
            utc_date(device.created_at)
        )?;
    }
    write!(out, "Replace which device? [1-{}]: ", devices.len())?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    let choice = line
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|index| devices.get(index))
        .ok_or(LoginError::NoChoice)?;
    Ok(choice.device_id.clone())
}

/// `YYYY-MM-DD` in UTC for a time in epoch milliseconds.
fn utc_date(epoch_ms: u64) -> String {
    // Days to a civil date, from Howard Hinnant's `civil_from_days`.
    let days = (epoch_ms / 86_400_000) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The name other devices see in the device list: this machine's host name.
fn device_name() -> String {
    let output = Command::new("hostname").output().ok();
    let raw = output
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned());
    clean_name(raw.as_deref())
}

fn clean_name(raw: Option<&str>) -> String {
    let name: String = raw
        .unwrap_or_default()
        .trim()
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    let name = name.trim_end();
    if name.is_empty() {
        "device".to_owned()
    } else {
        name.to_owned()
    }
}

/// Writes the credential to a new private file, then moves it into place, so a crash never leaves
/// a partial file and the token is never readable by other users.
fn save_credential(paths: &Paths, credential: &DeviceCredential) -> Result<(), LoginError> {
    config::create_private_dir(paths.home())?;
    // A named temp file is created with mode 0600.
    let mut temp = tempfile::NamedTempFile::new_in(paths.home())?;
    serde_json::to_writer(&mut temp, credential).map_err(io::Error::from)?;
    temp.as_file().sync_all()?;
    temp.persist(paths.credentials()).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(token: Option<&str>, error: Option<&str>) -> TokenReply {
        TokenReply {
            access_token: token.map(str::to_owned),
            error: error.map(str::to_owned),
            error_description: None,
        }
    }

    #[test]
    fn each_token_reply_decides_the_next_poll() {
        let interval = Duration::from_secs(5);
        let cases = [
            (reply(Some("gho_x"), None), Poll::Token("gho_x".to_owned())),
            (
                reply(None, Some("authorization_pending")),
                Poll::Wait(interval),
            ),
            (
                reply(None, Some("slow_down")),
                Poll::Wait(Duration::from_secs(10)),
            ),
            (reply(None, Some("expired_token")), Poll::Expired),
            (
                reply(None, Some("access_denied")),
                Poll::Failed("access_denied".to_owned()),
            ),
            (
                reply(None, None),
                Poll::Failed("no token in the reply".to_owned()),
            ),
        ];
        for (reply, expected) in cases {
            assert_eq!(next_poll(reply, interval), expected);
        }
        let described = TokenReply {
            error_description: Some("The user denied it.".to_owned()),
            ..reply(None, Some("access_denied"))
        };
        assert_eq!(
            next_poll(described, interval),
            Poll::Failed("The user denied it.".to_owned())
        );
    }

    #[test]
    fn utc_dates_match_the_calendar() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
        assert_eq!(utc_date(1_793_000_000_000), "2026-10-26");
        assert_eq!(utc_date(1_798_761_599_999), "2026-12-31");
    }

    #[test]
    fn host_names_become_device_names() {
        assert_eq!(clean_name(Some("work-laptop.local\n")), "work-laptop.local");
        assert_eq!(clean_name(Some(&"h".repeat(70))), "h".repeat(64));
        assert_eq!(clean_name(Some("  \n")), "device");
        assert_eq!(clean_name(None), "device");
        let long_with_space = format!("{} tail", "a".repeat(63));
        assert_eq!(clean_name(Some(&long_with_space)), "a".repeat(63));
    }
}
