# M4 F3. Facts from turns — Design

Source: project `design.md` (Section 3 Extraction, Section 6 SQLite), `requirements.md` FR14, FR16, FR18, FR19, FR30, `milestones.md` M4 F3, M4 F1 and F2 designs and code, M3 F3 to F5 client code.

## Overview
- The daemon turns each pending row of `turn_queue` into 0 to 3 stored facts. It skips, adds or supersedes, and pushes the result.
- A new `Pipeline` thread calls `POST /v1/extract`, compares each candidate with the local index, calls `POST /v1/reconcile` only for the middle band, and writes through `Store::write_local`.

## Components
- `Pipeline` (new `crates/loremaster/src/pipeline.rs`): own thread, own `ureq::Agent` (30 s timeout, above the service's 25 s model limit). It handles one turn at a time, oldest `id` first. It wakes on a trigger from the capture worker after `commit_turn`, at start, and when a retry time is reached. Its target is `config::service_url` and `config::load_credential`, read on each use, with the same refused-token rule as `SyncClient::target`. After a turn writes a row, it calls `sync::Triggers::push`.
- `Recall` (`recall.rs`): new `neighbours(text, scope, project_id)`, which embeds the text and returns the vector and the top 5 live facts. New `write_fact(fact, vector, supersedes, writer)`, which writes the new fact and, if given, the superseded row, in one `write_local`, then updates the index.
- `Index` (`index.rs`): new `nearest_facts(vector, scope, project_id, k)`: live facts only, same `scope`, for project scope the same `project_id`, best `dot` first. It reuses `dot`.
- `Store` (`store.rs`): schema v2 adds `turn_queue.attempts INTEGER NOT NULL DEFAULT 0`. New methods: `next_pending_turn()`, `set_turn_status(id, status)`, `count_failed_attempt(id) -> attempts`.
- Daemon (`server.rs`): starts and stops the `Pipeline` next to `SyncClient`. The capture worker sends the trigger.
- Service (`loremaster-service/src/lib.rs`, follow-up from M4 F2, user decision): `check_reconcile` rejects a neighbour `memory_id` that is not a ULID (`loremaster_core::is_ulid`), with 400.

## Data Model
- `turn_queue.status`: `pending` (default), `extracted` (done), `failed` (never tried again). Rows are kept. M4 F4 reads them for the episode and deletes them.
- `turn_queue.attempts`: the number of counted failures. Migration v1 to v2: `ALTER TABLE turn_queue ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0`, then `user_version = 2`, in one transaction.
- New fact: `memory_id` from `new_ulid()`, `type` fact, `kind` and `scope` from the candidate, `project_id` from the turn row for project scope and empty for user scope, `text` trimmed, `created_at` now, empty `hlc`, `vector` and `model_id` from the `neighbours` call.
- Superseded row: the stored record with `superseded_by` = the new id and an empty `hlc`. `text` is unchanged (project design: text is never edited in place).

## Flow
- 0. The Pipeline waits while there is no login, no `api_url` or no embedding model. Turns stay `pending`.
- 1. `next_pending_turn()`. `POST /v1/extract {turn}`.
- 2. For each candidate: if the trimmed `text` is not 1 to 2,000 chars, drop it (FR18).
- 3. `Recall::neighbours`. Top similarity >= 0.95: skip. No neighbour, or top <= 0.75: add, with no reconcile call. Else: `POST /v1/reconcile` with the candidate and the top 5 as `{memory_id, text}`.
- 4. Decision `add`: write the fact. `skip`: nothing. `supersede X`: if X is still live (not deleted, `superseded_by` empty), write the fact and mark X. If X is gone, write the fact as an add (user decision).
- 5. When all candidates are done: `set_turn_status(id, extracted)`. If a row was written: `Triggers::push`. Next turn.
- Replies (extract or reconcile) that stop the turn. The turn stays `pending`, and the candidates written so far stay:

| Reply | Effect |
|-------|--------|
| No network, timeout, 503 | Retry in 30 s. Not counted. |
| 429 | Retry in 1 h. Not counted. |
| 401 | Wait until the credential changes (as `SyncClient`). Not counted. |
| 500, 502, a 200 body that does not parse | `count_failed_attempt`. At 5: `failed`, then the next turn. Else retry in 30 s. |
| 400, 413 | `failed` at once, then the next turn. |

- A retry extracts the whole turn again. Candidates written on the first try now match their own row at >= 0.95, so they are skipped (user decision).

## Key Decisions
- D1. At most 5 counted failures, then `failed` (user decision). Alt: retry forever. Why: a turn that always gets a bad reply would cost Haiku tokens on every try and hold up the queue. Network, 429, 503 and 401 do not count, because the turn is not at fault, so FR19 holds for every turn the service can process.
- D2. A partial turn is extracted again (user decision). Alt: store the candidates. Why: no new table, and the 0.95 skip makes the retry write nothing twice. Cost: one more extract call, only after a failure.
- D3. A supersede of a target that is gone becomes an add (user decision). Alt: skip. Why: the candidate is the newest fact, and the target is already not live.
- D4. One turn at a time, in one thread. Alt: parallel turns. Why: most turns give no candidates, and the order of turns decides which fact is newer (FR30).
- D5. The neighbours are live facts of the same scope and project only, never episodes. Why: the project design compares within one scope, and an episode is not a fact that a turn can replace.
- D6. The Pipeline reuses the candidate vector from `neighbours` for the new row. Alt: embed again on write. Why: same text, same model, one embedding.
- D7. 429 retries in 1 h. Alt: wait to UTC midnight. Why: the service owns the cap, and one call an hour costs nothing when it is refused before the model call.
- D8. The Pipeline does not wait for the push. Why: the outbox and the sync thread already retry, as for every local write.

## Testing
- `tests/pipeline.rs` with the axum fake service (`tests/common/fake_service.rs`), extended with `/v1/extract` and `/v1/reconcile` scripted replies and request counts, a sandbox home and the real embedding model. Cases: the milestones verify list for M4 F3 (stub service cases), plus 400 and 413 give `failed` at once, five 502 replies give `failed`, a partial turn retried writes no duplicate, and a supersede of a gone target writes an add.
- `store.rs` unit tests: a v1 store opens as v2 with `attempts` 0 and its rows kept.
- `tests/reconcile.rs` (service): a neighbour `memory_id` that is not a ULID gives 400.
- /verify: the first milestones case on the dev stack with real Haiku (approval), the others on a stub service.

## Open Questions
- None.
