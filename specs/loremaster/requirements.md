# Loremaster — Requirements

Cross-device, agent-agnostic memory for AI agents. Requirements apply to every supported agent; none are specific to one agent. Loremaster is invisible in normal use: the user never manages memory.

## Definitions
- **Agent**: an AI agent the user runs (e.g. a coding CLI).
- **Supported agent**: an agent Loremaster integrates with. P0: Claude Code. P1: Codex CLI.
- **Agent integration**: the per-agent connection that reports session start, prompt, turn and session end to Loremaster and accepts injected memory.
- **User**: one person with a Loremaster account, using one or more supported agents on 1 to 10 devices. A Loremaster account is created by signing in with a GitHub account.
- **Device**: a machine with Loremaster installed and logged in.
- **Online device**: a device with network access and Loremaster running.
- **Memory**: a fact (preference, decision, correction, environment fact, pitfall) or an episode (summary of one session).
- **Scope**: user (recalled in every project) or project (recalled only in that project).
- **Project ID**: normalized `origin` remote URL if the working directory is in a git repo with a remote (SSH and HTTPS forms normalize to the same ID, e.g. `github.com/a/b`); otherwise the last path segment of the working directory.
- **Profile**: a short briefing of key user and project facts, injected once at session start.
- **Turn**: one user prompt plus the agent's full response.
- **Session end**: as reported by the agent, or 60 min after the last turn if never reported.
- **Eval set**: 20 labeled real Claude Code sessions, each labeled with expected memories, junk, per-prompt relevant memories, and contradiction cases.

## User Stories

P0
- US1. As an agent user with multiple devices, I want my agents to have access to the same memory across devices.
- US2. As an agent user, I want a memory to be usable from any device no later than 10 min after it is recorded.
- US3. As an agent user working offline, I want recall and capture to keep working and to sync when I reconnect, so nothing is lost.
- US4. As an agent user running sessions on two or more devices at once, I want no device's memories lost.
- US5. As a new user, I want to sign up and set up my first device in one guided flow, with agent integration set up automatically for every supported agent on the device.
- US6. As an existing user, I want to add a device and have my full memory on it within 15 min.
- US7. As an agent user, I want the agent to remember durable preferences, decisions and corrections without being told.
- US8. As an agent user, I want project facts recalled only in that project, and user preferences recalled everywhere.
- US9. As an agent user, I want the same project on different devices to share project memory.
- US10. As an agent user, I want a newer fact to replace an older conflicting one, so the agent never acts on stale facts.
- US11. As an agent user, I want the agent to recall what happened in past sessions (attempted, worked, failed) from any device, so it does not repeat failed approaches.
- US12. As an agent user, I want memory to add no noticeable delay to my agent.
- US13. As an agent user, I want secrets kept out of memory, because stored memory syncs everywhere.
- US14. As a departing user, I want to delete my account and all server data.

P1
- US16. As an agent user, I want to say "remember X" or "forget X" in a session.
- US17. As a user with a lost device, I want to revoke that one device without disturbing the others.
- US18. As an agent user, I want to export my memories in an open format.
- US19. As a Windows user, I want native Windows support.
- US15. As a user of several agents, I want them all to share one memory, so a fact learned in one agent is recalled in another.
- US20. As a user of agents beyond Claude Code, I want them supported too, starting with Codex CLI.

## Flows (P0)
- F1. Sign up and first device: install command, browser login, per-device credential issued, agent integration installed for each supported agent on the device, capturing.
- F2. Add device: install command, browser login, bootstrap with visible progress, partial recall during bootstrap.
- F3. Session: session start injects profile; each prompt injects relevant memories; each turn processed for new memories; session end produces episode.
- F4. Cross-device: fact stated on device A is recalled on device B within 10 min, while A's session continues.
- F5. Offline: device works offline, reconnects, syncs both directions within 10 min.
- F6. Parallel: two or more devices active at once; memories from all of them survive on every device.
- F7. Account deletion: credentials revoked immediately, server data deleted within 30 days.
- F8. Uninstall: agent integrations removed, local store kept unless user requests deletion.

## Conventions
The key words MUST, MUST NOT, REQUIRED, SHALL, SHALL NOT, SHOULD, SHOULD NOT, RECOMMENDED, MAY and OPTIONAL are to be interpreted as described in RFC 2119 and RFC 8174 when, and only when, they appear in all capitals.

"Loremaster" below means the whole system: software on each device plus the sync service.

## Functional Requirements

