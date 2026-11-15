# M4 F2. Reconcile endpoint — Tasks

Input: `design.md` next to this file (D1 to D6), project `design.md` (Section 3 Extraction, Section 5 Service), `milestones.md` M4 F2, `requirements.md` FR18, FR30, M4 F1 code (`src/llm.rs`, the `extract` handler, `tokens_used`, `add_tokens`, the fake Messages API in `tests/common/mod.rs`). Service tests need DynamoDB Local (`infra/local.sh`).

## 1. A candidate and its neighbours give a decision [serial]
- [x] 1.1 In `crates/loremaster-core/src/lib.rs`, add `ReconcileRequest {candidate: Candidate, neighbours: Vec<Neighbour>}`, `Neighbour {memory_id: String, text: String}` and `ReconcileResponse` (`#[serde(tag = "decision", rename_all = "snake_case")]`: `Add`, `Supersede {memory_id, text}`, `Skip`). Unit test: all 3 decisions and the request round-trip through `serde_json` with the wire shapes in the design.
- [x] 1.2 In `crates/loremaster-service/src/llm.rs`, move the Messages call out of `extract` into one shared function that takes the system prompt, the tool (name, description, `input_schema`), `max_tokens` and the user content, and returns `Reply`. `extract` calls it. All `tests/extract.rs` tests pass unchanged.
- [x] 1.3 Write `crates/loremaster-service/prompts/reconcile.md`: the user message is the request JSON; SUPERSEDE when the candidate updates or contradicts a neighbour (the candidate is newer and wins), with a text that may combine both, one self-contained sentence, at most 2,000 chars; SKIP when a neighbour already says the same; ADD otherwise. Built in with `include_str!`. Compiles.
- [x] 1.4 In `llm.rs`, add `reconcile` (tool `record_decision`, schema as in the design, forced `tool_choice`, `max_tokens` 1,024, the request body JSON as the user message) and `decision(tool_input, neighbours) -> Result<ReconcileResponse, LlmError>` with the Flow step 6 check. Compiles.
- [x] 1.5 In `tests/common/mod.rs`, the fake Messages API names the `tool_use` block after `tool_choice.name` in the request, and a `REPLY` marker in a reconcile candidate text selects the tool input. All existing service tests pass.
- [x] 1.6 Route `POST /v1/reconcile` behind the auth middleware in `src/lib.rs`, with the cap check and `add_tokens` as `extract`. New `tests/reconcile.rs`: fake add → `{"decision":"add"}`; fake skip → `{"decision":"skip"}`; fake supersede of the second neighbour with text padded by spaces → that `memory_id` and the trimmed text; the request to the fake has the reconcile prompt, tool `record_decision` and the candidate and neighbours in the user message; no token → 401.

## 2. Bad bodies, the daily cap and bad replies [serial, needs 1]
- [x] 2.1 Body checks in the reconcile handler. Table test `bad_bodies_are_400`: not JSON, candidate text empty and 2,001 chars, 0 and 6 neighbours, a neighbour text empty and 2,001 chars, two neighbours with one `memory_id` → 400, fake call count 0. 5 neighbours and texts of 2,000 chars → 200.
- [x] 2.2 Test `user_over_cap_today_is_429`: `tokens_used` 5,000,000 today → 429, call count 0. Test `counter_adds_input_tokens`: one reconcile and one extract → `tokens_used` is 2 times the fake usage.
- [x] 2.3 Table test `bad_replies_are_502`: supersede with a `memory_id` not in the neighbours, with no `memory_id`, with empty or blank text, with text of 2,001 chars, an unknown `decision`, no `decision`, no tool call → 502, and the counter adds the tokens of each. Supersede with text of 2,000 chars → 200. An add reply with extra `memory_id` and `text` → `{"decision":"add"}`.
- [x] 2.4 Test `model_failures`: fake 429 and 529 → 503, fake 500, a body that is not JSON and a delay over the client timeout → 502, counter unchanged.

## 3. The dev stack reconciles with Haiku [serial, needs 2] [approval]
- [x] 3.1 With the user's approval, `sam build` and `sam deploy --config-env dev`. Over HTTPS with a device from `gh auth token`, run the M4 F2 verify cases in `milestones.md`: "Uses pnpm" with neighbour "Uses npm" → SUPERSEDE, the neighbour id, text <= 2,000 chars; same meaning as a neighbour → SKIP; related topic, different fact → ADD; `tokens_used` 5,000,000 today → 429. Delete the test device after and reset the counter.

## Dependencies
- 1 blocks 2. 2 blocks 3.
- 3 needs the user's approval before the deploy.
