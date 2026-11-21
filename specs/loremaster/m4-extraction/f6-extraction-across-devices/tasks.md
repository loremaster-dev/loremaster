# M4 F6. Extraction across devices — Tasks

Input: `milestones.md` M4 F6, project `design.md` (Section 2 `Pipeline`, `SessionTracker`, Section 4 Sync), M3 F3 and M4 F3, F4 designs. No new code is planned: the Pipeline pushes after each handled step, B pulls on SessionStart and each 5 min, pulled rows are embedded and recalled. These tasks prove it end to end. A failing test is a bug in the existing path: fix it in its file (`pipeline.rs`, `sync.rs`, `recall.rs`, `server.rs`).

Tests go in `crates/loremaster/tests/sync.rs` and use its `home`, `session_start` and `user_prompt` helpers. Two daemons A and B, one `FakeService`, 2 devices of one user. A's transcript comes from `common::fixture_dir("claude-code")`, as `daemon_home` in `tests/pipeline.rs`.

## 1. A fact extracted on A is recalled on B [serial]
- [x] 1.1 Test `a_fact_extracted_on_one_home_is_recalled_on_another`: fake `Route::Extract` gives one user-scope preference. Stop hook on A, no SessionEnd → fake has the fact; SessionStart on B → `user_prompt` on B contains it. A's session row still has `ended = 0`. Test passes.
- [x] 1.2 Same test: no `Route::Episode` request yet, because A's session is still open. Assertion passes.

## 2. An episode made on A is recalled on B [serial, needs 1]
- [x] 2.1 Test `an_episode_after_session_end_is_recalled_on_another_home`: fake `Route::Episode` gives a fixed episode. Stop then SessionEnd on A → fake has one `RecordType::Episode` record with A's project id; SessionStart on B → `user_prompt` on B, in the same project, contains the episode. Test passes.
- [x] 2.2 Test `an_episode_after_idle_time_is_recalled_on_another_home`: queue turns on A with `commit_turn` `now_ms` 50 min in the past, then start A's daemon → turns extracted, then episode pushed with no SessionEnd; B pulls and recalls it. Test passes.
- [x] 2.3 Same project check: `user_prompt` on B from another project does not contain the episode. Assertion passes.

## 3. Milestone status [serial, needs 2]
- [x] 3.1 In `specs/loremaster/milestones.md`, set M4 F6 `Status: done`. M4 stays not done until F5.

## Dependencies
- 1 blocks 2. 2 blocks 3.
- /verify runs the 3 milestone cases on two sandbox homes on the dev stack with real Haiku. A dev deploy and Haiku calls need the user's approval.
- /verify case 1 and case 2: B gets the rows by its 5 min pull, not by a SessionStart, so the 10 min limit is tested as built.
- /verify case 3: a test clock. Set `sessions.last_turn_at` in A's sandbox store 50 min back, as test 2.2, then wait for A's next idle wake (5 min at most). A real 50 min wait is not needed.

## Open Questions
- None.
