# M3 F6. Bootstrap with progress — Tasks

Input: `design.md` next to this file (D1 to D5), `milestones.md` M3 F6, project `design.md` Section 4 Bootstrap, M3 F3 design and code. The pull from `cursor=0`, the saved cursor and the in-place index are F3 code; F6 adds `total`, `Store::progress` and `loremaster status`, and tests the bootstrap behavior. Client tests use the axum fake service in `tests/common/fake_service.rs`.

## 1. The pull saves `total` and the store reports progress [serial]
- [x] 1.1 In `crates/loremaster/src/store.rs`, `Store::apply_pulled` takes `total: i64` and writes `sync_state.total` in the page transaction, next to `cursor`. `Recall::apply_pulled` (`recall.rs`) passes `page.total`. Store unit test: after a page, `total` is saved; after a second page, it has the new value.
- [x] 1.2 In `store.rs`, add `Progress {recallable: i64, total: Option<i64>, outbox: i64}` and `Store::progress()`. `recallable` counts rows with `deleted = 0` and `model_id` = `embed::MODEL_ID`. Unit test: a tombstone, a row with no vector and a row of another model are not counted; `total` is `None` before a pull; `outbox` counts queued rows. Add `progress` to `operations_report_sql_errors`.

## 2. `loremaster status` shows sync state and progress [serial, needs 1]
- [x] 2.1 New `crates/loremaster/src/status.rs`: `pub fn report(paths: &Paths) -> Result<String, StatusError>` builds the 3 lines of the design. Sync line from `config::service_url` and `config::load_credential` ("on, device <id>", "off: no api_url", "off: no login", URL checked first). Memories line with thousands separators, "N recallable" while `total` is `None`. Unit tests for each sync line, both memories lines, and 0, 999, 1,000 and 50,000 as numbers.
- [x] 2.2 In `crates/loremaster/src/main.rs`, add `Command::Status` that prints `report` and exits 0, or prints the error on stderr and exits 1. Export `status` from `lib.rs`. New test file `tests/status.rs`: a logged-in sandbox with seeded rows and a saved `total` → the 3 lines, exit 0; an empty home → "Sync: off: no api_url", "Memories: 0 recallable", "Outbox: 0 waiting"; a store with a newer schema → exit 1 with the store error. Add "status" to `help_lists_the_commands` in `tests/cli.rs`.

## 3. A bootstrap stopped part way continues and recalls what it has [parallel, needs 1]
- [x] 3.1 In `tests/common/fake_service.rs`, add `FakeService::fail_after(route, requests)`: each request to `route` after the first `requests` gets 503 until `FakeService::recover()`.
- [x] 3.2 Test `a_bootstrap_stopped_part_way_continues_from_its_cursor` in `tests/sync.rs`: 25 records on the fake, page size 10, `fail_after(Pull, 2)` → the pull fails as unavailable, `Store::progress()` gives 20 of 25. A new `SyncClient` on the same home (a restart), `recover()`, pull → 25 of 25, 25 rows, each `memory_id` once, the third page requested with `cursor=20`.
- [x] 3.3 Test `recall_uses_the_pages_loaded_so_far`: same partial load with the embedder ready → a recall probe returns a fact from the first 2 pages, and a fact from the third page is not returned until the pull completes.

## Dependencies
- 1 blocks 2 and 3. 2 and 3 run in parallel.
- /verify runs the 3 F6 cases with 5,000 records on the local service (DynamoDB Local, `cargo lambda watch`), including a real kill of the daemon mid-bootstrap and a Stop hook mid-bootstrap.
