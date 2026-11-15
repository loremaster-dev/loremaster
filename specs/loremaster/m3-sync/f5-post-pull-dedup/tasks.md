# M3 F5. Post-pull dedup — Tasks

Input: `design.md` next to this file (D1 to D7), `milestones.md` M3 F5, project `design.md` Section 3 and Section 4, M3 F3 and F4 code. The pull, the in-place index update, `add_vectors` and `Store::write_local` are F3 code; F5 adds `Index::near_duplicates`, the dedup step in `Recall`, and the device id for `set_embedder`. Tests use the axum fake service in `tests/common/fake_service.rs` and two sandbox homes, with `SyncClient::push_outbox` and `SyncClient::pull` called in order, as F4.

## 1. The index finds near-identical live facts [serial]
- [x] 1.1 In `crates/loremaster/src/index.rs`, add `DUPLICATE_SIMILARITY` (0.95) and `Index::near_duplicates(&self, memory_id: &str) -> Vec<&str>`: other live facts with dot product >= `DUPLICATE_SIMILARITY` and the same `scope`, and for project scope the same `project_id`. Unknown or not-live `memory_id`, or not a fact: empty.
- [x] 1.2 Unit tests in `index.rs` with hand-made unit vectors: 0.96 found, 0.94 not; same vector in the other scope, in another `project_id`, as an episode, as a superseded fact, or marked not live: not found; the memory itself and an unknown id: not found.

## 2. A pulled duplicate is tombstoned on this device [serial, needs 1]
- [x] 2.1 In `crates/loremaster/src/recall.rs`, add the private dedup step (design Flow 2 to 4): for each given live fact, each id from `near_duplicates` gives a pair; the larger `memory_id` gets a tombstone through `Store::write_local` (`deleted = true`, empty `hlc`, no text or metadata) and `Index::mark_not_live`. `Recall::apply_pulled` runs it on the stored facts with a vector, after the index update.
- [x] 2.2 Test in `tests/sync.rs`: a local fact and a pulled fact with the same text and scope, the pulled one with the larger ULID → the pulled row is a tombstone in the outbox, the local row stays live, a recall probe returns the text once.
- [x] 2.3 Test: the same with the pulled fact with the smaller ULID → the local row is the tombstone in the outbox, the pulled row stays live.
- [x] 2.4 Test: the same text in user and project scope, in two projects, as an episode and a fact, and unrelated texts in one scope → nothing tombstoned, outbox empty.
- [x] 2.5 Test: two pulled facts with the same text in one page → the larger ULID is the tombstone.

## 3. A duplicate that gets its vector late is tombstoned too [serial, needs 2]
- [x] 3.1 In `recall.rs`, `Recall::set_embedder` takes `writer: Option<&str>` and passes it to `add_vectors`; `add_vectors` runs the dedup step on the facts it embeds when `writer` is `Some`. The late-model path in `apply_pulled` passes its `writer`. Update the callers in `tests/` and `dev.rs` to pass `None` where no device is logged in.
- [x] 3.2 In `crates/loremaster/src/server.rs`, `load_model_in_background` also takes the credentials path (`paths.credentials()` at its call in the daemon start), reads `config::load_credential` after the model loads, and passes its `device_id` to `set_embedder`, or `None` with no login.
- [x] 3.3 Test in `tests/sync.rs`, after `rows_pulled_before_the_model_loads_get_vectors_when_it_does`: a local fact and a pulled duplicate with no model, then `set_embedder(embedder(), Some(device_id))` → the larger ULID is a tombstone in the outbox. With `None` → both stay live.

## 4. Two devices end with the same one live fact [serial, needs 3]
- [x] 4.1 Test `the_same_fact_from_two_devices_ends_as_one_on_both` in `tests/sync.rs`: A and B each write the same text and scope with no sync. A pushes, B pushes, A pulls, B pulls, A pushes, B pushes, A pulls, B pulls → both stores have the smaller ULID live and the larger as a tombstone, the fake has the larger as a tombstone, both outboxes are empty.

## Dependencies
- 1 blocks 2. 2 blocks 3. 3 blocks 4.
- /verify runs the three M3 F5 cases on the local service (DynamoDB Local, `cargo lambda watch`) with `dev seed` on two homes and two daemons.
