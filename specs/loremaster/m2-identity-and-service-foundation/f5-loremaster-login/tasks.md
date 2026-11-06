# M2 F5. `loremaster login` — Tasks

Input: `design.md` next to this file, project `design.md` Section 7, `milestones.md` M2 F5, M2 F2 to F4 designs. CLI tests run in CI (no DynamoDB Local needed).

## 1. A login saves a private device token [serial]
- [x] 1.1 In `crates/loremaster/src/config.rs`, add top-level `api_url: Option<String>` to `ConfigFile`, a `load_api_url(path)` that returns it (missing file → `None`), and `Paths::credentials()` (`<home>/credentials`). Unit tests: file with `api_url`, file without it, missing file, `[redact]` still parses.
- [x] 1.2 Add `axum` to `[dev-dependencies]` of `crates/loremaster/Cargo.toml` (same version as `loremaster-service`). New `crates/loremaster/tests/login.rs` with a fake on `127.0.0.1:0`: `POST /login/device/code`, `POST /login/oauth/access_token` (a scripted list of replies, `interval: 0`), `POST /v1/devices` (a scripted list of status and body). The fake records each request body.
- [x] 1.3 New `crates/loremaster/src/login.rs` and `Login` in `main.rs`: API URL (config, then `option_env!("LOREMASTER_API_URL")`), device code, print `Open <uri> and enter code <code>`, poll, `POST /v1/devices`, write `credentials` (temp file 0600, rename), print `Logged in as device <name> (<device_id>).` Test `login_saves_a_private_credential`: exit 0, `credentials` is the fake's `DeviceCredential`, mode 0600, home 0700, no GitHub token in stdout, stderr, `credentials` or `config.toml`.
- [x] 1.4 Poll rules in `login.rs`: `authorization_pending` waits, `slow_down` adds 5 s, any other `error` or `expires_in` passed stops. Unit test of the pure step function for each reply. Test `waits_while_authorization_is_pending`: 2 pending replies, then a token → exit 0.
- [x] 1.5 Failures exit 1 with one `error: ...` line and write no `credentials`. Table test `failed_logins_write_nothing`: no `api_url`, `access_denied`, `expired_token`, service 401, service 502. `tests/cli.rs` `help_lists_the_commands` also checks `login`.

## 2. Logins keep the device count right [serial, needs 1]
- [x] 2.1 A saved `credentials` sends its `device_id` as `replace_device_id`. 404 then sends again with none. Tests `second_login_replaces_this_machines_device` and `deleted_device_registers_again`: the fake's recorded bodies show the right `replace_device_id`, and `credentials` holds the new device.
- [x] 2.2 409: print devices 1 to 10 (name, UTC date), ask `Replace which device? [1-10]:`, read one stdin line, send again with that `device_id`. Test `full_account_replaces_the_chosen_device`: stdin `3` → second request has device 3's id, exit 0.
- [x] 2.3 Table test `full_account_with_no_valid_choice_changes_nothing`: stdin empty, `0`, `11`, `abc` → exit 1, no second request, no `credentials`, the old `credentials` (if any) unchanged.
- [x] 2.4 Device name from `hostname`, trimmed, cut to 64 characters, `device` on failure or empty. Unit test of the pure function that cleans the output. Test checks the fake got a non-empty `name` of at most 64 characters.

## 3. Real login on the dev stack [serial, needs 2] [manual] [approval]
- [x] 3.1 On the dev Mac, in a sandbox `LOREMASTER_HOME` with `api_url` set to the dev URL, from a shell with no browser: `loremaster login` prints the URL and code. The user approves in a browser on another device. `credentials` exists with mode 0600, and its token gets 200 from `GET /v1/devices`.
- [x] 3.2 After 3.1: `grep` for `gho_` in `~/.loremaster/` and in the saved login output finds nothing.
- [x] 3.3 Fill the dev account to 10 devices with `curl` and `gh auth token`. `loremaster login` lists the 10 and asks. After a choice: exit 0, the replaced token gets 401. Delete the test devices after.

## Dependencies
- 1 blocks 2. 2 blocks 3.
- 3 needs the user and approval for the dev stack. The headless Linux check over SSH moved to M5 F1.
