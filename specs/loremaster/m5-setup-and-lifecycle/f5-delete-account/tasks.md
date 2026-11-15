# M5 F5. Delete account — Tasks

Input: project `design.md` (Section 5 Endpoints and Account deletion, Data Model, D15, D19, D21, Appendix A access patterns), `requirements.md` FR34, `milestones.md` M5 F5, M2 F3, M2 F4 and M3 F1 designs. Service tests need DynamoDB Local (`infra/local.sh`) and run on the developer machine only.

M3 F2 changes `tests/common/mod.rs` at the same time. To prevent merge conflicts, put the new test helpers in `tests/account.rs`, and seed `memories` items with `PutItem`, not with push.

## 1. A purge deletes all rows of one user [serial]
- [x] 1.1 `pub async fn purge_account(state: &AppState, user_id: &str)` in `crates/loremaster-service/src/lib.rs`: page a `Query` on the `memories` base table (`user_id = :u`, keys only), then page `Query by_user` on `devices`, `BatchWriteItem` deletes in groups of 25 with `UnprocessedItems` sent again, then `DeleteItem users` last. Compiles, existing tests pass.
- [x] 1.2 New `tests/account.rs`, test `purge_deletes_only_this_users_rows`: alice with 2 devices and 30 memories, bob with 1 device and 2 memories → after `purge_account(alice)`, scans have no alice item in `users`, `devices`, `memories`. Bob's items unchanged.
- [x] 1.3 Test `purge_twice_gives_no_error`: purge alice, purge alice again → both `Ok`. Purge a `user_id` that never existed → `Ok`.

## 2. `DELETE /v1/account` revokes every token at once [serial, needs 1]
- [x] 2.1 Add `self_function: Option<String>` to `Config`, from `AWS_LAMBDA_FUNCTION_NAME` (Lambda sets it). Add `aws-sdk-lambda` to `crates/loremaster-service/Cargo.toml` and a Lambda client to `AppState`. Existing tests pass with `self_function: None`.
- [x] 2.2 `DELETE /v1/account` in the authenticated router in `src/lib.rs`: `UpdateItem users SET deleting = :true`, `Query by_user`, `DeleteItem devices` for each `token_hash`, then start the purge: `Some(name)` → Lambda `Invoke` with `InvocationType::Event` and body `{"purge_user_id": "<user_id>"}`. `None` → call `purge_account` inline. Return 202, no body.
- [x] 2.3 Test `delete_account_rejects_every_token`: alice with 2 devices and memories → `DELETE /v1/account` with token 1 → 202. Token 1 and token 2 on `GET /v1/devices` → 401. Bob's token → 200.
- [x] 2.4 Test `delete_account_without_token_and_storage_failure`: no token → 401. Tables missing → 500.

## 3. No signup while a purge is pending [parallel, needs 2.2]
- [x] 3.1 In `add_device` in `src/lib.rs`, add `attribute_not_exists(deleting)` to the user item condition. In `replace_device`, add a `ConditionCheck` on the user item with the same condition. A failed `deleting` condition → 409 `{"error": "account deletion in progress"}`, kept apart from `DeviceLimit` by a consistent `GetItem users` after the cancel.
- [x] 3.2 Test `register_while_deleting_is_409`: set `deleting = true` on alice's user item with `UpdateItem` → `POST /v1/devices` (new and with `replace_device_id`) → 409 with that error, no new device item. After `purge_account(alice)` → `POST /v1/devices` → 200, `device_count` 1.
- [x] 3.3 Existing device-limit tests in `tests/devices.rs` still give 409 with the device list.

## 4. The Lambda runs the async purge event [parallel, needs 2]
- [x] 4.1 In `src/lib.rs`, `#[serde(untagged)] pub enum Event { Purge { purge_user_id: String }, Http(lambda_http::request::LambdaRequest) }`. Unit test: `{"purge_user_id": "gh:1"}` parses as `Purge`. An API Gateway v2 HTTP sample event parses as `Http`.
- [x] 4.2 In `src/main.rs`, replace `lambda_http::run` with `lambda_http::lambda_runtime::run` on a `service_fn` over `Event`: `Purge` → `purge_account`, an error is returned so Lambda retries. `Http` → `lambda_http::Adapter` over `router`. `cargo lambda watch`: `POST /v1/devices` and `GET /v1/devices` still work.

## 5. The dev stack deletes an account [serial, needs 3 and 4] [approval]
- [x] 5.1 In `infra/template.yaml`: `Timeout: 300`. IAM `dynamodb:DeleteItem` and `dynamodb:ConditionCheckItem` on `UsersTable.Arn`, `dynamodb:BatchWriteItem` on `DevicesTable.Arn` and `MemoriesTable.Arn`, `dynamodb:Query` on `MemoriesTable.Arn`, and `lambda:InvokeFunction` on the function's own ARN (`!Sub` with a fixed `FunctionName` or a name pattern, to prevent a circular reference). `sam validate --lint` passes.
- [x] 5.2 `sam build` and `sam deploy --config-env dev`. Over HTTPS: user with 2 devices from `gh auth token` and pushed memories, `DELETE /v1/account` → 202, both tokens → 401.
- [x] 5.3 After the async run: no `users`, `devices` or `memories` items for that `user_id`. A second test user's rows unchanged. `aws dynamodb scan` with filter `attribute_exists(deleting)` → empty. `aws lambda invoke` with `{"purge_user_id": ...}` again → no error in CloudWatch Logs. Local `store.db` unchanged, recall probe still returns local memories.

## Dependencies
- 1 blocks 2. 2 blocks 3 and 4. 3 and 4 run in parallel.
- 5 needs 3 and 4, and the user's approval before the deploy in 5.2.
