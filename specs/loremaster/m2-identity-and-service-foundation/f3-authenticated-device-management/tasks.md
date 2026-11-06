# M2 F3. Authenticated device management — Tasks

Input: `design.md` next to this file, project `design.md` (Endpoints, Data Model), `milestones.md` M2 F3, M2 F2 design. Service tests need DynamoDB Local (`infra/local.sh`) and run on the developer machine only.

## 1. A device token lists the user's devices [serial]
- [x] 1.1 Add `Device {device_id, name, created_at}` and `DeviceList {devices}` to `crates/loremaster-core/src/lib.rs`. Extend `device_api_bodies_use_the_wire_field_names`: both round-trip through `serde_json` with the field names in the design.
- [x] 1.2 In `crates/loremaster-service/tests/common/mod.rs`, add a second fake GitHub user (`gho_bob`, a different id) and a helper `get(path, token)` and `delete(path, token)` that set `Authorization: Bearer <token>`. Existing tests still pass.
- [x] 1.3 Auth middleware in `crates/loremaster-service/src/lib.rs`: parse `Authorization: Bearer <token>`, `GetItem devices[hex SHA-256]` with `ConsistentRead: true`, put `user_id` in request extensions, `ApiError::Unauthorized` → 401. Applied to all routes except `POST /v1/devices`.
- [x] 1.4 `GET /v1/devices` in `src/lib.rs`: `Query by_user` with pagination, sort by `created_at`, return `DeviceList`. New file `tests/device_management.rs`, test `lists_only_this_users_devices`: alice with 2 devices and bob with 1 → alice gets her 2, oldest first, with the names from `POST`.
- [x] 1.5 Test `requests_without_a_valid_token_are_401`: no header, `Basic abc`, `Bearer` with no token, `Bearer <unknown>` → 401 `{"error": ...}` on `GET /v1/devices`.

## 2. A device token deletes one of the user's devices [serial, needs 1]
- [x] 2.1 `DELETE /v1/devices/{id}` in `src/lib.rs`: find the item in `by_user` by `device_id`, `DeleteItem` with condition `attribute_exists(token_hash) AND user_id = :u`. Not found or condition failed → 404. Success → 204, no body.
- [x] 2.2 Test `deleting_a_device_revokes_its_token`: alice deletes device 1 → 204. Token 1 on `GET` → 401. Token 2 on `GET` → 200 with 1 device. `devices` scan has 1 alice item.
- [x] 2.3 Test `a_device_can_delete_itself`: token 1 deletes device 1 → 204, then token 1 → 401.
- [x] 2.4 Test `deleting_another_users_device_is_404`: alice deletes bob's `device_id` → 404. Alice deletes `dev_` + a new ULID → 404. Bob's token on `GET` still → 200 with his device.
- [x] 2.5 Test `storage_failure_on_device_routes_is_500`: tables missing (`Service::without_tables`) → `GET` and `DELETE` give 500, not 401 or 404.

## 3. The dev stack serves the new routes [serial, needs 2] [approval]
- [x] 3.1 In `infra/template.yaml`, add `dynamodb:GetItem` and `dynamodb:DeleteItem` on `DevicesTable.Arn`, and `dynamodb:Query` on `${DevicesTable.Arn}/index/by_user`. `sam validate --lint` passes.
- [x] 3.2 `sam build` and `sam deploy --config-env dev`. Over HTTPS with a real GitHub token: register 2 devices, `GET` lists them, `DELETE` one → 204, its token → 401, the other token → 200. Delete the test devices after.

## Dependencies
- 1 blocks 2. 2 blocks 3.
- 3 needs the user's approval before deploy (3.2).
