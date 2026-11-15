# M4 F1. Extract endpoint — Design

Source: project `design.md` (Section 3 Extraction, Section 5 Service, Section 6 `users`), `milestones.md` M4 F1, M2 F2 and M3 F1 designs. Resolves milestones OQ4 for Haiku and OQ5.

## Overview
- `POST /v1/extract` turns one prepared turn into 0 to 3 memory candidates `{text, scope, kind}`. The prompt is held by the server.
- The Lambda calls Claude Haiku 5.5 on the Anthropic Messages API with a service API key from AWS Secrets Manager and one forced tool. A per-user daily input-token counter on `users` enforces the 5M cap.

## Components
- `loremaster-core`: new `ExtractRequest {turn: String}`, `ExtractResponse {candidates: Vec<Candidate>}`, `Candidate {text: String, scope: Scope, kind: Kind}`. `Pipeline` (M4 F3) uses them.
- `loremaster-service` (`crates/loremaster-service/src/lib.rs`): route `POST /v1/extract` behind the auth middleware. `AppState` holds a `reqwest::Client` for the Messages API, the API key and the API base URL. A new `llm` module holds the Messages call, the reply parse and the prompt.
- `src/main.rs`: at cold start, reads the API key once with `aws-sdk-secretsmanager` `GetSecretValue`. A failed read stops the start-up (the Lambda reports an init error), so no request runs without a key.
- Prompt: `crates/loremaster-service/prompts/extract.md`, built in with `include_str!`. It has the junk rules (FR15) and the `kind` and `scope` definitions. The wording is tuned in M4 F5. M4 F2 and F4 add their prompts next to it.
- `infra/template.yaml`: Lambda timeout, env, IAM.

## Data Model
- `users` adds `tokens_day` (S, UTC date `YYYY-MM-DD`) and `tokens_used` (N, input tokens on `tokens_day`). Missing = 0. M4 F2 and F4 use the same counter.
- Secrets Manager: one secret per stack, named `<stack name>/anthropic-api-key` (`loremaster-dev/anthropic-api-key`), the key as a plain `SecretString`. The user creates it by hand. It is not in the template or in git.
- Nothing else is stored. The turn and the candidates are not written on the server.

## Interfaces
- `POST /v1/extract`, device token required.
  - Request: `{"turn": "<prepared turn>"}`, the `turn_queue.turn` string unchanged.
  - 200: `{"candidates": [{"text", "scope", "kind"}]}`, 0 to 3 items.
  - 400: body does not parse, or `turn` is empty.
  - 413: `estimate_tokens(turn)` > 32,000. No model call.
  - 429: `tokens_day` = today and `tokens_used` >= 5,000,000. No model call.
  - 502: Messages API error other than 429 and 529, no reply in 25 s, a body that does not parse, or no `tool_use` block in the reply.
  - 503: Messages API 429 (`rate_limit_error`) or 529 (`overloaded_error`).
  - 401, 500: as M2. Error body `{"error"}`.
- Messages API `POST {ANTHROPIC_API_URL}/v1/messages`:
  - Headers: `x-api-key`, `anthropic-version: 2026-10-21`, `content-type: application/json`.
  - `model` = env `MODEL_ID`, default `claude-haiku-5-5`. `max_tokens` 4,096.
  - `system` = the extract prompt. One user message with the turn.
  - `tools`: one tool `record_candidates`, `input_schema` `{candidates: array, maxItems 3, items {text: string, scope: enum user|project, kind: enum preference|decision|correction|environment|pitfall}}`. `tool_choice` = `{"type": "tool", "name": "record_candidates"}`. Haiku 5.5 accepts a forced tool; the reply then has no `thinking` block.
  - Reads the `tool_use` block's `input` and `usage.input_tokens`. Other content blocks are ignored.
- Lambda env adds `MODEL_ID`, `ANTHROPIC_KEY_SECRET` (the secret name), and the optional `ANTHROPIC_API_URL` (default `https://api.anthropic.com`, tests set the fake). Local runs set `ANTHROPIC_API_KEY` in place of `ANTHROPIC_KEY_SECRET`.

## Flow
- 0. Cold start: `GetSecretValue(ANTHROPIC_KEY_SECRET)` → key in `AppState`.
- 1. Auth middleware gives `user_id`.
- 2. Parse the body. Check the size: over 32k tokens gives 413.
- 3. `GetItem` on `users[user_id]` for `tokens_day` and `tokens_used`. Over the cap today gives 429.
- 4. Messages call with a 25 s timeout. 429 or 529 gives 503. Other failure gives 502.
- 5. Add `usage.input_tokens` to the counter, on the UTC day after the call:
  - `UpdateItem SET tokens_used = tokens_used + :n`, condition `tokens_day = :today`.
  - That condition fails: `UpdateItem SET tokens_day = :today, tokens_used = :n`, condition `attribute_not_exists(tokens_day) OR tokens_day <> :today`.
  - Both fail (a parallel call made the new day first): do the first update again.
  - The counter is written before the reply on every model success, also when no candidate is valid.
