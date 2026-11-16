# M4 F3. Facts from turns — Tasks

Input: `design.md` next to this file (D1 to D8), project `design.md` (Section 3 Extraction, Section 6 SQLite), `milestones.md` M4 F3, `requirements.md` FR14, FR16, FR18, FR19, FR30. Reuses M4 F1 and F2 endpoints and the M3 client code: `Store::write_local`, the outbox, `sync::Triggers::push`, `Index`, `Recall`, `Embedder`. Client tests use the axum fake service in `crates/loremaster/tests/common/fake_service.rs`, a sandbox home and the real embedding model (`common::embedder()`), and call the Pipeline step by step, as the M3 sync tests call `push_outbox` and `pull`. Service tests need DynamoDB Local (`infra/local.sh`).

## 1. The store tracks each turn's status and failed attempts [serial]
- [x] 1.1 In `crates/loremaster/src/store.rs`, schema v2: `SCHEMA_VERSION` 2, a new store gets `turn_queue.attempts INTEGER NOT NULL DEFAULT 0`, and a v1 store runs `ALTER TABLE turn_queue ADD COLUMN attempts ...` and `user_version = 2` in one transaction. Unit test: a v1 store with 2 queued turns opens as v2 with both rows kept and `attempts` 0; a v3 store still gives `SchemaTooNew`.
- [x] 1.2 In `store.rs`, add `next_pending_turn()` (oldest `pending` row by `id`: id, session_id, project_id, turn), `set_turn_status(id, TurnStatus)` (`pending`, `extracted`, `failed`) and `count_failed_attempt(id) -> i64`. Unit test: `extracted` and `failed` rows are not returned; the count goes 1, 2, 3.

## 2. A turn adds new facts and skips known ones [serial, needs 1]
- [x] 2.1 In `crates/loremaster/src/index.rs`, add `Index::nearest_facts(vector, scope, project_id, k) -> Vec<(&str, f32)>`: live facts only, same scope, same `project_id` for project scope, best `dot` first, at most `k`. Unit tests: episodes, superseded rows, the other scope and another project are not returned; order and `k` hold.
- [x] 2.2 In `crates/loremaster/src/recall.rs`, add `Recall::neighbours(text, scope, project_id)` (embeds the text, returns the vector and the top 5 from `nearest_facts`, `None` before the model loads) and `Recall::write_fact(fact, vector, supersedes: Option<&str>, writer)` (one `write_local` for the new fact and, if given, the stored row with `superseded_by` set and an empty `hlc`; then the index insert and `mark_not_live`). Unit tests through `Recall`: a written fact is in the outbox and the index; the superseded row keeps its text.
- [x] 2.3 In `tests/common/fake_service.rs`, add `Route::Extract` and `Route::Reconcile` with `POST /v1/extract` and `POST /v1/reconcile`: device token check, scripted replies with `script`, request counts with `requests`, and the last request body for each route. Existing sync tests pass.
- [x] 2.4 New `crates/loremaster/src/pipeline.rs`: `Pipeline::new(recall, paths)` and `Pipeline::process_next()`, which takes `next_pending_turn`, calls extract, drops a candidate whose trimmed text is not 1 to 2,000 chars, skips at top similarity >= 0.95, adds with no reconcile call at <= 0.75 or with no neighbour, and sets `extracted`. It returns whether a turn was handled.
- [x] 2.5 New `crates/loremaster/tests/pipeline.rs`: a candidate with no near fact → a new row of the candidate's kind and scope, the row in `outbox`, reconcile count 0, turn `extracted`; a candidate with similarity >= 0.95 to a same-scope fact → no new row; a project-scope candidate → the row has the turn's `project_id`, a user-scope one has none; a candidate of 2,001 chars → no row; an extract reply with no candidates → turn `extracted`, no row.

## 3. A related candidate is reconciled, and the newer fact wins [serial, needs 2]
- [x] 3.1 In `pipeline.rs`, a top similarity between 0.75 and 0.95 calls reconcile with the candidate and the top 5 as `{memory_id, text}`. `add` writes the fact, `skip` writes nothing. Test: two texts chosen by hand with a similarity in the band → one reconcile call with those neighbours; scripted `add` → new row; scripted `skip` → no row.
- [x] 3.2 `supersede X` with X live: `write_fact` with `supersedes = X`. Test: new row, X has `superseded_by` = the new id and its text unchanged, both in `outbox`; a recall probe through `Recall::for_prompt` returns only the new fact.
- [x] 3.3 `supersede X` with X deleted or already superseded: write the fact as an add. Test: new row, X unchanged.
- [x] 3.4 In `crates/loremaster-service/src/lib.rs` `check_reconcile`, reject a neighbour `memory_id` that is not `loremaster_core::is_ulid` with 400. Add the row to `bad_bodies_are_400` in `crates/loremaster-service/tests/reconcile.rs`.

## 4. Failed calls keep turns, and poison turns stop [serial, needs 3]
- [x] 4.1 In `pipeline.rs`, map each extract or reconcile reply as in the design table: retry delay or wait for a new credential, count with `count_failed_attempt`, `failed` at 5 counted failures or at once on 400 and 413. `process_next` reports when to try again. Delays come from a `Cadence`-like struct, so tests use short ones.
- [x] 4.2 Table test in `tests/pipeline.rs`: no service (closed port), 503 and 429 → turn `pending`, `attempts` 0; 401 → `pending`, and no request until the credential file changes; five 502 replies → `failed`, then the next turn is handled; 400 and 413 → `failed` at once; a 200 body that does not parse counts as one failure.
- [x] 4.3 Test: a turn with 2 candidates, the first written, the reconcile for the second gets 503 → turn `pending`; the retry extracts again and writes no second copy of the first candidate.

## 5. The daemon runs the Pipeline [serial, needs 4]
- [x] 5.1 In `pipeline.rs`, `Pipeline::spawn(cadence) -> PipelineHandle` with a trigger and a stop, as `SyncClient::spawn`. It waits while there is no login, no `api_url` or no embedding model, and calls `Triggers::push` after a turn writes a row. In `crates/loremaster/src/server.rs`, start it next to `SyncClient`, send the trigger after each `commit_turn` in `Capturer`, and stop it before the sync thread.
- [x] 5.2 Daemon test in `tests/pipeline.rs`: a capture with the fake service down, then a daemon restart → turn `pending`; the service back → turn `extracted`, the fact pushed to the fake.
- [x] 5.3 With the user's approval, on the dev stack with real Haiku: a Stop hook on a turn that states a preference, a new session, a relevant prompt → the recall probe returns the fact. Delete the test device after and reset the counter.

## Dependencies
- 1 blocks 2. 2 blocks 3. 3 blocks 4. 4 blocks 5.
- 3.4 changes only the service and can run at any time.
- 5.3 needs the user's approval, and a dev deploy only if 3.4 is not deployed yet.
