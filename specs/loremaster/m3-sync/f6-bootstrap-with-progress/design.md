# M3 F6. Bootstrap with progress — Design

Source: project `design.md` (Section 4 Bootstrap), `requirements.md` FR11, `milestones.md` M3 F6, M3 F3 design and code.

## Overview
- A new device loads all of the user's memories with visible progress, continues after a restart, and recalls while it loads.
- The F3 pull already does the load. F6 saves the pull `total` and adds `loremaster status`, which reads progress from `store.db`.

## Already built (F3)
- A new home has no `cursor` in `sync_state`, so the first pull starts at 0.
- Each page, its cursor and the clock are written in one transaction, so a restart continues from the saved cursor with no duplicates.
- Each page goes into the store and the index in place, so recall uses it at once. Capture runs on its own thread and takes the store lock only briefly.

## Components
- `Store` (`store.rs`): `apply_pulled` also saves `total`. New `Store::progress() -> Progress`.
- `Recall::apply_pulled` (`recall.rs`): passes `page.total` to the store.
- `loremaster status` (`main.rs`, new module `status.rs`): prints sync state, progress and outbox size.

## Data Model
- `sync_state.total`: `total` of the last applied pull page, the user's live records on the service. Written in the page transaction, with `cursor`.
- `Progress {recallable: i64, total: Option<i64>, outbox: i64}`:
  - `recallable`: rows with `deleted = 0` and `model_id` = the current model.
  - `total`: `sync_state.total`, `None` before the first pull.
  - `outbox`: rows in `outbox`.

## Interfaces
- `loremaster status`, exit 0:
  ```
  Sync: on, device dev_01M6...
  Memories: 1,234 of 5,000 recallable
  Outbox: 0 waiting
  ```
  - Sync line: "on, device <device_id>" with a service URL and a credential. "off: no api_url" or "off: no login" when one is missing, URL checked first.
  - Memories line: "Memories: N recallable" while `total` is `None`. Numbers have thousands separators.
  - A store that cannot be opened (newer schema, not a database): the store error on stderr, exit 1.
- Reads `store.db` directly, not through the daemon, so it works while the daemon is stopped.

## Verify
- Two homes, 5,000 records of 2,000 characters on the local service (about 10 pages):
  - New home → `loremaster status` shows the recallable count rising towards `total`.
  - Recall probe mid-bootstrap → matches from loaded rows. Stop hook mid-bootstrap → turn in `turn_queue`.
  - Kill the daemon mid-bootstrap, restart → continues from the saved cursor, ends with 5,000 rows, no duplicates.
- Not verified in P0: the 15-min bootstrap on a 5-year-old laptop (no such machine), and session start <= 2 s P95 with 50k records on the server.

## Key Decisions
- D1. `status` reads `store.db`. Alt: a new daemon socket verb. Why: works with the daemon stopped, and no new protocol.
- D2. Recallable = live rows with a current-model vector. Alt: all live rows. Why: FR11 asks for memories that are recallable, and a row with no vector is not.
- D3. `total` saved with the page. Alt: keep it in daemon memory. Why: `status` runs in another process, and the value holds across a restart.
- D4. Verify with 5,000 records, and the two performance cases not verified in P0. Alt: 50k on the local service. Why: 10 pages are enough for progress and a mid-load kill; the performance cases need a machine we do not have, or test only speed.
- D5. Keep the F3 order: embed a page, then store it. Alt: store first, embed later. Why: no change to F3; rows still become recallable one page at a time.

## Open Questions
- None.
