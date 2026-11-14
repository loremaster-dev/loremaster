# M3 F7. 50k cap — Design

Source: project `design.md` (Section 4 Sync, Section 5 50k cap, D13, Appendix A), `requirements.md` FR32, FR33, `milestones.md` M3 F7, M3 F1 and F2 designs.

## Overview
- When a push takes a user above the cap, the service writes tombstones for the oldest live memories, in class order, until the user is at the cap. Devices get them on their next pull.
- After the push transaction, one query on a new sparse GSI ordered by `evict_key`, a strongly consistent read of the candidates, then a second transaction that writes the tombstones.

## Components
- `loremaster-service` (`crates/loremaster-service/src/lib.rs`): `push_records` calls a new eviction step after a successful push. `Config` gets `memory_cap`.
- `infra/template.yaml`, `infra/local.sh`: GSI `by_evict_key` on the memories table.
- No client change: devices pull a `server` tombstone like any other (M3 F3, F4).

## Data Model
- GSI `by_evict_key`: HASH `user_id`, RANGE `evict_key`, projection KEYS_ONLY. Sparse: only live items have `evict_key` (F1).
- `evict_key` order (F1): `1#` replaced facts, `2#` episodes, `3#` other facts, then `created_at`. So ascending order is the eviction order.
- Eviction tombstone: as F1 tombstones (keys, `seq`, `hlc`, `writer`, `deleted: true`), `writer = server`.
- `Config.memory_cap: i64` from env `MEMORY_CAP`, default 50,000. A value that is not a positive whole number, an empty value included, stops the service at start, as a missing table variable does.

## Flow
- 1. Push transaction as F1. It fails: nothing more.
- 2. `live = live_count before + live_delta`. `live <= memory_cap`: reply 200. Nothing is evicted (FR32).
- 3. `excess = min(live - memory_cap, 99)`. Query `by_evict_key`: `user_id = :u`, ascending, `Limit = excess + 99`, keys only.
- 4. Candidates: the query ids plus the ids of this push's live records. Read them with strongly consistent `BatchGetItem` (`memory_id`, `hlc`, `evict_key`), in batches of 100. Keep the items that have an `evict_key`, sort by `evict_key`, take `excess`.
- 5. For each item, `hlc = Hlc::new("server")`, observe the item `hlc`, tick with the wall clock.
- 6. One `TransactWriteItems`:
  - users update: `next_seq = next_seq + n`, `live_count = live_count - n`, condition `next_seq = :old AND attribute_not_exists(deleting)`, with `:old` = the push's new `next_seq`.
  - each tombstone Put, `seq` from `:old + 1`, condition `hlc = :read_hlc`.
- 7. Cancelled: try again from step 3 with fresh reads, at most 3 tries in all (as `transact`). Still failing, or any storage error: log it, reply 200 with the push result.
- 8. Reply 200 `PushResponse` as F1. Eviction is not in the reply and the user gets no message.

## Infrastructure
- Add `by_evict_key` to the `MemoriesTable` in `infra/template.yaml`, and to `create memories` in `infra/local.sh`. IAM `dynamodb:Query` on `<MemoriesTable ARN>/index/*` exists since F2.
- `MEMORY_CAP` is not set in the template, so dev and prod use 50,000.
- Dev deploy: last task, needs the user's approval.

## Testing
- Service tests on DynamoDB Local, with `MEMORY_CAP=5`: class order, the exact count, no eviction at the cap, `writer = server`, tombstone `hlc` above a stored `hlc` with a future wall time, a pull that returns the tombstones.
- /verify: the class-order cases with a cap of 5, and one case at the default 50,000 on the local service (50,000 live + 1 new).

## Key Decisions
- D1. Evict in the push request, after the push transaction. Alt: in the push transaction; a queue or a scheduled job. Why: the push uses all 100 transaction items, and the request needs no new process.
- D2. A failed eviction keeps the push's 200, and the next push over the cap tries again. Alt: reply 409. Why: the push records are stored; a 409 would make the client push them again for nothing.
- D3. Tombstone `hlc` = server clock that observed the stored `hlc`, then ticked. Alt: wall clock only. Why: a device applies only a newer `hlc`, and a device clock ahead of the service would make it ignore the eviction.
- D4. Tombstone condition `hlc = <hlc from the consistent read>`. Alt: no condition. Why: another push can change the memory after the read; the condition stops the eviction from deleting a newer version.
- D5. At most 99 evictions in one push. Alt: a loop until at the cap. Why: one push adds at most 99 live records, so one batch brings the user back to the cap; a backlog from a failed eviction shrinks with each push.
- D6. Cap from env `MEMORY_CAP`, default 50,000. Alt: a constant. Why: tests reach the cap with 5 records.
- D7. GSI projection KEYS_ONLY. Alt: INCLUDE `hlc`; ALL. Why: the consistent read gives `hlc`, and ALL copies 2 KB of text for each item.
- D8. A device can bring an evicted memory back with a newer change (last write wins). Alt: a server tombstone always wins. Why: the sync rules stay the same for every writer; the next push over the cap evicts the oldest again.

- D9. Choose from a strongly consistent read of `excess + 99` index rows and this push's live records. Alt: trust the GSI order. Why: the GSI is eventually consistent. Rows that the last eviction removed can stay first in it, so each try fails on the same rows during a push burst. A fact that this push replaced can still show as class 3, so a younger class is evicted first. The extra 99 rows cover one earlier eviction batch. Limit: a record that an earlier push changed while the GSI lags can be missed, so the class order is best effort during a push burst; the cap still holds.

## Open Questions
- None.
