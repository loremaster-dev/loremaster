# M2 F2. Register a device — Tasks

Input: `design.md` next to this file, project `design.md` Sections 5 to 7, `milestones.md` M2 F2. Service tests need DynamoDB Local (`infra/local.sh`) and run on the developer machine only.

## 1. A GitHub token registers a device [serial]
- [x] 1.1 Add `RegisterDevice {github_token, name}` and `DeviceCredential {device_id, token}` to `crates/loremaster-core/src/lib.rs`. Unit test: both round-trip through `serde_json` with the field names in the design.
- [x] 1.2 Add `infra/local.sh`: download the DynamoDB Local jar if missing, start it with `-inMemory`, create `users` and `devices` (GSI `user_id`) with `aws dynamodb create-table --endpoint-url`. Running it twice gives no error.
- [x] 1.3 Make `crates/loremaster-service` (`Cargo.toml`, `src/lib.rs` with `router(state)`, `src/main.rs` with `lambda_http::run`). Config from `USERS_TABLE`, `DEVICES_TABLE`, `GITHUB_API_URL`, `DYNAMODB_ENDPOINT`. `cargo build -p loremaster-service` passes.
- [x] 1.4 Add `crates/loremaster-service/tests/common/mod.rs`: unique tables per test in DynamoDB Local, a fake GitHub `axum` server on `localhost`, and a panic that names `infra/local.sh` when DynamoDB Local is not reachable. Stopped DynamoDB Local gives that message.
- [x] 1.5 `POST /v1/devices` in `src/lib.rs`: GitHub `GET /user`, 32-byte token, `dev_<ULID>`, one `TransactWriteItems`. Test `tests/devices.rs` `registers_a_user_and_a_device`: 201, `users` has `gh:<id>` with `next_seq` and `live_count` 0, `devices` key is the SHA-256 of `token`, no stored attribute holds the raw token or the GitHub token.
- [x] 1.6 Test `second_login_adds_a_device_to_the_same_user`: same GitHub id twice gives 2 `device_id`s, one `users` item, both devices with the same `user_id`.

## 2. Bad input and GitHub failures write nothing [serial, needs 1]
- [x] 2.1 Table test `rejected_requests_write_nothing` in `tests/devices.rs`: no body, bad JSON, missing field, name empty, name only spaces, name 65 chars → 400. Name 64 chars → 201.
- [x] 2.2 Test `bad_github_token_is_401`: fake GitHub 401 → 401 `{"error": ...}`, both tables empty.
- [x] 2.3 Test `github_failure_is_502`: fake GitHub 500, and fake GitHub delayed past 5 s → 502, both tables empty.

## 3. The service runs locally and CI stays green [parallel, needs 1]
- [x] 3.1 In `.github/workflows/ci.yml`, add `--exclude loremaster-service` to the `test` step. Clippy steps still cover the crate. CI passes on all 3 runners.
- [x] 3.2 Run `infra/local.sh`, then `cargo lambda watch` with the 4 env vars. `curl` to `POST /v1/devices` with a fake GitHub gives 201. Record the exact URL and env vars in a comment at the end of `infra/local.sh`.

## 4. The dev stack runs on AWS [parallel, needs 1] [approval]
- [x] 4.1 Add `infra/template.yaml`: HTTP API, arm64 `provided.al2023` Lambda built by cargo-lambda, timeout 10 s, `users` and `devices` on demand with `SSEEnabled: true`, IAM role with `UpdateItem` on `users` and `PutItem` on `devices` only. `sam validate --lint` passes.
- [x] 4.2 Add `infra/samconfig.toml`: env `dev`, stack `loremaster-dev`, region `us-east-2`, `resolve_s3`, profile `loremaster`. `sam build` passes.
- [x] 4.3 `sam deploy --config-env dev`. With a real GitHub token from `gh auth token`: 201 for a valid token, a second call gives the same `user_id`, an invalid token gives 401, over HTTPS.
- [x] 4.4 `aws dynamodb describe-table` on both tables: `SSEType` `KMS`, billing mode `PAY_PER_REQUEST`.

## Dependencies
- 1 blocks 2, 3 and 4.
- 3 and 4 run in parallel.
- 4 needs the user's approval before deploy (4.3).
