# M3 F4. Conflicts and deletes — Tasks

Input: `milestones.md` M3 F4 (no feature design), `requirements.md` FR6, FR7, FR9, project `design.md` Section 4 (last write wins by `hlc`, tombstones kept forever), M3 F3 design and code. F4 adds no sync rule: it proves that the F3 rules converge in conflicts and deletes. Tests use the axum fake service in `tests/common/fake_service.rs` and two sandbox homes, with `SyncClient::push_outbox` and `SyncClient::pull` called in order, so no test waits on a timer.

## 1. `dev seed` refuses a given `hlc` that the service would refuse [serial]
- [x] 1.1 In `crates/loremaster/src/dev.rs`, pass the logged-in `device_id` to `parse_seed` and `line_error`. With a login, a line whose `hlc` does not parse, or whose `hlc` writer is not `device_id`, gives "line N: hlc writer is not this device" and nothing is seeded. With no login, any `hlc` is kept, as M1. Unit tests in `dev.rs`: own writer accepted, other writer refused, no login accepted.
- [x] 1.2 Test in `tests/dev_tools.rs`: seed with a login and a line with another writer's `hlc` → exit 1, that error on stderr, no `store.db` change.

## 2. Concurrent creates and changes converge [parallel, needs 1]
- [x] 2.1 Test `offline_devices_that_create_at_once_both_get_everything` in `tests/sync.rs`: A and B each write a different fact with no sync, then A pushes, B pushes, A pulls, B pulls → both stores have both facts, both outboxes are empty, and a recall probe on each home returns the other home's fact.
- [x] 2.2 Test `the_newer_change_of_one_memory_wins_on_both`: A and B write the same `memory_id` with different text, B with the higher `hlc`, push and pull in both orders (A first, then B first) → both stores have B's text and `hlc`, the fake has one record, both outboxes are empty.

## 3. Deletes hold on every device [parallel, needs 1]
- [x] 3.1 Test `a_tombstone_written_while_the_other_is_offline_takes_the_memory_away_there`: both homes have a fact, A writes its tombstone and pushes, B pulls → B's row is a tombstone with no vector, and a recall probe and the SessionStart profile on B no longer return it.
- [x] 3.2 Test `an_older_change_after_a_newer_tombstone_does_not_resurrect_it`: A pushes a tombstone, then B writes a live change to the same memory with an older `hlc` and its own writer, pushes, then pulls → the fake still has the tombstone, B's row is the tombstone, B's outbox is empty, A's pull leaves A's tombstone.
- [x] 3.3 Test `a_pulled_superseded_fact_is_not_recalled`: B has a fact, A pushes a newer version of it with `superseded_by` set and pushes the replacing fact, B pulls → a recall probe and the profile on B return only the replacing fact.

## Dependencies
- 1 blocks 2 and 3, because their verify cases seed a given `hlc`. 2 and 3 run in parallel.
- /verify runs the five M3 F4 cases on the real service locally (DynamoDB Local, `cargo lambda watch`) with `dev seed` on two homes.
