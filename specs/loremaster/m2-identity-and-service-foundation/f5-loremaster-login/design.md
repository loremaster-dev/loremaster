# M2 F5. `loremaster login` — Design

Source: project `design.md` (Section 7 Identity, Infrastructure), `milestones.md` M2 F5, M2 F2 to F4 designs.

## Overview
- `loremaster login` signs the machine in with GitHub's device flow, registers it as a device, and saves the device token. It works over SSH with no browser on the machine.
- The CLI gets a GitHub token with the device flow, sends it once to `POST /v1/devices`, drops it, and writes `{device_id, token}` to `~/.loremaster/credentials` (0600).

## Components
- `login` module: `crates/loremaster/src/login.rs`. Device flow, register, 409 prompt, credentials file.
- `Login` subcommand: `crates/loremaster/src/main.rs`.
- `config.rs`: `api_url` in `config.toml`, and `Paths::credentials()` (`<home>/credentials`).
- HTTP: `ureq` 3.5, already a dependency. No new runtime dependencies.
- Types from `loremaster-core`: `RegisterDevice`, `DeviceCredential`, `DeviceLimitReached`.

## Data Model
- `~/.loremaster/credentials`: JSON `DeviceCredential` `{"device_id": "dev_...", "token": "<64 hex>"}`. Mode 0600. The home dir is made with `create_private_dir` (0700). A home dir that exists already keeps its mode.
- `~/.loremaster/config.toml`: adds top-level `api_url = "https://..."` (optional). `[redact]` stays the same.
- The GitHub token is never written to disk or printed.

## Interfaces
- `loremaster login`: no arguments. Exit 0 on success, 1 on any failure, with one `error: ...` line on stderr.
- API URL: `config.toml` `api_url`, else `option_env!("LOREMASTER_API_URL")` at build time. Neither: exit 1, `error: no api_url: set it in ~/.loremaster/config.toml`. Trailing `/` characters are removed before `/v1/devices` is added.
- GitHub base URL: env `LOREMASTER_GITHUB_URL`, default `https://github.com`. Client ID `Ov23lisZK2tW63axnphp`, in the code.
- GitHub calls, both `POST` with `Accept: application/json`, form body:
  - `/login/device/code` `client_id` → `{device_code, user_code, verification_uri, expires_in, interval}`.
  - `/login/oauth/access_token` `client_id, device_code, grant_type=urn:ietf:params:oauth:grant-type:device_code` → `{access_token}` or `{error}`.
- Service call: `POST {api_url}/v1/devices` with `RegisterDevice`. 201 `DeviceCredential`. 409 `DeviceLimitReached`. 404 for a bad `replace_device_id`. Other status: exit 1 with the service's `error` text.
- HTTP timeout: 30 s for each request.

## Flow
- 1. Find `api_url` (exit 1 if none). Read `credentials` if it exists, for its `device_id`.
- 2. Device code from GitHub. Print to stdout: `Open <verification_uri> and enter code <user_code>`. Open no browser.
- 3. Poll the token endpoint every `interval` s:
  - `authorization_pending`: wait, poll again.
  - `slow_down`: add 5 s to the interval, poll again.
  - `expired_token`, `access_denied`, any other `error`, or `expires_in` passed: exit 1 with the error.
- 4. `POST /v1/devices` with `github_token`, `name`, and `replace_device_id` = the saved `device_id`, if any.
  - 404 and a saved `device_id` was sent: send again with no `replace_device_id`.
  - 409: print the devices with numbers 1 to 10, one line each: `  3. <name>  (added YYYY-MM-DD)`, date in UTC. Then `Replace which device? [1-10]:`. Read one line from stdin. A valid number: send again with that `device_id` as `replace_device_id` and the same GitHub token. Empty line, bad number, or end of input: exit 1, nothing changes.
- 5. Drop the GitHub token. Write `credentials`: temp file with mode 0600 in the home dir, then rename to `credentials`.
- 6. Print `Logged in as device <name> (<device_id>).` The token is never printed.
- Device name: output of the `hostname` command, trimmed, cut to 64 characters. Command fails or empty: `device`.

## Testing
- `crates/loremaster/tests/login.rs`. A sandbox home (`LOREMASTER_HOME`) with `api_url` in `config.toml`, and `LOREMASTER_GITHUB_URL` set.
- One in-process fake (`axum` and `tokio`, dev-dependencies already in the workspace) serves the 2 GitHub endpoints and `POST /v1/devices`. It replies with the `loremaster-core` types and records the requests it gets.
- The poll interval comes from the fake's `interval` field, so tests use `interval: 0` and run fast.
- Cases: success writes 0600 `credentials` and no GitHub token on disk or in output. Pending then success. `slow_down`. `access_denied`. No `api_url`. Second login sends the saved `device_id`. 404 then a new device. 409 with a number on stdin. 409 with a bad answer changes nothing.
- Real GitHub and the dev stack: the manual Verify cases in `milestones.md`, over SSH.

## Key Decisions
- D1. `api_url` from `config.toml`, then `LOREMASTER_API_URL` at build time, else error. Alt: the dev URL in the code. Why: no dev URL in the binary. M5 F1 sets the prod URL in the release build.
- D2. A second login replaces this machine's saved device. 404: register a new one. Alt: always register a new device, or refuse. Why: a second login on the same machine does not take one more of the 10 device slots.
- D3. 409 prompt reads one number from stdin. No `--replace` flag. Alt: a flag. Why: the Verify case asks for a prompt, and a test can send stdin.
- D4. Device name from the `hostname` command. No `--name` flag. Alt: the `hostname` crate. Why: no new dependency, and macOS and Linux have the command.
- D5. `credentials` is JSON `DeviceCredential`, written with a temp file and a rename. Alt: TOML, or a write in place. Why: the type exists, M5 uninstall needs `device_id`, and a crash never leaves half a file.
- D6. Fake GitHub and fake service in the CLI tests, through `LOREMASTER_GITHUB_URL` and `api_url`. Alt: the real service with DynamoDB Local. Why: CLI tests run in CI, which has no DynamoDB Local.
- D7. Blocking `ureq` with `std::thread::sleep` for the poll. Alt: `tokio`. Why: `login` is one short command, and `ureq` is the CLI's HTTP client today.
- D8. A home dir that exists keeps its mode. Alt: `chmod 0700` at login. Why: `enable` and `daemon` make it 0700, and `credentials` is 0600 in any dir.

## Open Questions
- None.
