# M4 F4. Episodes — Design

Source: `requirements.md` next to this file (FR1 to FR17), project `design.md` (Section 2 `SessionTracker`, Section 3 Episode, Section 5 `POST /v1/episode`), `milestones.md` M4 F4, M4 F1 to F3 designs and code.

## Overview
- When a session ends (SessionEnd, or 50 min with no turn), the daemon turns the session's queued turns into 0 or 1 project-scoped episode, pushes it, and deletes the turns.
- The F3 `Pipeline` thread does it after fact extraction, with a new `POST /v1/episode` on Haiku. The session state lives in SQLite, so no timer is lost on restart.

## Components
- Service `POST /v1/episode` (`loremaster-service/src/lib.rs`): behind the auth middleware. Same cap check, token counter and `ApiError` mapping as extract.
- `llm.rs`: new `episode(...)` on the shared `call_tool`. Tool `record_episode`, forced. Prompt `prompts/episode.md` (`include_str!`): fill the 3 fields; `task_attempted` false for a session with no task (pleasantries, a general question); FR15 junk rules. `max_tokens` 1,024. Wording tuned in M4 F5.
- `loremaster-core`: `EpisodeRequest {turns: Vec<String>}`, `EpisodeResponse {episode: Option<String>}`.
- `Store` (`store.rs`): schema v3 adds `sessions.episode_attempts INTEGER NOT NULL DEFAULT 0`. `commit_turn` also sets `ended = 0`. New methods:
  - `due_episode(now_ms, idle_ms) -> Option<DueEpisode {session_id, project_id, turns: Vec<(id, turn)>}>`: the oldest session with 1 or more `turn_queue` rows, none of them `pending`, and `ended = 1` or `last_turn_at <= now - idle`. Rows by `id`.
  - `next_episode_at(idle_ms) -> Option<i64>`: earliest `last_turn_at + idle` of a session with queued rows and `ended = 0`.
  - `count_episode_failure(session_id) -> i64`.
  - `finish_episode(session_id, turn_ids, episode: Option<&StoredMemory>, writer)`: one transaction: the episode row and its `outbox` row if given, delete the `turn_ids`, `episode_attempts = 0`.
- `Recall` (`recall.rs`): new `write_episode(text, project_id, turn_ids, session_id, writer)`: embeds the text, calls `finish_episode`, inserts into the index.
- `Pipeline` (`pipeline.rs`): `process_next` handles a pending turn first (F3), then a due episode, else `Idle` until the next due time. `Cadence` gets `episode_idle` (DAEMON: 50 min).
- `Capturer` (`server.rs`): wakes the Pipeline after `end_session` too.
- `SessionTracker` (project design) is this state: `sessions.last_turn_at` and `ended`, read by `due_episode` and `next_episode_at`. No in-memory timer.

## Interfaces
- `POST /v1/episode`, device token required.
  - Request: `{"turns": ["<prepared turn>", ...]}`, oldest first.
  - 200: `{"episode": "Attempted: ...\nWorked: ...\nFailed: ..."}`, 1 to 2,000 chars trimmed, or `{"episode": null}` when `task_attempted` is false.
  - 400: body does not parse, no turns, an empty turn. 413: sum of `estimate_tokens` over the turns > 100,000. 429: daily cap, no model call. 503: Messages API 429 or 529. 502: other model failure, no tool call, or a reply that fails the check below. 401, 500: as M2.
- Tool `record_episode` `input_schema`: `task_attempted` boolean (required), `attempted`, `worked`, `failed` strings, each `maxLength` 600. Check: if `task_attempted`, `attempted` is not empty and the joined text is 1 to 2,000 chars. `worked` or `failed` empty gives `none`.

