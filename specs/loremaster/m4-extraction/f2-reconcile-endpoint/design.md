# M4 F2. Reconcile endpoint — Design

Source: project `design.md` (Section 3 Extraction, Section 5 Service), `requirements.md` FR18, FR19, FR30, `milestones.md` M4 F2, M4 F1 design and code.

## Overview
- `POST /v1/reconcile` takes one candidate fact and its 1 to 5 nearest same-scope neighbours, and returns one decision: ADD, SUPERSEDE one neighbour with a new text, or SKIP.
- Same path as M4 F1: Claude Haiku 5.5 on the Messages API, one forced tool, the prompt on the server, the same daily token counter.

## Components
- `loremaster-core`: new `ReconcileRequest {candidate: Candidate, neighbours: Vec<Neighbour>}`, `Neighbour {memory_id: String, text: String}`, `ReconcileResponse`, an enum tagged by `decision`: `Add`, `Supersede {memory_id: String, text: String}`, `Skip`. `Pipeline` (M4 F3) uses them.
- `loremaster-service` `src/lib.rs`: route `POST /v1/reconcile` behind the auth middleware. Reuses `tokens_used`, `add_tokens`, `DAILY_TOKEN_CAP` and the `ApiError` mapping of `LlmError`.
- `src/llm.rs`: the Messages call of `extract` moves into one shared function that takes the system prompt, the tool definition and the user content, and returns `Reply`. `extract` and the new `reconcile` call it. New `decision(tool_input, neighbours)` checks the reply.
- Prompt: `crates/loremaster-service/prompts/reconcile.md`, with `include_str!`. Rules: SUPERSEDE when the candidate updates or contradicts a neighbour (the candidate is newer and wins, FR30), and the text may combine both. SKIP when a neighbour already says the same. ADD otherwise. The wording is tuned in M4 F5.
- No new table, attribute, env var, IAM or infra change.

## Interfaces
- `POST /v1/reconcile`, device token required.
  - Request: `{"candidate": {"text", "scope", "kind"}, "neighbours": [{"memory_id", "text"}]}`.
  - 200: `{"decision": "add"}`, `{"decision": "skip"}` or `{"decision": "supersede", "memory_id": "<a neighbour id>", "text": "<new fact>"}`. `text` is trimmed, 1 to 2,000 chars.
  - 400: body does not parse; candidate text not 1 to 2,000 chars; neighbours not 1 to 5; a neighbour text not 1 to 2,000 chars; two neighbours with the same `memory_id`.
  - 429: daily cap, as F1. No model call.
  - 502: as F1, and also a reply that does not pass the check in Flow step 6 (user decision).
  - 503: Messages API 429 or 529, as F1.
  - 401, 500: as M2.
- No 413: the 400 limits keep the input under about 4k tokens.
- Messages call: as F1, with `system` = the reconcile prompt, `max_tokens` 1,024 (one fact of 2,000 chars), and the request body JSON as the one user message.
  - Tool `record_decision`, forced with `tool_choice`. `input_schema`: `decision` enum `add|supersede|skip` (required), `memory_id` string, `text` string `maxLength` 2,000.

## Flow
- 1. Auth middleware gives `user_id`.
- 2. Parse and check the body. Fail gives 400.
- 3. Cap check, as F1. Over gives 429.
- 4. Messages call with a 25 s timeout. 429 or 529 gives 503. Other failure gives 502.
- 5. Add `usage.input_tokens` to the counter, as F1, before the reply check.
- 6. Check the tool input: `decision` parses; for `supersede`, `memory_id` is one of the request neighbours and `text` trimmed is 1 to 2,000 chars. For `add` and `skip`, other fields are ignored. Fail: log, 502.
- 7. 200 with the decision.

## Testing
- As F1: router in process, DynamoDB Local, the fake Messages API through `ANTHROPIC_API_URL`. New markers in the candidate text select the reply: add, skip, supersede, supersede with an unknown id, supersede with empty or too long text, no tool call, 429, 529.
- Tests check the 400 limits, 429 with no model call (fake call count), the counter after each model reply, including a reply that gives 502.
- `extract` tests stay unchanged and pass after the shared-call change.
- Dev stack verify cases (real Haiku) need approval, as in `milestones.md`.

## Key Decisions
- D1. A reply that is not valid gives 502 (user decision). Alt: fall back to ADD or SKIP. Why: the device keeps the turn pending and retries, and no wrong supersede, lost fact or duplicate is written.
- D2. One SUPERSEDE target for each decision. Alt: a list of neighbour ids. Why: the project design returns one decision with one id; a candidate that matches two old facts supersedes the closest, and the other stays until a later turn.
- D3. Neighbours carry only `memory_id` and `text`. Alt: also `kind` and age. Why: the project design sends (id, text); the device sends only same-scope neighbours, and the candidate is always the newest.
- D4. Bound the input with 400 limits (5 neighbours, 2,000 chars each), no token estimate. Alt: 413 on an estimate, as extract. Why: the limits already fix the maximum size, and the device never sends more than its top 5.
- D5. One shared Messages call in `llm.rs` for both endpoints. Alt: a copy for each endpoint. Why: the HTTP, error and usage code is the same; only the prompt, the tool and the check differ. M4 F4 uses it too.
- D6. The counter counts tokens of a reply that fails the check. Why: Anthropic charges for the call; same rule as F1.

## Open Questions
- None.