Sync and freshness
- FR1. A fact created on any device MUST be recallable on every other online device no later than 10 min after the end of the turn that produced it.
- FR2. An episode MUST be recallable on every other online device no later than 10 min after session end.
- FR3. When the agent does not report a session end, Loremaster MUST still produce the episode, and it MUST be recallable on every other online device no later than 60 min after the session's last turn.
- FR4. While a device is offline, recall MUST work against local memory and MUST meet NFR1. Turns from offline sessions MUST persist on the device across restarts and crashes and MUST NOT be lost. Processing them for new memories MAY wait until the device is online.
- FR5. When a device reconnects after any offline duration, its local changes MUST reach the server and remote changes MUST reach the device within 10 min of reconnect. Memories MUST NOT be lost.
- FR6. When two or more devices create memories concurrently, every memory from all of them MUST exist on every device within 10 min.
- FR7. When two or more devices change the same memory concurrently (update or delete), the most recent change MUST win on every device, and all devices MUST converge to the same state within 10 min.
- FR8. When the sync service is unavailable, devices MUST behave as offline per FR4. On recovery, FR5 applies.
- FR9. A deleted or replaced memory MUST NOT be injected again on any device. A device that was offline during the deletion MUST NOT resurrect it.

Device onboarding
- FR10. Device setup MUST need only one install command and a browser login, plus one in-agent approval step for any supported agent that requires user approval of hooks, and MUST reach capturing within 5 min. Setup MUST issue a per-device credential and install the integration for every supported agent on the device.
- FR11. Assuming 50k memories, a 50 Mbps connection and a laptop 5 years old or newer, a newly added device MUST have all memories recallable within 15 min. Loremaster MUST show bootstrap progress. Recall and capture MUST work on partially loaded memory during bootstrap.
- FR12. Loremaster MUST refuse an 11th device login with a message listing the user's current devices, and MUST let the user replace one of them. The replaced device's credential MUST be rejected immediately.
- FR13. Uninstall MUST remove all agent integrations and MUST free the device slot on the server. Uninstall MUST keep the local memory store unless the user requests its deletion.

Capture
- FR14. Loremaster MUST process every turn for new memories, with no user action.
- FR15. On the eval set, Loremaster MUST store at least 80% of labeled expected memories, and stored memories labeled junk MUST NOT exceed 20% (temporary task state, content visible in code or git history, generic knowledge, restated agent actions, pleasantries).
- FR16. Every memory MUST have user or project scope. A project-scoped memory MUST carry the project ID of its session's working directory.
- FR17. Every session in which a task was attempted MUST produce exactly one episode recording what was attempted, what worked and what failed. A session with no turns MUST NOT produce an episode. Episodes MUST be project-scoped. Details: M4 F4 `requirements.md`.
- FR18. Loremaster MUST NOT create memories longer than 2,000 characters.
- FR19. Processing turns for new memories MUST NOT require anything from the user beyond the Loremaster login: no extra API key, account or per-call charge. Turns not yet processed MUST NOT be lost.

Secrets
- FR20. Loremaster MUST redact secrets on the device before a turn is processed, stored or synced. A secret MUST NOT leave the device or be stored.
- FR21. Loremaster MUST redact 100% of a fixed secret test corpus (cloud provider keys, API tokens, private keys, JWTs, passwords in connection strings).
- FR22. Users MUST be able to add custom secret patterns. Matching text MUST be redacted per FR20 from that point on.

Recall
- FR23. At session start, Loremaster MUST inject a profile of at most 300 tokens. The profile MUST contain the user's most important user-scope facts, then the current project's facts. Memories that do not fit MUST be left out.
- FR24. On each prompt, Loremaster MUST inject only memories relevant to the prompt, at most 500 tokens. Loremaster MUST NOT inject anything when nothing is relevant.
- FR25. On the eval set, at least 80% of injected memories MUST be labeled relevant to their prompt.
- FR26. A project-scoped memory from project X MUST NOT be injected in a session whose project ID is not X. The eval set MUST show 0 occurrences.
- FR27. User-scoped memories MUST be eligible for injection in every project.
- FR28. Episodes MUST be recalled by relevance, like facts. Older episodes SHOULD rank lower than newer episodes of equal relevance.
- FR29. When a latency budget (NFR1 to NFR3) is exceeded, the agent MUST proceed without memory for that event. Loremaster MUST NOT block the agent.
- FR30. When a new fact contradicts an existing fact in the same scope, the most recent fact MUST win. Loremaster MUST NOT inject the older fact again. Exception: contradicting facts created on different devices before either has synced the other's MAY both survive.
- FR31. On the eval set, 100% of labeled contradiction cases MUST end with only the newer fact injected.

