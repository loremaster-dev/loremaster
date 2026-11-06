# M3 F1. Push records — Tasks

Input: `design.md` next to this file, project `design.md` (Section 4 Sync, Appendix A), `milestones.md` M3 F1, M2 F2 to F4 designs. Service tests need DynamoDB Local (`infra/local.sh`) and run on the developer machine only.

## 1. A push stores new records with a seq [serial]
- [x] 1.1 In `crates/loremaster-core/src/lib.rs`, add `PushRequest {records: Vec<Record>}` and `PushResponse {applied: Vec<String>}`. Unit test: both round-trip through `serde_json` with the field names in the design.
- [x] 1.2 Add the `memories` table (key `user_id` + `memory_id`, LSI `by_seq` on `seq`, projection ALL): `memories_table` in `Config` from `MEMORIES_TABLE`, the table in `tests/common/mod.rs` `create_tables`, and in `infra/local.sh`. Existing tests still pass, and `infra/local.sh` run twice gives no error.
- [x] 1.3 The auth middleware in `src/lib.rs` puts the caller's `user_id` and `device_id` in the request extensions. `GET` and `DELETE /v1/devices` use the new extension. Existing tests still pass.
- [x] 1.4 `POST /v1/sync/push` in `src/lib.rs`: `BatchGetItem` (consistent, `UnprocessedKeys` sent again), `GetItem users` (consistent), keep newer records, `seq` in request order, `live_count` delta, one `transact` call, 200 `PushResponse`. New `tests/sync_push.rs`, test `new_records_get_unique_seqs`: 2 facts → `applied` has both, `seq` 1 and 2, `live_count` 2, items have the token's `user_id` and the right `evict_key`.

## 2. The newest write wins [serial, needs 1]
- [x] 2.1 Test `same_push_again_applies_nothing`: the same body twice → second `applied` empty, `seq`, `live_count` and `next_seq` unchanged.
- [x] 2.2 Test `older_hlc_is_ignored_and_newer_hlc_wins`: an older `hlc` → not applied, item unchanged. A newer `hlc` → applied, new text, higher `seq`.
- [x] 2.3 Test `tombstone_keeps_only_ids_and_lowers_live_count`: a tombstone for a live record → item has only `user_id`, `memory_id`, `seq`, `hlc`, `writer`, `deleted`. `live_count` falls by 1. A later newer live record raises it by 1 again.
- [x] 2.4 Test `users_do_not_share_memories`: alice and bob push the same `memory_id` → 2 items, each with its own `user_id` and text, `seq` 1 for both users.
- [x] 2.5 `ConditionalCheckFailed` on any transaction item → 409 `{"error": "retry"}`. Test `concurrent_pushes_keep_every_record`: 2 pushes of 5 new records each, run with `tokio::join!`, each sent again on 409 until 200 → 10 items, 10 unique `seq`, `live_count` 10.

## 3. Bad pushes write nothing [parallel, needs 1.4]
- [x] 3.1 Record checks in `src/lib.rs`, 400 naming the first bad `memory_id`. Table test `bad_pushes_write_nothing`: not JSON, 100 records, text of 2,001 chars, empty text, fact with no `kind`, episode with `kind`, project scope with no `project_id`, user scope with `project_id`, `memory_id` not a ULID, `hlc` that does not parse, `writer` not the token's device, `hlc` writer not `writer`, the same `memory_id` twice → 400, `memories` empty, `next_seq` 0. Text of 2,000 chars → 200.
- [x] 3.2 Test `service_sets_user_seq_and_evict_key`: a record with another `user_id`, `seq` 99 and `evict_key` `x` → stored with the token's `user_id`, the server `seq` and the computed `evict_key`. A tombstone with `text` set → stored without it.
- [x] 3.3 Test `empty_push_and_storage_failure`: `{"records": []}` → 200 `applied` empty. Tables missing → 500.

## 4. The dev stack stores pushes [serial, needs 2 and 3] [approval]
- [x] 4.1 In `infra/template.yaml`, add `MemoriesTable` (as 1.2, on demand, `SSEEnabled: true`), env `MEMORIES_TABLE`, and IAM `BatchGetItem` and `PutItem` on it and `GetItem` on `users`. `sam validate --lint` passes.
- [x] 4.2 `sam build` and `sam deploy --config-env dev`. Over HTTPS with a device from `gh auth token`: push 2 records → both applied, the same push again → `applied` empty, `describe-table memories` shows KMS and the LSI. Delete the test items and the device after.

## Dependencies
- 1 blocks 2 and 3. 2 and 3 run in parallel.
- 4 needs 2 and 3, and the user's approval before the deploy in 4.2.
