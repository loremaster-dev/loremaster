# M2 F3. Authenticated device management — Design

Source: project `design.md` (Endpoints, auth middleware, Data Model), `milestones.md` M2 F3, M2 F2 design. Resolves milestones OQ3 for device tokens and device ids.

## Overview
- A device token lets its user list the user's devices and delete one of them. A deleted token is rejected on the next request.
- An `axum` auth middleware on all routes except `POST /v1/devices`. It hashes the bearer token, reads `devices` with a strongly consistent read, and gives the handler the `user_id`.

## Components
- `loremaster-service` (`crates/loremaster-service/src/lib.rs`): adds the auth middleware, `GET /v1/devices`, `DELETE /v1/devices/{id}`. `POST /v1/devices` stays as F2 built it, with no auth.
- `loremaster-core`: adds `Device` and `DeviceList`. M2 F4 (409 body) and M2 F5 (CLI) use the same types.
- `infra/template.yaml`: adds IAM actions on `devices`. No new tables or indexes.

## Data Model
- No change. Uses `devices` (key `token_hash`) and GSI `by_user` (key `user_id`, projection ALL) from F2.

## Interfaces
- Auth, all routes except `POST /v1/devices`:
  - Header `Authorization: Bearer <token>`.
  - 401: no header, header not `Bearer <token>`, or `SHA-256(token)` not in `devices` (unknown or deleted).
- `GET /v1/devices`:
  - 200: `{"devices": [{"device_id": string, "name": string, "created_at": number}]}`. Only this user's devices. Oldest first by `created_at` (epoch ms).
- `DELETE /v1/devices/{id}`:
  - 204, no body: the device is this user's, and it is deleted. A device can delete itself.
  - 404: `{id}` is not one of this user's devices. The same for another user's device and for an id that does not exist.
- Error body for all errors: `{"error": "<short message>"}`, as in F2. 500 on a storage failure.

## Flow
- Auth:
  - 1. Parse the bearer token. Fail: 401.
  - 2. `GetItem devices[hex SHA-256(token)]` with `ConsistentRead: true`. No item: 401.
  - 3. Give `user_id` to the handler.
- `GET /v1/devices`:
  - 1. `Query by_user` where `user_id = :u`. Follow `LastEvaluatedKey` to the end.
  - 2. Sort by `created_at`, map to `Device`, return 200.
- `DELETE /v1/devices/{id}`:
  - 1. `Query by_user` where `user_id = :u`. Find the item with `device_id = {id}`. None: 404.
  - 2. `DeleteItem devices[that token_hash]` with condition `attribute_exists(token_hash) AND user_id = :u`. Condition fails: 404.
  - 3. Return 204.

## Infrastructure
- IAM role adds `dynamodb:GetItem` and `dynamodb:DeleteItem` on `devices`, and `dynamodb:Query` on `devices/index/by_user`.

## Testing
- Same as F2: the router in process, DynamoDB Local, fake GitHub, tables with unique names. Run on the developer machine only.
- Devices for a test come from `POST /v1/devices`, so the tests use the real tokens.

## Key Decisions
- D1. 401 for a missing, malformed, unknown or deleted token. Alt: 403 for a deleted token. Why: the CLI does one thing for all of them, which is to log in again.
- D2. 404 for another user's device and for a device that does not exist. Alt: 403 for another user's device. Why: one status for both does not show that another user's device id exists (NFR7).
- D3. Find the device to delete through `by_user`. Alt: a second GSI on `device_id`. Why: a user has at most 10 devices, so one query is small, and F2's index is enough.
- D4. `ConsistentRead: true` on the auth `GetItem`. Alt: the default read. Why: an eventually consistent read can accept a deleted token for a short time, and the project design says that a delete takes effect at once.
- D5. `GET /v1/devices` and `DELETE` read the GSI, which is eventually consistent. A device made or deleted a moment ago can be missing from, or still in, the list. Alt: none in DynamoDB, because a GSI has no consistent read. Why: the auth check (D4) is the security boundary, and the list is only for display.
- D6. Body `{"devices": [...]}`, not a bare array. Alt: a bare array. Why: fields can be added later without a break in the CLI.

## Open Questions
- None.