Retention and limits
- FR32. Below 50k memories per user, Loremaster MUST NOT delete any memory automatically.
- FR33. When a user exceeds 50k memories, Loremaster MUST delete the oldest memories until the total is 50k, in this order: replaced facts, then episodes, then other facts. Loremaster MUST NOT notify the user. Deletions MUST apply on every device.

Account
- FR34. On account deletion, the server MUST reject every device credential immediately. All server-side data (memories, sync history, device records) MUST be deleted and unrecoverable within 30 days. Loremaster MUST NOT touch local stores.

Agent agnosticism
- FR36. Memory text, type, scope and project ID MUST NOT depend on the agent that produced the memory. The same working directory MUST yield the same project ID in every agent.
- FR37. When a supported agent is installed on a device after Loremaster, the user MUST be able to enable its integration with one command. All existing memory MUST be recallable in it from its next session.
- FR38. Every P0 FR and NFR MUST hold for each supported agent individually.

## Non-Functional Requirements
- NFR1. Performance: recall MUST add at most 100 ms at P95 to each prompt, measured at 50k memories.
- NFR2. Performance: session start MUST add at most 2 s at P95, including sync.
- NFR3. Performance: capture MUST add at most 50 ms at P95 to the end of a turn.
- NFR4. Scale: Loremaster MUST support up to 50,000 memories per user, up to 2,000 characters each, and up to 10 devices per user.
- NFR5. Availability: the sync service MUST meet 99.5% monthly availability.
- NFR6. Durability: server-stored data MUST survive the loss of any single storage location.
- NFR7. Security: all device-to-server traffic MUST be encrypted in transit. Memories MUST be encrypted at rest on the server; device storage relies on OS disk encryption. A device credential MUST NOT read or write another user's data.
- NFR8. Platforms: Loremaster MUST run on macOS (Apple Silicon) and Linux (x86_64 with AVX2, arm64). WSL counts as Linux. Intel Macs are out of scope (no prebuilt ONNX Runtime).
- NFR9. Extensibility: adding a new supported agent MUST NOT require changes to stored memories or a migration on existing devices.

## Out of Scope
- Memory management interface and notifications in P0 (see P1: Memory Management).
- Syncing or resuming sessions or transcripts across devices.
- End-to-end encryption.
- Recovery phrase or passphrase.
- Team or org shared memory.
- Procedural memory (rules, skills).
- Manual project rename or override; directory-name collisions between unrelated non-git projects are accepted.
- Business model and pricing.
- Agents other than Claude Code. Codex CLI is the first P1 agent.
- All P1 stories (US15 to US20) for this release.

## Open Questions
- None.

## P1: Memory Management
Not part of P0. The P0 system MUST work with no management interface.

User stories
- P1-US1. As an agent user, I want to see, search, edit, pin and forget my memories, with changes applied on every device.
- P1-US2. As an agent user, I want a hosted web dashboard to manage memories from any browser.

Flows
- P1-F1. Manage: user edits, pins or forgets a memory; change reaches all online devices within 10 min.

Requirements
- P1-FR1. Every device MUST provide a local interface to list, search, view (text, type, scope, project, source device, created date), edit text, change scope, pin, unpin and forget memories.
- P1-FR2. A change made in the interface MUST be applied on every online device within 10 min.
- P1-FR3. A pinned memory is one the user marks as always relevant. Pinned memories MUST take priority in the profile (FR23), newest first, within its 300-token cap, and MUST NOT be deleted by FR33.
- P1-FR4. When pinned memories exceed 300 tokens, the interface MUST show a warning.
- P1-FR5. User edits MUST NOT exceed 2,000 characters; Loremaster MUST reject longer edits with an error.
- P1-FR6. When a user reaches 45k memories, the interface MUST show a warning stating the count and the 50k limit.
- P1-FR7. A hosted web dashboard MUST offer the actions in P1-FR1.

## P1: More Agents
Not part of P0. P0 keeps the adapter boundary (FR36, NFR9), so adding an agent adds an adapter only.

Flows
- P1-F2. Cross-agent: fact stated in one supported agent is recalled in another supported agent, on the same device or another device.

Requirements
- FR35. A memory created in any supported agent MUST be recallable in every other supported agent, on the same device and on other devices, within the bounds of FR1 and FR2.
- P1-FR8. Codex CLI MUST be a supported agent. A Codex adapter written from Codex's published source is kept in `p1-codex/`; it has never run against a real Codex session.
