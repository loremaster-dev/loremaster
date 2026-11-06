# M3 F1. Push records — Design

Source: project `design.md` (Section 4 Sync, Section 5 Endpoints, Appendix A), `milestones.md` M3 F1, M2 F2 to F4 designs. Resolves milestones OQ3 for push.

## Overview
- `POST /v1/sync/push` stores up to 99 records for the caller's user, last write wins by HLC, and gives each stored record a new per-user `seq`.
- One DynamoDB transaction: the `users` item (`next_seq`, `live_count`, condition on the old `next_seq`) and one conditional put for each record that is newer than the stored one.

## Components
- `loremaster-core`: new `PushRequest {records: Vec<Record>}` and `PushResponse {applied: Vec<String>}`. `SyncClient` (M3 F3) uses both.
- `loremaster-service` (`crates/loremaster-service/src/lib.rs`): route `POST /v1/sync/push` behind the auth middleware. The middleware now gives the handler `user_id` and `device_id`. Reuses `transact` (M2 F4).
- `infra/template.yaml`: `MemoriesTable`, env `MEMORIES_TABLE`, IAM actions. `infra/local.sh` and the test setup make the same table.

## Data Model
- `memories` (new), as Appendix A:
  - Key: `user_id` (S, HASH), `memory_id` (S, RANGE).
  - LSI `by_seq`: `user_id` + `seq` (N), projection ALL. An LSI can only be made with the table, so F1 makes it. M3 F2 reads it.
  - Live item: `user_id`, `memory_id`, `seq`, `hlc`, `writer`, `type`, `kind` (facts), `scope`, `project_id` (project scope), `text`, `created_at`, `superseded_by` (if set), `evict_key`.
  - Tombstone item: `user_id`, `memory_id`, `seq`, `hlc`, `writer`, `deleted: true`. Nothing else.
  - `evict_key` = `<class>#<created_at>`, class `1` fact with `superseded_by`, `2` episode, `3` other fact. Written on live items only. The sparse GSI on it comes in M3 F7.
  - On-demand billing, `SSEEnabled: true`, as `users` and `devices`.
- `users`: uses `next_seq` and `live_count` from M2 F2. No new attributes.

## Interfaces
- `POST /v1/sync/push`, device token required.
  - Request: `{"records": [Record]}`.
  - 200: `{"applied": [memory_id]}`, the records stored by this push, in request order. Records with an `hlc` not newer than the stored one are not in it.
  - 400 `{"error"}`: body does not parse, more than 99 records, or a record fails a check below. The error names the first bad `memory_id`. Nothing written.
  - 409 `{"error": "retry"}`: another push for the same user changed the data between the read and the write. Nothing written. The client pushes again later.
  - 401, 500: as M2.
- Checks for each record:
  - `memory_id` is a ULID. No `memory_id` is twice in one push.
  - `hlc` parses with `Hlc::parse`. `writer` equals the token's `device_id`, and the `hlc` ends with that writer.
  - Live (`deleted` false): `type`, `scope`, `created_at`, `text` set. `text` 1 to 2,000 characters. `kind` set for a fact, absent for an episode. `project_id` set for `scope` project, absent for `scope` user.
  - Tombstone (`deleted` true): content fields are ignored and not stored.
  - `user_id`, `seq` and `evict_key` from the client are ignored. The service sets them.

## Flow
- 1. Parse and check the body. No records: 200 `{"applied": []}`.
- 2. `GetItem` on `users` with `ConsistentRead` for `next_seq`. Then read the stored versions: `BatchGetItem` on `memories` with `ConsistentRead`, projection `memory_id, hlc, deleted`. Send `UnprocessedKeys` again until none are left.
- 3. Keep each record with no stored item, or a stored `hlc` lower than its own. None kept: 200 `{"applied": []}`.
- 4. Give the kept records `seq` = `next_seq + 1` to `next_seq + n`, in request order. `live_count` delta for each: none stored or stored tombstone, and new live: +1. Stored live and new tombstone: -1. Other: 0.
- 5. `TransactWriteItems` with `transact`:
  - Update `users[user_id]`: `SET next_seq = :new, live_count = live_count + :delta`, condition `next_seq = :old`.
  - Put each kept record, condition on the version read in step 2: `attribute_not_exists(memory_id)` if none was stored, else `hlc = :read_hlc`.
- 6. Cancelled with `ConditionalCheckFailed` on any item: 409. `TransactionConflict`: `transact` tries again, as in M2 F4.
- 7. 200 with the kept `memory_id`s.

## Infrastructure
- IAM adds: `dynamodb:BatchGetItem` and `dynamodb:PutItem` on `memories`, and `dynamodb:GetItem` on `users`. The transaction uses `UpdateItem` on `users`, which M2 F2 granted.

## Testing
- As M2: the router in process, DynamoDB Local, tables with unique names. The test setup makes `memories` with the `by_seq` LSI.
- 2 pushes at the same time: each test push sends again on 409 until it gets 200, as `SyncClient` will. Then all records are stored and every `seq` is unique.

## Key Decisions
- D1. 400 for all bad input, the whole push refused. Alt: 413 for too many records, or a partial store. Why: it is a client bug, and one rule with nothing written is the simplest for the client.
- D2. The service sets `user_id`, `seq` and `evict_key`, and checks `writer` against the token's device. Alt: trust the client. Why: a device must not write for another user (NFR7), or as another device, or as `server` (M3 F7 evictions).
- D3. `evict_key` written in F1, its GSI added in M3 F7. Alt: both in F7. Why: no fill-in of old items in F7. A GSI can be added later, so it waits for the feature that reads it.
- D4. No retry in the push handler. A failed condition gives 409, and the client pushes again. Alt: read again and retry on the server, as the project design said. Why: 2 pushes of one user in the same ~50 ms are rare, and the client must handle 409 anyway.
- D5. `seq` in request order inside one push. Alt: order by `hlc`. Why: pull order only has to be stable and complete, and the request order is already known.
- D6. Each put is conditioned on the exact version read in step 2. Alt: `attribute_not_exists(memory_id) OR hlc < :hlc`. Why: `BatchGetItem` gives no order across items, so another push can commit between the reads. With the exact-version condition, the push then gets 409, and the `live_count` delta is never computed from a stale read.

## Open Questions
- None.