- 6. Check each candidate: `text` trimmed is 1 to 2,000 chars, and `scope` and `kind` parse. Drop and log the bad ones. Keep the first 3.
- 7. 200 with the kept candidates.

## Infrastructure
- Lambda `Timeout` is 300 s, set for the account purge. The API Gateway HTTP API still ends a request at 30 s, so the model call stops at 25 s.
- IAM: `secretsmanager:GetSecretValue` on `arn:${AWS::Partition}:secretsmanager:${AWS::Region}:${AWS::AccountId}:secret:${AWS::StackName}/anthropic-api-key-??????` (Secrets Manager adds a 6-character suffix). No Bedrock permissions.
- IAM adds `dynamodb:GetItem` and `dynamodb:UpdateItem` on `users`. Both are granted already, so there is no change.
- HTTP client: `reqwest` (already a dependency, rustls), 25 s request timeout, no retries. The device retries.
- New crate: `aws-sdk-secretsmanager`, same SDK family and `aws-config` as `aws-sdk-dynamodb`. `aws-sdk-bedrockruntime` and `aws-smithy-types` are removed.
- Before the first deploy, the user creates the secret: `aws secretsmanager create-secret --name loremaster-dev/anthropic-api-key --secret-string <key>`.

## Testing
- As M2 and M3: the router in process, DynamoDB Local, tables with unique names.
- Fake Messages API: `ANTHROPIC_API_URL` points the client at it, the same way as `GITHUB_API_URL`. Each test starts a local `axum` fake on `POST /v1/messages`. It checks the `x-api-key` and `anthropic-version` headers and returns a Messages reply with a `tool_use` block and `usage`. A marker in the turn selects the reply: candidates, none, bad candidates, no tool use, 429, 529, 500, delay over the test timeout. The fake counts calls, so the 413 and 429 tests check "no model call".
- The key read at cold start is in `main.rs`, which tests do not run. Tests build `AppState` with a test key.
- Cap test: the test writes `tokens_day` = today and `tokens_used` = 5,000,000 on the user item.
- Dev stack verify cases (real Haiku) need approval, as in `milestones.md`.

## Key Decisions
- D1. Anthropic Messages API with a service API key in Secrets Manager. Alt: Bedrock with the Lambda IAM role. Why: the account's Bedrock quotas for every Claude model are 0 tokens per minute and AWS Support refused an increase, so Bedrock cannot serve calls. The key belongs to the service, not the user, so FR19 holds.
- D2. Read the key once at cold start with `aws-sdk-secretsmanager`. Alt: CloudFormation dynamic reference into an env var; the Parameters and Secrets Lambda Extension. Why: the key does not show in the Lambda console or `GetFunctionConfiguration`, a rotation needs only a cold start, and the code is a few lines with the SDK family already in use.
- D3. Forced tool use for structured output. Alt: JSON in the text reply. Why: the reply always has the schema shape, so there is no text parsing.
- D4. Cap: check before the call, add the actual `input_tokens` after. Alt: reserve an estimate with a conditional update first. Why: exact count. Parallel calls can go over the cap by at most one call each, which is acceptable for a cost backstop.
- D5. 503 for API 429 and 529, 502 for other failures, no retry in the Lambda. Alt: one 502, or retries. Why: the device keeps the turn pending on any non-200 (M4 F3), and 503 shows rate-limit and overload problems in logs.
- D6. Fake Messages API through `ANTHROPIC_API_URL`. Alt: an `LlmClient` trait with a stub. Why: the real HTTP and parse code run in tests, as M2 F2 D2 does for GitHub.
- D7. Drop bad candidates and keep the first 3. Alt: 502 for the whole reply. Why: one bad candidate does not make the device pay for the turn again.
- D8. The device sends the stored prepared-turn string unchanged. Alt: a structured turn object. Why: `turn_queue.turn` already has the condensed and redacted JSON, so the server does not parse it.
- D9. Raw HTTP with `reqwest`. Alt: a community Rust SDK. Why: there is no official Anthropic Rust SDK, and `reqwest` is already used for GitHub.

## Open Questions
- OQ1. The user creates an Anthropic API key (console.anthropic.com) and the secret `loremaster-dev/anthropic-api-key` before the Dev stack deploy. Owner: user. Replaces milestones OQ8 "LLM credential".
