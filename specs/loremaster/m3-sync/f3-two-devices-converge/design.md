# M3 F3. Two devices converge — Design

Source: project `design.md` (Section 4 Sync, Cadence, Bootstrap, Section 6 local store), `requirements.md` FR1, FR5, FR8, NFR2, `milestones.md` M3 F3, M3 F1 and F2 designs.

## Overview
- A record written on device A reaches device B through the service, with the outbox kept across restarts and outages.
- `SyncClient` runs on its own thread in `loremasterd`. It pushes `outbox` rows to `POST /v1/sync/push`, pulls `GET /v1/sync/pull` pages, embeds them and writes them through `Recall`.

## Components
- `SyncClient`: new module `crates/loremaster/src/sync.rs`. One thread, blocking `ureq`, 10 s timeout on each request. Waits on an `mpsc` channel of triggers with a timeout, so it also wakes for the outbox check and the 5-min timer.
- `loremasterd` (`server.rs`): starts the `SyncClient` thread after `Recall::new`. The SessionStart handler sends a pull trigger and does not wait. Shutdown stops the thread after its current request.
- `Store` (`store.rs`): new methods for the outbox, `sync_state` and the HLC. No schema change: `outbox` and `sync_state` are in schema v1.
- `Recall` (`recall.rs`): new method that applies a pulled page: store write, then index insert in place. No index rebuild.
- `dev seed` (`dev.rs`): writer and outbox rows from `credentials`.
- `dev bench` (`dev.rs`): copies `config.toml` and `credentials` into its temp home.

## Data Model
- `outbox (memory_id)`: one row for each memory with a local change not yet pushed.
- `sync_state`:
  - `cursor`: the `next_cursor` of the last applied pull page. Missing: 0.
  - `hlc`: the device clock, as the last `Hlc` string ticked or observed. Missing: `Hlc::new(device_id)`.
- `memories`: pulled records keep their `seq`. `user_id` and `evict_key` from the pull are not stored.

## Interfaces
- API URL: as `loremaster login`. `api_url` from `config.toml`, else the build-time `LOREMASTER_API_URL`. Device: `credentials`, read again on each cycle.
- No API URL or no `credentials`: `SyncClient` does nothing. Recall and capture work as M1.
- Push body: `PushRequest` with at most 99 records, oldest outbox rows first. Each record is the local row with no `vector`, `model_id`, `seq`, `user_id` or `evict_key`. An outbox row with no `memories` row is removed.
- Replies, push and pull:
  - 200: see Flow.
  - 400 on push: remove the named `memory_id` from the outbox, log it, push the rest again.
  - 401: stop this cycle, keep the outbox, log "run loremaster login".
  - 409: push again up to 3 times. Pull again after 1 s, up to 3 times. Then wait for the next trigger.
  - Network error, timeout, 5xx: retry mode, see Cadence.

## Cadence
| Trigger | Action |
|---|---|
| Daemon start | push outbox, then pull |
| Every 5 s | `SELECT count(*) FROM outbox` > 0: push |
| SessionStart hook | pull, in the background |
| Every 5 min | push, then pull |
| Retry mode, every 30 s | push, then pull |

- Retry mode starts when a request fails with a network error, a timeout or a 5xx. It ends at the first success. In retry mode the 5-s check does not start a push.
- Writes from inside the daemon (extraction, M4) send a push trigger on the channel. `dev seed` writes from another process, so the 5-s check finds its rows.

## Flow
- Push:
  - 1. Read up to 99 outbox rows and their `memories` rows.
  - 2. `POST /v1/sync/push`.
  - 3. 200: in one transaction, delete each outbox row whose memory `hlc` is still the pushed `hlc`. A row changed during the push stays for the next push.
  - 4. Loop until the outbox is empty.
- Pull:
  - 1. Read `cursor` from `sync_state`.
  - 2. `GET /v1/sync/pull?cursor=N`.
  - 3. Keep each record that has no local row, or an `hlc` above the local `hlc`. Observe each record `hlc` into the device clock.
  - 4. Embedder ready: embed the kept live records. Not ready: no vector, `set_embedder` adds it later (M1 backfill).
  - 5. In one transaction: put the kept records, the new `cursor` and the new `hlc`.
  - 6. Update the index in place: insert live records, mark tombstones and superseded records not live.
  - 7. `more`: go to 2.
- `dev seed`:
  - With `credentials`: writer = `device_id`. In one transaction for each memory: read `hlc` from `sync_state`, tick it, put the memory, put its outbox row, write `hlc` back.
  - No `credentials`: writer `local`, `Hlc::new("local")`, no outbox row. As M1.
- `dev bench`: if the real home has `config.toml` and `credentials`, copy both into the temp home, mode 0600 for `credentials`. The daemon then pulls on start and on each session start of the bench. The 50k bench memories keep writer `bench` and have no outbox rows, so the bench pushes nothing of them.

## Testing
- `SyncClient` against the service router in process with DynamoDB Local, as the service tests, or a local HTTP stub for the 400, 401, 409, 5xx and timeout paths.
- Two homes: 2 `LOREMASTER_HOME` sandboxes, 2 devices of one user, 1 service.
- Verify case for `dev bench`: change it in `milestones.md` to "`dev bench --runs 20` with sync on → the bench completes and reports session-start P95. No limit." The NFR2 limit (<= 2 s P95 at 50k) moves to F6, which has 50k records on the server.

## Key Decisions
- D1. Outbox check every 5 s with `count(*)`. Alt: a file watch, or a socket call from `dev seed`. Why: no new dependency, and the check is cheap. In-daemon writes use the channel.
- D2. Retry every 30 s in retry mode. Alt: only the 5-min timer. Why: A pushes within 30 s of a reconnect, so A to B stays inside 10 min (FR5, FR8).
- D3. Writer from `credentials` in `dev seed`. Alt: always `local`. Why: the service needs `writer` = the caller's `device_id`. No credentials keeps M1 as it is.
- D4. Delete an outbox row only if the memory `hlc` is still the pushed `hlc`. Alt: delete all pushed ids. Why: a write during the push is not lost.
- D5. Push 400 removes the named row. Alt: keep it. Why: a bad row then does not block the outbox forever.
- D6. Read `credentials` on each cycle. Alt: read once at start. Why: a login after the daemon starts works with no restart.
- D7. Page, cursor and clock in one SQLite transaction. Alt: cursor after the page. Why: a daemon stop between pages continues from a saved cursor with no half-applied page.
- D8. HLC in `sync_state`, read and written in the transaction of each write. Alt: in memory in each process. Why: `dev seed` and the daemon both write, so the clock must stay monotonic across both.
- D9. Index updates in place through `Recall`. Alt: let `data_version` rebuild the index. Why: a rebuild at 50k is slow. The daemon writes on its own connection, so `data_version` does not change.
- D10. SessionStart sends a trigger and does not wait. Alt: pull before the profile. Why: session start stays fast (NFR2).
- D11. `dev bench` copies `config.toml` and `credentials`. Alt: a new flag. Why: no new flag, and the real store stays safe. Bench memories are not pushed, so a bench does not fill the user's server store.

## Open Questions
- None.
