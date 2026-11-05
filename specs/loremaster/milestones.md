# Loremaster Milestones

Source: `design.md`, `requirements.md` (P0). Tracking document: update `Status` as work progresses (`not started`, `in progress`, `done`).

FR38 applies to every milestone: each exit criterion holds in Claude Code, the only P0 agent.

Build order: M1 and M2 in parallel; then M3, M4 and M5 in parallel. M4 and M5 finish their last exit checks after M3.

```
M1 ─┐
    ├─► M3 Sync ───────────────┐
M2 ─┤                          ├─► final exit checks
    ├─► M4 Extraction (build) ─┤
    └─► M5 Setup (build) ──────┘
```

## How features are verified

Each feature lists `Verify` cases as `input → output`. /to-tasks and /verify start from these. A feature is done only when every case passes.

Environments:
- **Local:** `cargo lambda watch` on `localhost:9000` + DynamoDB Local.
- **Dev stack:** `sam deploy --config-env dev`, a personal AWS stack.
- **Two homes:** 2 `LOREMASTER_HOME` directories, one `loremasterd` each, both with `api_url` set to Local or the Dev stack.

Tags on a case:
- **[approval]:** uses a real cloud account, a paid API or a public release. Needs the user's approval before /verify runs it.
- **[manual]:** needs a person or specific hardware. /verify lists it under `Not verified` for the user.

Recall probe: pipe `UserPromptSubmit` hook JSON to `loremaster hook claude-code UserPromptSubmit`, then read `additionalContext` on stdout. Test records: `loremaster dev seed <file.jsonl>`, which accepts `memory_id`, `hlc`, `superseded_by` and `deleted`.

## M1. Local client core
- Delivers: `loremasterd` + hook CLI on one device, no server. Claude Code gets profile and recall from local memory; turns captured and redacted into `turn_queue`.
- Builds: `loremasterd` (`HookServer`, socket), `loremaster` CLI (`hook`, `enable`, `dev seed`), `ClaudeCodeAdapter`, `Redactor`, `Embedder`, `Index`, `Recall` (profile + per prompt), `ProjectId`, SQLite store (all 6 tables), spool, `loremaster-core` crate (record, ULID, HLC).
- Covers: FR4, FR9 (recall filter), FR14 (capture), FR20, FR21, FR22, FR23, FR24, FR26, FR27, FR28, FR29, FR30 (recall filter), FR36, NFR1, NFR3, NFR9.
- Exit criteria:
  - Recall adds <= 100 ms P95 at 50k seeded memories (NFR1).
  - Capture adds <= 50 ms P95 (NFR3).
  - Secret corpus 100% redacted; custom pattern redacted after config change (FR21, FR22).
  - Profile <= 300 tokens, per-prompt <= 500 tokens, nothing injected when nothing passes tau (FR23, FR24).
  - Project-scoped memory never injected in another project; SSH and HTTPS remotes give one project ID (FR26, FR36).
  - Service killed mid-session: agent proceeds, spool replayed on restart, no turn lost (FR4, FR29).
  - Superseded and tombstoned seeded rows never injected (FR9, FR30).
