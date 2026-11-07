# M3 F3. Two devices converge — Tasks

Input: `design.md` next to this file (D1 to D11), project `design.md` (Section 4 Sync, Cadence), `milestones.md` M3 F3, M3 F1 and F2 designs. Client tests use an axum fake service, as `tests/login.rs`, and need no DynamoDB Local.

## 1. `dev seed` queues outbox rows with the device writer [serial]
- [x] 1.1 In `crates/loremaster/src/store.rs`, add `Store::write_local(&mut self, memory: &mut StoredMemory, writer: &str, now_ms: i64)`: one transaction reads `sync_state.hlc` (missing: `Hlc::new(writer)`), ticks it into `memory.record.hlc` (a given `hlc` is observed and kept), sets `writer`, puts the memory and its `outbox` row, writes `hlc` back. Unit test: 2 calls give rising `hlc`s, 2 outbox rows, `sync_state.hlc` = the last one.
- [x] 1.2 In `store.rs`, add `Store::outbox_count`, `Store::outbox_batch(limit) -> Vec<StoredMemory>` (oldest rowid first, rows with no memory removed) and `Store::remove_pushed(&[(memory_id, hlc)])` (deletes only where the memory `hlc` is still the pushed one). Unit tests: batch order, orphan row removed, changed row kept.
- [x] 1.3 In `crates/loremaster/src/dev.rs` `seed`: with `credentials` (`DeviceCredential`), call `write_local` for each memory with `device_id`. With none, keep writer `local` and `put_memories`. Test in `tests/dev_tools.rs`: seed with a saved credential → writer = `device_id`, outbox has each id; seed with none → outbox empty, writer `local`.

## 2. The daemon pushes the outbox [serial, needs 1]
- [x] 2.1 New `crates/loremaster/tests/common/fake_service.rs`: axum `POST /v1/sync/push` and `GET /v1/sync/pull` in memory, per-user `seq`, newer-`hlc` wins, pages of N records with `more`, token check against known devices, scripted replies (400 with an id, 401, 409, 500, delay). Used only by tests.
- [x] 2.2 New `crates/loremaster/src/sync.rs`: `SyncClient` with a `Cadence {outbox_check, period, retry}` and `push_outbox`: reads API URL (as `login.rs`) and `credentials` each cycle, `ureq` agent with 10 s timeout, batches of 99 with no `vector`, `model_id`, `seq`, `user_id`, `evict_key`, `remove_pushed` on 200, loop until empty. Export from `lib.rs`. Test in new `tests/sync.rs`: 150 rows → 2 pushes, outbox empty, fake has 150 records with no vector fields.
- [x] 2.3 In `sync.rs`, the thread loop: `mpsc::Receiver<Trigger>` with `recv_timeout`, push + pull on start, push when `outbox_count` > 0 each `outbox_check`, push + pull each `period`, stop on channel close. In `server.rs` `run_async`, start it with 5 s, 5 min, 30 s, and stop it in the ordered shutdown. Test in `tests/sync.rs`: daemon running, `dev seed` on its home → fake has the record within 10 s, outbox empty.
- [x] 2.4 No API URL or no `credentials`: `SyncClient` sends nothing and logs once. Test: daemon with a seeded outbox row and no credentials → fake gets no request, recall still works.

## 3. Pulled records are recalled on the other home [serial, needs 2]
- [x] 3.1 In `crates/loremaster/src/recall.rs`, add `Recall::apply_pulled(&self, page: PullResponse) -> Result<usize, RecallError>`: keep records with no local row or a higher `hlc`, embed kept live ones if the embedder is ready, one transaction for the rows, `sync_state.cursor` and the observed `hlc`, then index insert or `mark_not_live` in place. Unit tests: older `hlc` ignored, tombstone not recalled, no embedder → row with no vector that `set_embedder` fills, no index rebuild.
- [x] 3.2 In `sync.rs`, `pull`: cursor from `sync_state` (missing 0), `GET /v1/sync/pull`, `apply_pulled`, loop while `more`. Test in `tests/sync.rs`: fake with 25 records, page size 10 → 3 pages, cursor saved, 25 rows with `seq`, `vector` and `model_id`.
- [x] 3.3 In `server.rs` SessionStart handler, send `Trigger::Pull` and do not wait. Test: two sandbox homes A and B, one fake, 2 devices of one user: seed on A, SessionStart on B → recall probe on B returns the fact, B's row has `vector` and `model_id`.

## 4. Failures and outages keep every record [parallel, needs 3]
- [x] 4.1 Push 400 → named id removed from the outbox and logged, the rest pushed. 401 → outbox kept, log "run loremaster login". Test each in `tests/sync.rs` with the scripted fake.
- [x] 4.2 Push 409 → retried up to 3 times. Pull 409 → retried after 1 s, up to 3 times. Test: fake gives 409 twice, then 200 → record stored, no record lost.
- [x] 4.3 Network error, timeout or 5xx → retry mode: push + pull every `retry`, no 5-s push. First success ends it. Test with short `Cadence`: fake stopped, seed → outbox 1 row, recall works; fake back → outbox empty within `retry`.
- [x] 4.4 Credentials saved after the daemon starts → next cycle syncs. Kill -9 the daemon with an outbox row, restart → record reaches the fake. Test both in `tests/sync.rs`.

## 5. `dev bench` runs with sync on [parallel, needs 3]
- [x] 5.1 In `dev.rs` `bench`, copy `config.toml` and `credentials` (mode 0600) from the real home into the temp home if present. Bench memories keep writer `bench` and no outbox rows. Test in `tests/dev_tools.rs`: temp home gets both files with the same bytes and mode 0600; outbox empty after seeding.

## Dependencies
- 1 blocks 2. 2 blocks 3.
- 4 and 5 run in parallel after 3.
- /verify runs the two-homes cases on the real service (`cargo lambda watch` and DynamoDB Local). Deploys to dev need the user's approval.
