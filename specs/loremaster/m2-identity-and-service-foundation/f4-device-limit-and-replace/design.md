# M2 F4. Device limit and replace — Design

Source: project `design.md` (Endpoints, Data Model, FR12), `milestones.md` M2 F4, M2 F2 and F3 designs. Resolves milestones OQ3 for `replace_device_id`.

## Overview
- A user has at most 10 devices. The 11th `POST /v1/devices` gets 409 with the current devices. With `replace_device_id`, the new device takes the place of an old one, and the old token is rejected on the next request.
- A `device_count` on the `users` item, changed and checked in the same DynamoDB transaction as the device write, so concurrent logins cannot pass the limit.

## Components
- `loremaster-service` (`crates/loremaster-service/src/lib.rs`): `register_device` gets the limit and the replace path. `delete_device` (F3) also decrements `device_count`.
- `loremaster-core`: `RegisterDevice` gets `replace_device_id: Option<String>`. New `DeviceLimitReached {error, devices: Vec<Device>}`, the 409 body. M2 F5 uses both.

## Data Model
- `users`: adds `device_count` (N). Set by every device write and delete. Starts at 0 with `if_not_exists`.
- `devices`: no change.

## Interfaces
- `POST /v1/devices`, no auth (as F2):
  - Request: `{"github_token", "name", "replace_device_id"?}`. `replace_device_id` absent or `null`: a new device.
  - 201: `{device_id, token}`, as F2.
  - 409: no `replace_device_id` and the user has 10 devices. Body `{"error": "device limit reached", "devices": [Device]}`, oldest first, the same `Device` as `GET /v1/devices`.
  - 404: `replace_device_id` is not one of this user's devices (another user's, or no such device). Body `{"error"}`. Nothing written.
  - 400, 401, 502: as F2. All checked before any limit or replace check.
- `DELETE /v1/devices/{id}`: same contract as F3.

## Flow
- New device (no `replace_device_id`):
  - 1. As F2: body, GitHub `GET /user`, token, `device_id`.
  - 2. `TransactWriteItems`:
    - Update `users[user_id]`: `SET next_seq = if_not_exists(next_seq, 0), live_count = if_not_exists(live_count, 0), device_count = if_not_exists(device_count, 0) + 1`. Condition `attribute_not_exists(device_count) OR device_count < 10`.
    - Put `devices[token_hash]`, condition `attribute_not_exists(token_hash)`, as F2.
  - 3. Cancelled with `ConditionalCheckFailed` on the user item: `Query by_user`, return 409 with the list.
  - 4. Cancelled with `TransactionConflict` (another write to the same user at the same time): run step 2 again, at most 3 tries in all. Still in conflict: 500.
- Replace (`replace_device_id` set):
  - 1. As F2 step 1.
  - 2. `Query by_user`, find `device_id = replace_device_id`. None: 404.
  - 3. `TransactWriteItems`:
    - Delete `devices[old token_hash]`, condition `attribute_exists(token_hash) AND user_id = :user_id`.
    - Put the new `devices[token_hash]`, condition `attribute_not_exists(token_hash)`.
    - `device_count` does not change, so the user item is not in the transaction.
  - 4. Cancelled with `ConditionalCheckFailed` on the delete: 404. `TransactionConflict`: as the new-device flow.
- Delete (F3, changed): one `TransactWriteItems`:
  - Delete `devices[token_hash]`, condition as F3.
  - Update `users[user_id]`: `SET device_count = device_count - 1`, condition `device_count > 0`.
  - Cancelled with `ConditionalCheckFailed` on the delete: 404, as F3. `TransactionConflict`: as the new-device flow.

## Infrastructure
- IAM: no new actions. The transactions use `UpdateItem` on `users`, and `PutItem` and `DeleteItem` on `devices`, which F2 and F3 grant.
- Dev stack data: the `gh:8943122` user item has no `device_count` and 2 devices from F2. Before the F4 deploy, delete those 2 devices and that user item. No backfill code. Prod does not exist until M5 F1.

## Testing
- As F3: router in process, DynamoDB Local, fake GitHub, tables with unique names.
- Concurrency: 2 registrations started together at 9 devices → one 201, one 409, 10 devices.

## Key Decisions
- D1. `device_count` on `users`, checked in the transaction. Alt: count with `Query by_user`, then write. Why: the query and the write are not atomic, so 2 logins at 9 devices can make 11.
- D2. Replace is one transaction of delete old + put new. Alt: delete, then register. Why: a failed put never leaves the user with one device less.
- D3. 404 for a bad `replace_device_id`, for another user's device and for no such device. Alt: 400 or 403. Why: the same rule as F3 `DELETE`, and it does not show that another user's device id exists.
- D4. `replace_device_id` is allowed below 10 devices. Alt: 400 below the limit. Why: the result is correct (the old device goes, the new comes), and the CLI needs no special case.
- D5. 409 body has `error` and `devices` in one object, with F3's `Device`. Alt: a bare list. Why: the same error shape as all other errors, and F5 reads the list from it.
- D6. Retry a `TransactionConflict` up to 3 tries. Alt: return 500, or 409 for a retry by the client. Why: the SDK does not retry a cancelled transaction, and a conflict means only that another login ran at the same moment.
- D7. Reset the dev data, no backfill. Alt: count each user's devices on first use. Why: only 2 test devices in dev have no count.

## Open Questions
- None.
