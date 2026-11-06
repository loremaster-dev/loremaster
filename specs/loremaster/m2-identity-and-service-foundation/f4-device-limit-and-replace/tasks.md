# M2 F4. Device limit and replace — Tasks

Input: `design.md` next to this file, project `design.md` (Endpoints, FR12), `milestones.md` M2 F4, M2 F2 and F3 designs. Service tests need DynamoDB Local (`infra/local.sh`) and run on the developer machine only.

## 1. The 11th device is refused [serial]
- [x] 1.1 In `crates/loremaster-core/src/lib.rs`, add `replace_device_id: Option<String>` to `RegisterDevice` (`serde(default)`, skipped when `None`) and add `DeviceLimitReached {error, devices}`. Extend `device_api_bodies_use_the_wire_field_names`: a body with no `replace_device_id` parses, and both types round-trip with the design's field names.
- [x] 1.2 In `crates/loremaster-service/src/lib.rs` `store_device`: add `device_count = if_not_exists(device_count, 0) + 1` and the condition `attribute_not_exists(device_count) OR device_count < 10` to the user update. `ConditionalCheckFailed` on the user item → `Query by_user` → 409 `DeviceLimitReached`, oldest first. Test `tests/devices.rs` `eleventh_device_is_409_with_the_list`: 10 registrations → 201, the 11th → 409 with the 10 devices, still 10 items, `device_count` 10.
- [x] 1.3 Retry the transaction on `TransactionConflict`, at most 3 tries in all, then 500. Test `concurrent_logins_stop_at_ten`: 9 devices, 2 registrations run together with `tokio::join!` → one 201 and one 409, 10 items, `device_count` 10.

## 2. A device is replaced in one transaction [serial, needs 1]
- [x] 2.1 `register_device` with `replace_device_id`: find it in `by_user` (none → 404), then one `TransactWriteItems` with the old device delete (condition `attribute_exists(token_hash) AND user_id = :user_id`) and the new device put. `ConditionalCheckFailed` on the delete → 404. Same conflict retry as 1.3.
- [x] 2.2 Test `replacing_a_device_keeps_ten`: 10 devices, replace one → 201. The old token on `GET /v1/devices` → 401, the new token → 200 with 10 devices, `device_count` 10.
- [x] 2.3 Test `replacing_a_device_that_is_not_the_users_is_404`: bob's `device_id`, and `dev_` + a new ULID → 404 `{"error"}`. Bob's device remains, alice's items and `device_count` do not change.
- [x] 2.4 Test `replacing_below_the_limit_works`: 2 devices, replace one → 201, still 2 devices, `device_count` 2.

## 3. Delete frees a slot [parallel, needs 1.2]
- [x] 3.1 `delete_device` in `src/lib.rs`: one `TransactWriteItems` with the F3 delete and `SET device_count = device_count - 1` (condition `device_count > 0`) on the user. `ConditionalCheckFailed` on the delete → 404. Same conflict retry as 1.3.
- [x] 3.2 Test `tests/device_management.rs` `deleting_a_device_frees_a_slot`: 10 devices, delete one → 204, `device_count` 9, a new registration → 201. Existing F3 tests still pass.

## 4. The dev stack enforces the limit [serial, needs 2 and 3] [approval]
- [x] 4.1 With approval, in the dev stack: delete the 2 F2 devices (`dev check 1`, `dev check 2`) and the `gh:8943122` user item with `aws dynamodb delete-item`. Both tables then have no items for that user.
- [x] 4.2 `sam build` and `sam deploy --config-env dev`. Over HTTPS with a real GitHub token: register a device, replace it → 201, the old token → 401, the new token → 200 with 1 device. Delete the test device after; the user item then has `device_count` 0.

## Dependencies
- 1 blocks 2 and 3. 2 and 3 run in parallel.
- 4 needs 2 and 3, and the user's approval before 4.1 and the deploy in 4.2.
