# M3 F2. Pull records — Design

Source: project `design.md` (Section 4 Sync and Bootstrap, Section 5 Endpoints, Appendix A), `requirements.md` FR7, FR9, FR11, NFR7, `milestones.md` M3 F2, M3 F1 design.

## Overview
- `GET /v1/sync/pull?cursor=N` gives the caller's records with `seq > N`, in `seq` order, one page at a time.
- A serializable `TransactGetItems` on `users` for `next_seq` and `total`, then one consistent query on the `by_seq` LSI, cut at that `next_seq`.

## Components
- `loremaster-core`: new `PullResponse {records: Vec<Record>, next_cursor: i64, total: i64, more: bool}`. `SyncClient` (M3 F3) uses it.
- `loremaster-service` (`crates/loremaster-service/src/lib.rs`): route `GET /v1/sync/pull` behind the auth middleware. Uses `Caller.user_id` from F1.
- `infra/template.yaml`: IAM `dynamodb:Query` on the `memories` table indexes.

## Data Model
- No new table or attribute. Reads `memories` through LSI `by_seq` (F1) and `users.live_count` (M2 F2, kept by F1).

## Interfaces
- `GET /v1/sync/pull?cursor=N`, device token required.
  - 200 `PullResponse`:
    - `records`: each stored item with `seq > N`, in `seq` order, all attributes as stored. A tombstone has only its ids, `seq`, `hlc`, `writer` and `deleted: true`.
    - `next_cursor`: `seq` of the last record in the page. Empty page: the `cursor` sent.
    - `total`: `users.live_count`, the number of live records of the user. The client shows progress as live records pulled / `total`. Tombstones do not count.
    - `more`: true if the query stopped at the 1 MB limit, or the page was cut at `next_seq`. The page after it can be empty.
  - 400 `{"error"}`: `cursor` missing, or not a whole number from 0 to 2^63 - 1.
  - 409 `{"error": "retry"}`: a push for the same user was in progress for all 3 tries of the user read. Nothing changed. The client pulls again later.
  - 401, 500: as M2.

## Flow
- 1. Parse `cursor`. Bad: 400.
- 2. `TransactGetItems` on `users[user_id]`, projection `next_seq, live_count`. Cancelled with `TransactionConflict`: try again, at most 3 tries in all, as `transact` (M2 F4). Still cancelled: 409.
- 3. `Query` LSI `by_seq`: `user_id = :u AND seq > :cursor`, `ConsistentRead`, ascending, no `Limit`.
- 4. Drop the items with `seq` above the `next_seq` of step 2. Items to `Record`. `next_cursor` from the last kept item. `more` = the query gave a `LastEvaluatedKey`, or step 4 dropped an item.
- 5. 200.

## Infrastructure
- IAM adds `dynamodb:Query` on `<MemoriesTable ARN>/index/*`. `TransactGetItems` needs `dynamodb:GetItem` on `users`, which came in F1.

## Testing
- As F1: the router in process, DynamoDB Local, tables with unique names. Records go in through `POST /v1/sync/push`.
- Over 1 MB: about 600 records of 2,000 characters, pushed 99 at a time. Pages followed by `next_cursor` until `more` is false give each record once.
- DynamoDB Local applies each transaction at once, so the cut at `next_seq` has a unit test on the page function, not a race test.

## Key Decisions
- D1. `total` = `users.live_count`. Alt: a new counter of all items, kept by push; or a COUNT query. Why: no new attribute and no change to push. A COUNT query reads the whole partition on each pull.
- D2. `ConsistentRead` on the LSI query. Alt: eventually consistent, half the read units. Why: an eventual read can show `seq` 12 before 11. The cursor then moves past 11, and the record is never pulled.
- D3. 400 for a missing `cursor`. Alt: missing means 0. Why: the client always has a cursor, and a silent 0 turns a client bug into a full download.
- D4. Records as stored, `user_id` and `evict_key` included. Alt: drop the attributes the client does not need. Why: the simplest code, with one `Record` type for both directions.
- D5. One query per page, the 1 MB limit sets the page size, `more` from `LastEvaluatedKey`. Alt: a `Limit`, or a look-ahead read for an exact `more`. Why: as the project design. An empty last page costs 1 extra request.
- D6. Read `next_seq` with `TransactGetItems` before the query, and cut the page at it. Alt: trust the consistent query. Why: a `Query` is only read-committed against `TransactWriteItems`, so it can show `seq` 12 of a push in progress before its `seq` 11. `TransactGetItems` is serializable: it fails while a push on the user item is in progress, and after it succeeds every push up to its `next_seq` is complete.

## Open Questions
- None.
