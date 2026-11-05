# M2 F2. Register a device — Design

Source: project `design.md` (Sections 5, 6, 7, Infrastructure, Test Tooling), `milestones.md` M2 F2. Resolves milestones OQ4 for GitHub.

## Overview
- A deployable stack where `POST /v1/devices` turns a GitHub token and a device name into a user, a device and a device token.
- One Rust Lambda (`axum` via `lambda_http`) behind an API Gateway HTTP API. The Lambda reads GitHub `GET /user` and writes `users` and `devices` in one DynamoDB transaction.

## Components
- `loremaster-service`: new crate, one binary. `axum` router, `lambda_http`, `aws-sdk-dynamodb`, `reqwest` (rustls). `crates/loremaster-service/`.
- `loremaster-core`: adds `RegisterDevice` and `DeviceCredential`, the request and response types. M2 F5 uses the same types.
- `infra/template.yaml`: HTTP API, Lambda, `users`, `devices`, IAM role. `infra/samconfig.toml`: env `dev`.
- `infra/local.sh`: starts DynamoDB Local (jar, Java, `-inMemory`) and creates the 2 tables for a local run.

## Data Model
- `users`: key `user_id` (S, `gh:<numeric GitHub id>`). Attributes `next_seq` (N, 0), `live_count` (N, 0). `tokens_day` and `tokens_used` come in M4.
- `devices`: key `token_hash` (S, hex SHA-256 of the token). Attributes `user_id` (S), `device_id` (S, `dev_<ULID>`), `name` (S, trimmed), `created_at` (N, epoch ms). GSI `by_user` with key `user_id`, which F3 and F4 use.
- Both tables: on-demand billing, `SSESpecification: SSEEnabled: true` (AWS managed KMS key). No PITR.
- Not stored: the raw device token, the GitHub token.

## Interfaces
- `POST /v1/devices`, no auth.
  - Request: `{"github_token": string, "name": string}`. `name` is 1 to 64 characters after trim.
  - 201: `{"device_id": "dev_<ULID>", "token": "<64 hex chars>"}`. The token is 32 bytes from the OS random source.
  - 400: body does not parse, or `name` is out of range.
  - 401: GitHub returns 401 for `github_token`.
  - 502: GitHub returns another error, or no reply in 5 s.
  - Error body: `{"error": "<short message>"}`.
  - Nothing is written on any error.
- `GET {GITHUB_API_URL}/user` with `Authorization: Bearer <github_token>` and a `User-Agent`. Reads `id`.
- Lambda env: `USERS_TABLE`, `DEVICES_TABLE`, `GITHUB_API_URL` (default `https://api.github.com`).

## Flow
- 1. Parse and check the body.
- 2. GitHub `GET /user` → `id`. `user_id` = `gh:<id>`.
- 3. Make the token, `token_hash` = SHA-256, `device_id` = `dev_` + `new_ulid()`.
- 4. `TransactWriteItems`:
  - Update `users[user_id]`: `SET next_seq = if_not_exists(next_seq, 0), live_count = if_not_exists(live_count, 0)`.
  - Put `devices[token_hash]` with condition `attribute_not_exists(token_hash)`.
- 5. Return 201 with `device_id` and `token`. Drop `github_token`.

## Infrastructure
- Lambda: arm64, `provided.al2023`, built by `sam build` with cargo-lambda. Timeout 10 s. M4 makes it longer.
- Table names are made by CloudFormation and passed to the Lambda in env vars.
- IAM role: `dynamodb:UpdateItem` on `users`, `dynamodb:PutItem` on `devices` (the transaction needs both), nothing more.
- `samconfig.toml` env `dev`: stack `loremaster-dev`, region `us-east-2`, `--resolve-s3`. The prod env comes in M5 F1.

## Testing
- Service tests call the `axum` router in process, with DynamoDB Local (`DYNAMODB_ENDPOINT`) and a fake GitHub on `localhost`.
- Fake GitHub: an `axum` server started by each test. It returns `{id, login}` for a known token, 401 for other tokens, and 500 or a delay for the 502 cases.
- Each test makes its own tables with unique names, so tests run in parallel.
- No DynamoDB Local reachable: the tests fail with a message that names `infra/local.sh`. They never skip.
- Service tests run on the developer machine only. CI builds and lints `loremaster-service`, and its test step excludes the crate.
- Local run for /verify: `infra/local.sh`, then `cargo lambda watch` with the 3 env vars.

## Key Decisions
- D1. Service tests run locally only, not in CI. Alt: a Linux CI job with a DynamoDB Local container. Why: simplest for now. The CI job moves to P1.
- D2. GitHub base URL from `GITHUB_API_URL`. Alt: a GitHub client trait with a fake. Why: the real HTTP code runs in tests, with no trait for one real implementation.
- D3. User and device in one transaction. Alt: put the user, then the device. Why: a failed device write never leaves a user with no device.
- D4. AWS managed KMS key. Alt: the AWS owned key (the default). Why: NFR7 needs KMS encryption at rest, and the owned key does not show in `describe-table`.
- D5. `replace_device_id` and the 10-device limit not in F2. Alt: build them now. Why: M2 F4 owns them. F2 has no device count check.

## Open Questions
- None.
