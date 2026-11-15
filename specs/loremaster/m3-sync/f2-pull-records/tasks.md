# M3 F2. Pull records — Tasks

Input: `design.md` next to this file, project `design.md` (Section 4 Sync and Bootstrap, Appendix A), `milestones.md` M3 F2, M3 F1 design. Service tests need DynamoDB Local (`infra/local.sh`) and run on the developer machine only.

## 1. A pull gives the records in seq order [serial]
- [x] 1.1 In `crates/loremaster-core/src/lib.rs`, add `PullResponse {records: Vec<Record>, next_cursor: i64, total: i64, more: bool}`. Unit test: round-trips through `serde_json` with the field names in the design.
- [x] 1.2 Move `device`, `hlc`, `fact`, `tombstone`, `push` and `push_ok` from `tests/sync_push.rs` to `tests/common/mod.rs`. Existing tests still pass.
- [x] 1.3 `GET /v1/sync/pull` in `src/lib.rs`: parse `cursor`, in parallel a consistent `Query` on LSI `by_seq` (`seq > :cursor`, ascending, no `Limit`) and a consistent `GetItem users` for `live_count`, items to `Record`, 200 `PullResponse`. New `tests/sync_pull.rs`, test `pull_from_zero_gives_records_in_seq_order`: 3 pushed facts, `cursor=0` → 3 records in `seq` order with all stored attributes (`user_id` and `evict_key` included), `next_cursor` 3, `total` 3, `more` false.
- [x] 1.4 Test `pull_from_last_cursor_is_empty`: `cursor=<next_cursor>` → no records, `next_cursor` the same cursor, `more` false.

## 2. Pages and late changes reach the reader [parallel, needs 1]
- [x] 2.1 Test `pages_over_one_megabyte_give_each_record_once`: 600 facts of 2,000 characters, pushed 99 at a time. First page `more` true. Pages followed by `next_cursor` until `more` false → 600 records, each `memory_id` once, `seq` rising.
- [x] 2.2 Test `late_push_after_cursor_is_pulled`: pull to the end, then a second device pushes a newer `hlc` for an old record and a new record → the next pull gives both, with their new `seq`.
- [x] 2.3 Test `tombstone_is_pulled_and_total_counts_live_records`: 3 facts, 1 deleted → pull gives the tombstone with only `user_id`, `memory_id`, `seq`, `hlc`, `writer`, `deleted`, and `total` 2.

## 3. Bad pulls and other users [parallel, needs 1.3]
- [x] 3.1 Table test `bad_cursor_gives_400`: no `cursor`, `cursor=` empty, `abc`, `-1`, `1.5` → 400 `{"error"}`.
- [x] 3.2 Test `users_do_not_see_each_others_records`: alice and bob push → each pull gives only that user's records, and `total` counts only that user's records.
- [x] 3.3 Test `pull_without_token_and_storage_failure`: no token → 401. Tables missing → 500.

## 4. The dev stack serves pulls [serial, needs 2 and 3] [approval]
- [x] 4.1 In `infra/template.yaml`, add IAM `dynamodb:Query` on `${MemoriesTable.Arn}/index/*`. `sam validate --lint` passes.
- [x] 4.2 `sam build` and `sam deploy --config-env dev`. Over HTTPS with a device from `gh auth token`: push 2 records, `cursor=0` → both in `seq` order, `total` 2. `cursor=<next_cursor>` → empty, `more` false. Delete the test items and the device after.

## Dependencies
- 1 blocks 2 and 3. 2 and 3 run in parallel.
- 4 needs 2 and 3, and the user's approval before the deploy in 4.2.
