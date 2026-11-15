# M4 F1. Extract endpoint — Tasks

Input: `design.md` next to this file, project `design.md` (Section 3 Extraction, Section 5 Service), `milestones.md` M4 F1, `requirements.md` FR15. Service tests need DynamoDB Local (`infra/local.sh`) and run on the developer machine only.

## 1. A turn gives candidates [serial]
- [x] 1.1 In `crates/loremaster-core/src/lib.rs`, add `ExtractRequest {turn: String}`, `ExtractResponse {candidates: Vec<Candidate>}` and `Candidate {text: String, scope: Scope, kind: Kind}`. Unit test: all 3 round-trip through `serde_json` with the field names in the design.
- [x] 1.2 In `crates/loremaster-service/Cargo.toml`, replace `aws-sdk-bedrockruntime` and `aws-smithy-types` with `aws-sdk-secretsmanager`. `Config` gets `model_id` from `MODEL_ID` (default `claude-haiku-5-5`) and `anthropic_api_url` from `ANTHROPIC_API_URL` (default `https://api.anthropic.com`). `AppState::new` takes a `reqwest::Client` from `model_client(api_key, timeout)` (key and version headers, 25 s in production). In `src/main.rs`, read the key with `GetSecretValue(ANTHROPIC_KEY_SECRET)`, or from `ANTHROPIC_API_KEY` for local runs. Existing tests still pass.
- [x] 1.3 Write the extract prompt in `crates/loremaster-service/prompts/extract.md`: 0 to 3 durable facts, the `kind` and `scope` definitions, the FR15 junk list (temporary task state, content visible in code or git history, generic knowledge, restated agent actions, pleasantries). Built in with `include_str!`. Compiles.
- [x] 1.4 Module `crates/loremaster-service/src/llm.rs`: `POST /v1/messages` with `x-api-key` and `anthropic-version: 2026-10-21`, the prompt as `system`, the turn as the user message, tool `record_candidates` (schema as in the design) as the forced `tool_choice`. Returns the `tool_use` input and `usage.input_tokens`. Remove the Document converters. Compiles.
- [x] 1.5 Fake Messages API in `tests/common/mod.rs`: a local `axum` server on `POST /v1/messages` that checks the key and version headers and returns a Messages reply with a `tool_use` block and `usage`, selected by a marker in the turn. It counts calls. `Service::start` sets `anthropic_api_url` to it. Existing tests still pass.
- [x] 1.6 Route `POST /v1/extract` behind the auth middleware in `src/lib.rs`: parse `ExtractRequest` (400 if not JSON or `turn` empty), call `llm`, 200 `ExtractResponse`. New `tests/extract.rs`, test `turn_gives_candidates`: fake returns 1 preference candidate with scope user → 200 with that candidate. Test `pleasantries_give_no_candidates`: fake returns none → 200 `candidates` empty. Test `no_token_is_401`.

## 2. Size limit and daily cap [serial, needs 1]
- [x] 2.1 413 in the extract handler when `estimate_tokens(turn)` > 32,000. Test `turn_over_32k_tokens_is_413`: 96,003 chars → 413, fake call count 0. 96,000 chars → 200.
- [x] 2.2 `pub fn utc_day(epoch_ms) -> String` (`YYYY-MM-DD`) in `src/lib.rs`, with no new date crate. Unit test: `0` → `1970-01-01`, a leap day and a year end give the right dates.
- [x] 2.3 Before the call, `GetItem` on `users[user_id]`: `tokens_day` = today and `tokens_used` >= 5,000,000 → 429. Test `user_over_cap_today_is_429`: item set to today and 5,000,000 → 429, call count 0. Same count on yesterday's `tokens_day` → 200.
- [x] 2.4 After each model success, add `usage.input_tokens` on the UTC day read after the call: the conditional `UpdateItem` on `tokens_day = :today`, then the new-day `UpdateItem`, then the first update again, as in the design. Test `counter_adds_input_tokens`: 2 calls with fake usage 1,200 → `tokens_used` 2,400, `tokens_day` today. Test `new_day_resets_counter`: yesterday and 4,999,999 → `tokens_used` 1,200, `tokens_day` today.

## 3. Model failures and bad output [parallel, needs 1]
- [x] 3.1 Map API errors in `src/llm.rs` and `ApiError`: 429 and 529 → 503, other status, a body that does not parse, or timeout → 502. Test `model_failures`: fake 429 → 503, fake 529 → 503, fake 500 → 502, fake delay of 2 s with the test client timeout at 1 s → 502, counter unchanged in all 4.
- [x] 3.2 Reply with no `tool_use` block → 502. Test `reply_without_tool_use_is_502`.
- [x] 3.3 Drop bad candidates, keep the first 3, log the dropped ones. Table test `bad_candidates_are_dropped`: empty text, text of 2,001 chars, unknown `kind`, unknown `scope`, 4 valid candidates → only valid ones returned, at most 3. Text of 2,000 chars kept. Counter still adds the tokens when no candidate is left.

## 4. The dev stack extracts with Haiku [serial, needs 2 and 3] [approval]
- [x] 4.1 In `infra/template.yaml`: `Timeout: 30`, env `MODEL_ID` and `ANTHROPIC_KEY_SECRET` (`${AWS::StackName}/anthropic-api-key`), IAM `secretsmanager:GetSecretValue` on that secret ARN with the `-??????` suffix. Remove the `ModelName` parameter and the Bedrock statements. Add `ANTHROPIC_API_KEY` and `MODEL_ID` to the local run comment in `infra/local.sh`. `sam validate --lint` passes.
- [x] 4.2 Needs design OQ1 (secret created). `sam build` and `sam deploy --config-env dev`. Over HTTPS with a device from `gh auth token`, run the M4 F1 verify cases in `milestones.md`: "I prefer pnpm over npm" → 1 to 3 candidates, one `kind` preference, `scope` user. Pleasantries only → `candidates` empty. Over 32k tokens → 413. `tokens_used` set to 5,000,000 today → 429. No token → 401. Delete the test device after and reset the counter.

## Dependencies
- 1 blocks 2 and 3. 2 and 3 run in parallel. They both edit `src/lib.rs` and `tests/extract.rs`, so use worktrees.
- 4 needs 2 and 3, design OQ1, and the user's approval before the deploy in 4.2.