- Needs: none.
- Design: yes.
- Features: none. Built as one PR (#1) before features were used; not split afterwards.
- Verify (regression checks, all local):
  - `dev bench` on a developer laptop → every row within budget, exit 0. Recall <= 100 ms and capture <= 50 ms P95.
  - `cargo test -p loremaster --test redact_corpus` → pass.
  - `cargo test -p loremaster --test recall` → pass. Covers token caps, tau, project filter, superseded and tombstoned rows.
  - `cargo test -p loremaster --test crash` → pass. Covers service killed mid-session and spool replay.
- Status: done.

## M2. Identity and service foundation
- Delivers: AWS stack with GitHub device-flow login and auth middleware. `loremaster login` issues a per-device token, over SSH too.
- Builds: SAM template (`infra/template.yaml`, dev and prod envs), GitHub OAuth App (device flow), `POST /v1/devices`, `GET /v1/devices`, `DELETE /v1/devices/{id}`, auth middleware, DDB `users`, `devices`, `loremaster login`, `api_url` config, API types in `loremaster-core`.
- Covers: FR12, FR13 (slot freed on the server), NFR6 (DDB), NFR7.
- Exit criteria:
  - Login from a headless Linux box via URL + code.
  - 11th device refused with the list of current devices; replacing one succeeds and the replaced token is rejected on the next request (FR12).
  - Deleted device token rejected on the next request.
  - Token for user A cannot read or write user B (NFR7).
- Needs: none. `loremaster login` joins M1's CLI once both exist.
- Features:
  - F1. CI/CD: cache the Cargo build and the embedding model in CI, require CI on `main` (done), and remove the latency checks from CI. Docs: tasks. Needs: none. Status: done.
    - Verify:
      - Repeat CI run → cache hit for the Cargo build and the model on all 3 runners, no `ort-sys` build.
      - Test made to fail → `test` step fails on all 3 runners.
  - F2. Register a device: deployable stack and `POST /v1/devices` create a user and a device. Docs: design, tasks. Needs: none. Status: not started.
    - Verify (Local, except where tagged):
      - Valid GitHub token + device name → `{device_id, token}`. `users` has `gh:<GitHub id>`. `devices` key is the SHA-256 of `token`. No stored item holds the raw `token` or the GitHub token.
      - Second login, same GitHub account → new `device_id`, same `user_id`.
      - Invalid GitHub token → 401, no item written.
      - `sam deploy --config-env dev` → stack up, same 3 cases pass on HTTPS with a real GitHub token. [approval]
      - `aws dynamodb describe-table` on `users` and `devices` → KMS encryption on, billing on demand. [approval]
  - F3. Authenticated device management: auth middleware, `GET /v1/devices`, `DELETE /v1/devices/{id}`. Docs: tasks. Needs: F2. Status: not started.
    - Verify (Local):
      - Valid token, `GET /v1/devices` → this user's devices only.
      - No token, or unknown token → rejected (status: OQ3).
      - `DELETE` own device → 204. That token on the next request → rejected. The user's other token → still works. Device count falls by 1.
      - User A token, `DELETE` user B's device → B's device remains, B's token works (status: OQ3).
  - F4. Device limit and replace: 11th device refused, one device replaced in a transaction. Docs: tasks. Needs: F3. Status: not started.
    - Verify (Local):
      - 10 devices, 11th `POST /v1/devices` → 409 with the 10 devices. Still 10 in `devices`.
      - Same request with `replace_device_id` → `{device_id, token}`. Replaced token on the next request → rejected. Still 10 devices.
      - `replace_device_id` of another user's device → that device remains, no device created (status: OQ3).
  - F5. `loremaster login`: device flow in the CLI, token saved. Docs: tasks. Needs: F4. Status: not started.
    - Verify:
      - `loremaster login` against the Dev stack, over SSH with no browser on the box → prints URL + user code. After approval in a browser elsewhere, `~/.loremaster/credentials` exists with mode 0600, and the token passes `GET /v1/devices`. [manual] [approval]
      - GitHub token after login → not in `credentials`, `config.toml` or the output. [manual] [approval]
      - `api_url` in `config.toml` → the CLI calls that URL.
      - Account at 10 devices → CLI lists the 10 and asks which to replace. After a choice, login succeeds and the replaced token is rejected. [manual] [approval]
- Status: in progress.

## M3. Sync
- Delivers: memories converge across a user's devices, offline and in parallel. New device bootstraps with progress. Tested with seeded and manually written records; no extraction needed.
- Builds: DDB `memories` table (Appendix A), `POST /v1/sync/push`, `GET /v1/sync/pull`, 50k cap eviction, `SyncClient` (HLC, outbox, cadence, bootstrap), post-pull dedup, `loremaster status` (bootstrap progress).
- Covers: FR1, FR5, FR6, FR7, FR8, FR9, FR11, FR32, FR33, NFR2, NFR4, NFR7 (`memories` rows).
- Exit criteria:
  - Record written on device A recallable on B within 10 min while A's session continues (FR1).
  - Device offline then reconnected: both directions synced within 10 min, nothing lost (FR5, FR8).
  - Two devices create concurrently: all memories on both (FR6). Same memory changed on both: newest wins on both (FR7).
  - Offline device does not resurrect a deleted memory (FR9).
  - 50k memories bootstrap on a new device within 15 min with visible progress; recall works mid-bootstrap (FR11).
  - Session start <= 2 s P95 (NFR2).
  - Above 50k: oldest evicted in class order, applied on every device (FR33).
- Needs: M1, M2.
- Design: yes.
- Features:
  - F1. Push records: `memories` table and `POST /v1/sync/push` with last-write-wins. Docs: tasks. Needs: M2 F3. Status: not started.
    - Verify (Local):
      - 2 new facts → `applied` has both ids. Each item has a unique `seq`. `live_count` = 2.
      - Same body again → `applied` empty. `seq`, `live_count` unchanged.
      - Same `memory_id`, older `hlc` → not applied, stored item unchanged. Newer `hlc` → applied, higher `seq`.
      - Tombstone for a live record → item keeps only keys, `seq`, `hlc`, `writer`, `deleted`. `live_count` falls by 1.
      - 2 pushes at the same time for one user → all records stored, all `seq` unique.
      - 100 records → rejected, nothing written (status: OQ3).
      - User A and user B push the same `memory_id` → 2 separate items.
  - F2. Pull records: `GET /v1/sync/pull` pages by cursor. Docs: tasks. Needs: F1. Status: not started.
    - Verify (Local):
      - 3 stored, `cursor=0` → 3 records in `seq` order, `total` = 3, `more` false.
      - `cursor=<next_cursor>` → no records, `more` false.
      - Over 1 MB stored → `more` true on the first page. Following `next_cursor` returns each record once.
      - Offline device pushes after a reader's cursor → the next pull returns that record.
      - User B token → none of user A's records.
  - F3. Two devices converge: `SyncClient` push, pull, embed and cadence. `dev seed` queues rows in `outbox`. Docs: design, tasks. Needs: F2. Status: not started.
    - Verify (Two homes):
      - Seed a fact on A → within 10 min, recall probe on B returns it. B's row has a `vector` and `model_id`. The DDB item has no vector.
      - Service unreachable, seed on A → recall probe on A still returns memories, `outbox` has 1 row. Service back → within 10 min B returns it and A's `outbox` is empty.
      - Kill A's daemon with a row in `outbox`, restart it → record reaches B.
      - B offline while A writes, B reconnects → B gets A's record, A gets B's.
      - `dev bench` with `api_url` set → session start <= 2 s P95.
  - F4. Conflicts and deletes: concurrent changes converge, tombstones hold. Docs: tasks. Needs: F3. Status: not started.
    - Verify (Two homes):
      - A and B offline, each seeds a different fact, both reconnect → both facts on both, within 10 min.
      - Same `memory_id` on both with different `hlc` → the newer text on both, within 10 min.
      - B offline, tombstone seeded on A, B reconnects → recall probe on B no longer returns it.
      - B offline, older `hlc` change on B after a newer tombstone on A, B reconnects → tombstone wins on both.
      - Pulled record with `superseded_by` → recall probe does not return it.
  - F5. Post-pull dedup: near-identical facts from two devices become one. Docs: tasks. Needs: F4. Status: not started.
    - Verify (Two homes):
      - A and B seed the same text, same scope, both sync → larger ULID tombstoned on both. Same live row on both.
      - Same text, different scope → both stay live.
      - Unrelated texts → both stay live.
  - F6. Bootstrap with progress: new device pulls from `cursor=0`, resumes after restart, recalls while loading. `loremaster status` shows progress. Docs: tasks. Needs: F3. Status: not started.
    - Verify (Two homes, 50k records on the server):
      - New home → `loremaster status` shows loaded count rising towards `total`.
      - Recall probe mid-bootstrap → returns matches from loaded rows. Stop hook mid-bootstrap → turn in `turn_queue`.
      - Kill the daemon mid-bootstrap, restart → resumes from the saved cursor, ends with 50k rows, no duplicates.
      - 5-year-old laptop, 50 Mbps → all 50k recallable within 15 min. [manual]
  - F7. 50k cap: server evicts the oldest over the cap, in class order. Docs: tasks. Needs: F1, F3. Status: not started.
    - Verify (Local):
      - 50,000 live + 1 new, with one replaced fact, one episode, one other fact → oldest replaced fact tombstoned, `writer` = `server`, `live_count` = 50,000.
      - No replaced facts left → oldest episode evicted. No episodes left → oldest other fact evicted.
      - 49,999 live + 1 new → nothing evicted (FR32).
      - Device pulls an eviction → recall probe no longer returns it. No user-visible message.
- Status: not started.

## M4. Extraction
- Delivers: facts and episodes extracted automatically and recalled in later sessions, on any device.
- Builds: `POST /v1/extract`, `POST /v1/reconcile`, `POST /v1/episode` (prompts, Haiku 5.5 calls, size limits, daily token cap), `Pipeline`, `SessionTracker`, `loremaster dev eval`.
- Covers: FR2, FR3, FR14, FR15, FR16, FR17, FR18, FR19, FR25, FR30, FR31.
- Exit criteria:
  - Eval set: >= 80% expected memories stored, <= 20% junk (FR15).
  - Eval set: >= 80% injected memories relevant, 0 project leaks (FR25, FR26).
  - Eval set: 100% contradiction cases end with only the newer fact injected (FR31).
  - One episode per session, on SessionEnd or 60 min idle (FR3, FR17).
  - Episode recallable on device B within 10 min of session end, or 60 min after last turn if no session end (FR2, FR3).
  - Offline: turns stay queued, processed on reconnect; 429 leaves turns queued (FR19).
  - Fact told in one session recalled in a later session on the same device and on another device (FR1).
- Needs: M1, M2 to start. M3 for the FR1, FR2 and FR3 cross-device exit checks.
- Design: yes.
- Features:
  - F1. Extract endpoint: `POST /v1/extract` with server-held prompt, size limit and daily cap. Docs: design, tasks. Needs: M2 F3. Status: not started.
    - Verify (Dev stack) [approval]:
      - Turn stating "I prefer pnpm over npm" → 1 to 3 candidates, one with `kind` preference, `scope` user.
      - Turn with only pleasantries → `candidates` empty.
      - Turn over 32k tokens → 413, no Haiku call.
      - User over 5M tokens today → 429, no Haiku call.
      - No device token → rejected (status: OQ3).
  - F2. Reconcile endpoint: `POST /v1/reconcile` returns one decision. Docs: tasks. Needs: F1. Status: not started.
    - Verify (Dev stack) [approval]:
      - "Uses pnpm" + neighbour "Uses npm" → SUPERSEDE, neighbour id, text <= 2,000 chars.
      - Same meaning as a neighbour → SKIP.
      - Related topic, different fact → ADD.
      - User over 5M tokens today → 429.
  - F3. Facts from turns: `Pipeline` takes `turn_queue` to `memories`, with reconcile, outbox and retry. Docs: design, tasks. Needs: F2. Status: not started.
    - Verify (Dev stack for the first case, a stub service for the others):
      - Stop hook on a turn stating a preference, new session, relevant prompt → recall probe returns the fact. [approval]
      - Candidate with similarity >= 0.95 to a same-scope fact → no new row.
      - Candidate with similarity <= 0.75 → new row, row in `outbox`, no reconcile call.
      - Candidate in between, SUPERSEDE reply → new row, old row `superseded_by` it, old text unchanged. Recall probe returns only the new fact.
      - Project-scope candidate → row has the session's project ID.
      - Candidate over 2,000 chars → no row.
      - Service unreachable, restart the daemon → turn stays `pending`. Service back → processed.
      - Service replies 429 → turn stays `pending`.
  - F4. Episodes: `POST /v1/episode`, `SessionTracker`, one episode per session. Docs: tasks. Needs: F3. Status: not started.
    - Verify (Dev stack for the first case, a stub service for the others):
      - Session with turns, SessionEnd → one project-scoped episode with attempted, worked, failed. Recall probe on a related prompt returns it. Session's queued turns deleted. [approval]
      - Session with turns, no SessionEnd, 60 min after the last turn (clock set by the test) → one episode.
      - SessionEnd, then the idle timer passes → still one episode.
      - Turns over 100k tokens → 413 from the service; turns kept.
      - Session with no turns → OQ6.
  - F5. Eval: `loremaster dev eval` scores the labeled eval set, thresholds tuned to pass. Docs: tasks. Needs: F4, OQ1. Status: not started.
    - Verify (Dev stack) [approval]:
      - `loremaster dev eval <dir>` on the 20 labeled sessions → report with >= 80% expected stored, <= 20% junk, >= 80% injected relevant, 0 project leaks, 100% contradiction cases with only the newer fact injected.
      - Eval directory with a known wrong result → that score falls below its limit, exit non-zero.
  - F6. Extraction across devices: extracted facts and episodes reach another device. Docs: tasks. Needs: F4, M3 F3. Status: not started.
    - Verify (Two homes on the Dev stack) [approval]:
      - Preference stated on A → recall probe on B returns it within 10 min, while A's session continues.
      - SessionEnd on A → episode recallable on B within 10 min.
      - No SessionEnd on A → episode recallable on B within 60 min of the last turn.
- Status: not started.

## M5. Setup and lifecycle
- Delivers: one command takes a device from nothing to capturing; clean uninstall and account deletion.
- Builds: `install.sh`, GitHub Actions release builds (3 targets), `loremaster setup` (login, service unit, agent detection, hooks), `uninstall`, `update`, `DELETE /v1/account`, prod stack deploy.
- Covers: FR10, FR13, FR34, FR37, NFR8.
- Exit criteria:
  - Fresh macOS and Linux device reaches capturing within 5 min with one command and browser login (FR10).
  - Agent installed after Loremaster: `loremaster enable <agent>` makes all memory recallable next session (FR37).
  - Uninstall removes hooks, frees the device slot, keeps `store.db` unless `--purge` (FR13).
  - Account deletion: all device tokens rejected immediately, all DDB rows gone, local stores untouched (FR34).
  - Binaries run on macOS arm64, Linux x86_64/arm64 (NFR8).
  - `loremaster update` installs the latest release and restarts the service; previous-version clients still work against `/v1`.
- Needs: M1, M2 to start. M3 for the account-deletion exit check (`memories` rows).
- Features:
  - F1. Release and install: prod stack, 3-target release builds with the prod URL, `install.sh` with checksum check. Docs: tasks. Needs: M2 F5. Status: not started.
    - Verify:
      - `sam deploy --config-env prod` → stack up, `DeletionPolicy: Retain` on the prod API. [approval]
      - Push a release tag → GitHub Release with macOS arm64, Linux x86_64, Linux arm64 binaries and SHA-256 values. [approval]
      - `install.sh` on each platform → binary in `~/.loremaster/bin`, `loremaster --version` runs. [approval]
      - `install.sh` with a changed binary or wrong checksum → exits non-zero, nothing installed.
      - `install.sh` on Intel Mac or unsupported OS → exits non-zero with a message.
  - F2. `loremaster setup`: login, service unit, agent detection, hooks, bootstrap progress. Docs: tasks. Needs: F1, M3 F6. Status: not started.
    - Verify:
      - Fresh macOS and fresh Linux, install command + setup + browser login → within 5 min, a Claude Code turn appears in `turn_queue`. [manual] [approval]
      - Service unit → launchd agent on macOS, systemd user unit on Linux. Service restarts after a reboot. [manual]
      - Existing `~/.claude/settings.json` entries → kept. Our entries added once, also on a second run.
      - No Claude Code on the device → setup finishes, no hooks written. Install Claude Code, `loremaster enable claude-code` → next session's recall probe returns existing memory.
      - Existing account with 50k memories → setup shows bootstrap progress.
  - F3. `loremaster uninstall`: remove hooks and service, free the slot, keep the store. Docs: tasks. Needs: F2. Status: not started.
    - Verify (Local):
      - Uninstall → only entries whose command starts with our binary path are removed from `settings.json`. Other hooks unchanged. Service unit gone.
      - After uninstall → device absent from `GET /v1/devices`, its token rejected, `store.db` present.
      - `uninstall --purge` → `store.db` gone.
      - Service unreachable during uninstall → OQ7.
  - F4. `loremaster update`: install the latest release and restart the service. Docs: tasks. Needs: F1. Status: not started.
    - Verify [approval]:
      - Previous release installed, `loremaster update` → `--version` shows the latest, service restarted, recall probe works.
      - Wrong checksum for the download → exits non-zero, installed binary unchanged.
      - Previous-release client against the current service → login state, push and pull still work.
  - F5. Delete account: `DELETE /v1/account` revokes all tokens, then deletes all rows. Docs: tasks. Needs: M2 F3, M3 F1. Status: not started.
    - Verify (Dev stack) [approval]:
      - User with 2 devices and memories, `DELETE /v1/account` → 202. Both tokens rejected on the next request.
      - After the async run → no `users`, `devices` or `memories` items for that `user_id`.
      - Other user's rows → unchanged.
      - Local `store.db` → unchanged, recall probe still returns local memories.
      - Repeat the async delete on empty rows → no error.
- Status: not started.

## Deferred
- NFR5 (99.5% availability). Met by managed-service SLAs per design D19; no monitoring in P0, so no milestone verifies it.
- US15 to US20, FR35 and P1 Memory Management: out of scope for P0 per requirements. Codex CLI is the first P1 agent.

## Coverage
| Requirement or component | Milestones |
|---|---|
| FR1 | M3 (sync), M4 (extracted facts) |
| FR2, FR3 | M4 (M3 F3 carries the episode) |
| FR5, FR6, FR7, FR8, FR11, FR32, FR33 | M3 |
| FR15, FR16, FR17, FR18, FR19, FR25, FR31 | M4 |
| FR4 | M1 |
| FR9, FR30 | M1 (recall filter), M3 (tombstone sync), M4 (supersede) |
| FR10, FR34, FR37 | M5 |
| FR12 | M2 |
| FR13 | M2 (slot freed on the server), M5 (uninstall) |
| FR14 | M1 (capture), M4 (processing) |
| FR20, FR21, FR22, FR23, FR24, FR26, FR27, FR28, FR29, FR36 | M1 |
| FR38 | M1 to M5 |
| NFR1, NFR3, NFR9 | M1 |
| NFR2, NFR4 | M3 |
| NFR5 | Deferred |
| NFR6 | M2, M3 |
| NFR7 | M2 (devices), M3 (`memories` rows) |
| NFR8 | M1 (runs), M5 (release builds) |
| `ClaudeCodeAdapter` | M1 |
| `loremasterd`, `HookServer`, `Redactor`, `Embedder`, `Index`, `Recall`, `ProjectId` | M1 |
| `loremaster` CLI | M1 (`hook`, `enable`, `dev seed`, `dev bench`), M2 (`login`), M3 (`status`), M4 (`dev eval`), M5 (`setup`, `uninstall`, `update`) |
| SQLite store | M1 |
| `SyncClient` | M3 |
| `Pipeline`, `SessionTracker` | M4 |
| GitHub OAuth App, auth middleware, `users`, `devices` | M2 |
| `memories` DDB table | M3 |
| Sync endpoints, 50k cap | M3 |
| LLM endpoints | M4 |
| Account deletion | M5 |
| `install.sh`, release builds, prod stack | M5 |

## Open Questions
- OQ1. Labeled eval set (20 Claude Code sessions) must exist before M4 F5. Owner: user.
- OQ2. Design OQ4 (tunables) set during M4 F5.
- OQ3. Status codes the design does not define: missing, unknown or deleted device token; acting on another user's device; unknown `replace_device_id`; push over 99 records or text over 2,000 chars. Owner: /to-design, by M2 F3.
- OQ4. Test fakes: the design has no way for local tests to replace GitHub (M2) or Haiku (M4). Owner: /to-design M2 F2, /to-design M4 F1.
- OQ5. Where the Lambda holds the LLM credential, and which API it calls (Anthropic or Bedrock). The design says only that the key stays off devices. Owner: /to-design M4 F1.
- OQ6. Session with no turns: FR17 says every session produces one episode, and there is nothing to summarize. Owner: /to-brd, by M4 F4.
- OQ7. Uninstall with the service unreachable: FR13 requires the slot freed, and the design does not define this path. Owner: /to-design, by M5 F3.
- OQ8. Prerequisites. Owner: user.
  - By M2 F2: AWS account, `aws`, `sam`, `cargo-lambda`, DynamoDB Local, GitHub OAuth App. The 3 tools are not on the dev Mac today.
  - By M2 F5: a headless Linux box.
  - By M3 F6: a 5-year-old laptop on 50 Mbps.
  - By M4 F1: LLM credential.
