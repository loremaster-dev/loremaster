# M4 F4. Episodes — Requirements

Keywords MUST, MUST NOT, SHOULD, MAY per RFC 2119.

Refines project FR2, FR3, FR17 and FR19. Resolves milestones OQ6. Terms as in the project `requirements.md`.

## Functional Requirements

Which sessions get an episode
- FR1. Session end for a session with 0 turns: Loremaster MUST NOT make an episode and MUST NOT call the service.
- FR2. Session end for a session with 1 or more turns: Loremaster MUST make at most one episode for those turns (user decision).
- FR3. Session with turns but no task attempted (for example only a general question or pleasantries): Loremaster MAY make no episode. The junk rules of project FR15 apply to episodes.
- FR4. Session where a task was attempted: Loremaster MUST make exactly one episode. Its text MUST state what was attempted, what worked and what failed.

Resume
- FR5. New turns with the session id of a session that already has its episode (for example after a resume): Loremaster MUST treat them as a new session for episodes. At its own session end, those turns MUST get at most one new episode, per FR1 to FR4 (user decision).
- FR6. Loremaster MUST NOT change or remove an earlier episode because of FR5.

Content
- FR7. Every episode MUST be project-scoped and MUST carry the project ID of its session.
- FR8. Episode text MUST NOT be longer than 2,000 characters (project FR18).
- FR9. Session turns over the episode input limit (100k tokens): Loremaster MUST still make the episode, from the first turn plus the most recent turns that fit under the limit (user decision). The size of a session MUST NOT stop its episode.

Timing
- FR10. Session end reported by the agent: the episode MUST be recallable on every other online device no later than 10 min after it (project FR2).
- FR11. No session end reported: the episode MUST be recallable on every other online device no later than 60 min after the session's last turn (project FR3).
- FR12. Session end reported, then 60 min with no turn: Loremaster MUST NOT make a second episode for the same turns.

Turns and failures
- FR13. Loremaster MUST NOT delete a turn before fact extraction of that turn is done (project FR14).
- FR14. After the episode of a session is written, or Loremaster gives up per FR16, Loremaster MUST delete that session's queued turns once FR13 holds.
- FR15. Service unreachable, device offline, daily cap reached (429) or model busy (503): Loremaster MUST keep the turns and MUST try again later, with no limit on tries (project FR4, FR19). These failures MUST NOT count toward FR16.
- FR16. The episode call gets an unusable reply (server error, or a reply that does not parse) 3 times for one session: Loremaster MUST make no episode for that session and MUST stop trying (user decision).
- FR17. Daemon restart or crash: pending episodes and their turns MUST NOT be lost. The 60 min idle time of FR11 MUST count from the last turn, not from the restart.

## Out of Scope
- Episodes that combine more than one session.
- Edits to an episode after it is written.
- A user action to make, change or remove an episode.

## Open Questions
- OQ1. A session whose working directory changes to another project between turns: which project ID the episode carries. Owner: /to-design.