## Flow
- 1. `process_next`: F3 path while `next_pending_turn` gives a row.
- 2. Else `due_episode(now, 50 min)`. None: `Idle`, and `run` waits until `next_episode_at` or `Cadence.idle`, whichever is first.
- 3. Turn selection (FR9): keep the first turn, then add turns newest first while the sum of `estimate_tokens` stays <= 100,000. Send them oldest first. Each prepared turn is at most 32k tokens, so the first turn always fits.
- 4. `POST /v1/episode`. Reply `episode` text: `Recall::write_episode` (type `episode`, scope `project`, the session's `project_id`, no `kind`, `created_at` now, empty `hlc`). Reply `null`: `finish_episode` with no episode. Both delete exactly the rows sent and selected, including the dropped middle ones. Then `Triggers::push` if a row was written.
- 5. Errors:

| Reply | Effect |
|-------|--------|
| No network, timeout, 503 | Retry in 30 s. Not counted. |
| 429 | Retry in 1 h. Not counted. |
| 401 | Wait for a new credential, as F3. |
| 500, 502, a 200 body that does not parse | `count_episode_failure`. At 3: `finish_episode` with no episode (user decision). Else retry in 30 s. |
| 400, 413 | `finish_episode` with no episode at once. |

- Resume: `upsert_session` and `commit_turn` set `ended = 0`; the old rows are gone, so the new turns make the next episode (FR5). SessionEnd, then 50 min: the rows are gone, so no second episode (FR12).
- A session with no turns has no rows, so `due_episode` never returns it (FR1).

## F3 follow-ups in this feature (user decision)
- #2. `extract` decides all candidates of a turn first, then writes them in one `write_local` with `set_turn_status(extracted)`. A failed call in the middle writes nothing, so a retry cannot add a near copy. A second supersede of a target already superseded in the same turn becomes an add (F3 D3). This replaces F3 D2 ("the 0.95 skip makes the retry write nothing twice").
- #1. `run` keeps the end time of a `Retry(Unavailable)` or `Retry(DailyCap)` pause. A wake before that time does not start a call. A wake still ends an `Idle` wait.

## Key Decisions
- D1. Idle time 50 min (user decision). Alt: 60 min as the project design says. Why: 50 min + call + push + a 5 min pull is about 56 min, inside FR3. The project design text changes to 50 min.
- D2. Episodes run in the Pipeline thread, after the session's turns leave `pending`. Alt: own thread. Why: FR13 holds by order, and one thread keeps one 429 and login state.
- D3. Session state in SQLite, due time computed from `last_turn_at`. Alt: in-memory timer per session. Why: FR17 needs restart safety, and the 50 min count from the last turn comes for free.
- D4. The model fills 3 fields and the service joins them. Alt: one free text field. Why: FR4 needs all 3 parts, and `task_attempted` gives FR3 a clear signal.
- D5. The device picks the turns that fit; the service only checks 100k with 413. Alt: the service trims. Why: the device knows the turn order, and a smaller request costs less to send.
- D6. 400 and 413 give up at once, as in F3. Why: the same request can never pass. Only 500, 502 and a bad body count toward 3.
- D7. Delete only the rows that were selected. Why: a row added during the call belongs to the next episode.
- D8. The project ID is the session row's `project_id`, set at the first event and never changed (answers BRD OQ1). Why: every queued turn of the session already carries it.

## Testing
- Service `tests/episode.rs`, as `tests/reconcile.rs`: fake Messages API markers for an episode, `task_attempted` false, an empty `attempted`, no tool call, 429, 529; the 400 and 413 limits; 429 with no model call; the counter after each model reply.
- Client `tests/pipeline.rs`, fake service `Route::Episode`, `Cadence` with short times: the milestones verify list (stub cases), plus 3 bad replies give no episode and delete the turns, 400 at once, a pending turn holds the episode back, a daemon restart keeps a due episode, a wake does not end a 429 pause, a failed reconcile on the second candidate writes nothing.
- `store.rs` unit tests: v2 to v3 migration, `due_episode` and `next_episode_at` rules.
- /verify: the first milestones case on the dev stack with real Haiku (approval). The others on a stub service.

## Open Questions
- None.
