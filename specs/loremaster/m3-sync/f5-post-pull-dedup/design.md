# M3 F5. Post-pull dedup — Design

Source: project `design.md` (Section 3 post-pull dedup, Section 4 Sync), `requirements.md` FR9, FR26, FR30, `milestones.md` M3 F5, M3 F3 and F4 designs and code.

## Overview
- A fact that two devices wrote with near-identical text, in the same scope, ends as one live fact on every device.
- When a fact gets a vector on this device, the device compares it with its local live facts. For each pair with similarity >= 0.95, it tombstones the larger ULID, and the tombstone syncs as any local delete.

## Components
- `Index` (`crates/loremaster/src/index.rs`): new `near_duplicates`.
- `Recall` (`crates/loremaster/src/recall.rs`): new private dedup step, called from `apply_pulled` and `add_vectors`. `set_embedder` gets `writer: Option<&str>`, the device id, and passes it to `add_vectors`.
- Daemon (`crates/loremaster/src/server.rs`): reads the credential with `config::load_credential` when the model has loaded, and passes its `device_id` to `set_embedder`.
- `Store::write_local` (`store.rs`, F3): writes the tombstone and queues it for push. No store change.
- No service change. No new table or column.

## Interfaces
- `Index::near_duplicates(&self, memory_id: &str) -> Vec<&str>`: the other live facts whose vector has dot product >= `DUPLICATE_SIMILARITY` (0.95) with the vector of `memory_id`, and the same `scope`. For project scope, also the same `project_id`. Never episodes, superseded or deleted rows, or the memory itself. Unknown or not-live `memory_id`: empty.

## Flow
- 1. A fact gets a vector: in `Recall::apply_pulled` after the index update, or in `Recall::add_vectors` after `set_vector` and the index insert.
- 2. For each such live fact, `Index::near_duplicates`. For each pair, the larger `memory_id` is the loser.
- 3. Each loser: `Store::write_local` with a tombstone record (`memory_id`, `deleted = true`, empty `hlc`, no text or metadata), `writer` = this device. It gets the next device stamp and goes to the outbox. No login (`writer` is `None`): no dedup. Only a logged-in device pulls, so only it can have another device's duplicate.
- 4. `Index::mark_not_live` on each loser, so the next comparison and recall skip it.
- 5. The next push sends the tombstones. The other device does the same comparison and tombstones the same loser. Both tombstones are for one `memory_id`, so last write wins gives one result.

## Key Decisions
- D1. Duplicates need the same `scope`, and for project scope the same `project_id` (user decision). Alt: scope only. Why: the same text in two projects is two memories; a tombstone would take a fact away from its project (FR26).
- D2. Dedup runs wherever a fact gets a vector: `apply_pulled` and `add_vectors` (user decision). Alt: `apply_pulled` only. Why: a fact pulled before the model loads gets its vector in `add_vectors`; without dedup there, two devices that both pulled early keep both facts.
- D3. Compare each new fact with the index, not pairs inside a page. Why: page facts are in the index before the step, so one search covers both local and pulled facts.
- D4. Tombstone the larger ULID, through `write_local`. Alt: `superseded_by` the smaller one; a server-side dedup. Why: the project design rule; a tombstone is the F4 delete path, and both devices choose the same loser with no coordination.
- D5. A tombstone is never undone. Chains (a~b, b~c, a not ~ c) can end with different live sets on two devices for a short time. Each device only adds tombstones, and tombstones sync, so both end with the union: the smallest ULID of each chain stays live.
- D6. Exact scan of the index for each new fact. Known limit: a 50k bootstrap does 50k scans, about the cost of one recall search each, which is small next to embedding each fact. Upgrade path: an ANN index (Section 3 `Index` note).
- D7. Threshold 0.95 as a constant in `index.rs`. Why: project design OQ4 tunes it on the eval set (M4 F5).

## Testing
- Unit (`index.rs`): 0.96 found, 0.94 not; other scope, other `project_id`, episode, superseded and not-live rows not found.
- `tests/sync.rs` (fake service, two homes, F4 pattern): pulled duplicate with the larger ULID tombstoned; local duplicate with the larger ULID tombstoned; same text in another scope or another project, unrelated text, and an episode stay live; a duplicate pulled before the model loads is tombstoned when the model loads; two homes end with the same live fact and the same tombstone.
- /verify: the three M3 F5 cases with `dev seed` on two homes on the local service.

## Open Questions
- None.
