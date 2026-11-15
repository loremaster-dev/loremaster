# M3 F7. 50k cap — Tasks

Input: `design.md` next to this file (D1 to D9), `milestones.md` M3 F7, project `design.md` Section 5, M3 F1 and F2 code. `evict_key` on live items, the push transaction and `transact` are F1 code; F7 adds the GSI, the cap setting and the eviction step after a push. Service tests run on DynamoDB Local with the helpers in `crates/loremaster-service/tests/common/mod.rs`. No client change.

## 1. The memories table has the eviction index and the service has a cap [serial]
- [x] 1.1 In `crates/loremaster-service/src/lib.rs`, add `Config.memory_cap: i64`. `Config::from_env` reads `MEMORY_CAP`, default 50,000. Unit test: unset gives 50,000; `5` gives 5.
- [x] 1.2 In `infra/template.yaml`, add GSI `by_evict_key` to `MemoriesTable`: HASH `user_id`, RANGE `evict_key` (attribute definition S), projection KEYS_ONLY. `sam validate` passes.
- [x] 1.3 In `infra/local.sh`, add the same GSI to `create memories`. The script creates the table on DynamoDB Local.
- [x] 1.4 In `tests/common/mod.rs`, add the GSI to `create_tables`, and `Service::start_with_cap(cap)` that sets `memory_cap`. `Service::start` uses 50,000. All existing service tests pass.

## 2. A push over the cap evicts the oldest memories, in class order [serial, needs 1]
- [x] 2.1 In `lib.rs`, add the eviction step that `push_records` calls after a successful push transaction (design Flow steps 2 to 6). The reply stays the F1 `PushResponse`.
- [x] 2.2 New test file `crates/loremaster-service/tests/memory_cap.rs`, cap 5: 5 live + 1 new with one replaced fact, one episode, one other fact → the replaced fact is a tombstone with `writer = server`, `live_count` = 5, `next_seq` counts the tombstone.
- [x] 2.3 Test: no replaced facts left → the oldest episode is evicted; no episodes left → the oldest other fact is evicted. Inside one class, the lowest `created_at` goes first.
- [x] 2.4 Test: 4 live + 1 new → nothing evicted, `live_count` = 5. A push that adds 3 over the cap → exactly 3 tombstones.
- [x] 2.5 Test: the evicted memory has a stored `hlc` with a wall time far ahead of now → the tombstone `hlc` sorts above it. A pull from the cursor before the push returns the tombstones with `seq` after the pushed records.

## 3. A failed or contended eviction keeps the push [serial, needs 2]
- [x] 3.1 In `lib.rs`, a cancelled eviction transaction tries again from the query with fresh reads of `next_seq` and `live_count`, at most 3 tries in all. Still failing, or any storage error: `tracing::error!` and reply 200 with the push result.
- [x] 3.2 Test in `memory_cap.rs`: a memories table with no `by_evict_key` → a push over the cap gets 200 with all records applied, and nothing is evicted.
- [x] 3.3 Test: concurrent pushes from 2 devices that each take the user over the cap → each push gets 200, and at the end `live_count` = cap and the live items are the newest by class order.
- [x] 3.4 Test: a backlog left by a failed eviction (cap 5, 110 live items written directly to DynamoDB Local, `live_count` = 110) → the next push evicts 99, and the push after it brings `live_count` to the cap.

- [x] 3.5 In `lib.rs`, the eviction chooses from a strongly consistent `BatchGetItem` (batches of 100) of `excess + 99` index rows plus this push's live records: keeps items with an `evict_key`, sorts by it, takes `excess`; the tombstone condition uses the read `hlc`.
- [x] 3.6 Unit test in `lib.rs` for the choice: a candidate that the read shows as a tombstone is not chosen, a replaced fact sorts before an older episode, at most `excess` are chosen.

## 4. The dev stack has the eviction index [serial, needs 3]
- [x] 4.1 With the user's approval, deploy the dev stack. `aws dynamodb describe-table` shows `by_evict_key` ACTIVE with projection KEYS_ONLY. A push on dev returns 200.

## Dependencies
- 1 blocks 2. 2 blocks 3. 3 blocks 4.
- /verify runs the M3 F7 cases on the local service (DynamoDB Local, `cargo lambda watch`): the class-order and no-eviction cases with `MEMORY_CAP=5`, one case at 50,000 live + 1 new, and a device that pulls an eviction and no longer recalls it.

## Open Questions
- None. A `MEMORY_CAP` that is not a positive number stops the service at start (user decision).
