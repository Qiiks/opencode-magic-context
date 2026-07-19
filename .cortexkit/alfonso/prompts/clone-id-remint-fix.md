# Fix clone-session.ts id reminting (runner infinite-turn loop) + recreate the drive clone

Repo: this worktree (branch from `subc-migration` HEAD). The bug is proven by OpenCode source analysis (below); implement exactly, no deviation.

## Proven mechanism (from the OpenCode team, verified against live DB)

OpenCode's runner selects the latest user/assistant by RAW LEXICOGRAPHIC id comparison (`info.id > previous.id`), and its terminal guard exits only when `lastUser.id < lastAssistant.id`. Native message ids are `msg_` + 12-char lowercase-hex timestamp prefix + 14 base62 chars, monotonically ascending (MessageID.ascending). Our clone script (packages/plugin/scripts/clone-session.ts) reminted ARBITRARY ids (e.g. msg_yd..., msg_zy...) that sort ABOVE all future native ids (msg_f7...), so the guard is permanently false and the runner loops forever creating assistants parented to the poisoned newest user row (429 junk turns observed). TUI lifetime correlation: standalone TUI hosts the in-process runner.

## Fix requirements (all from the OpenCode prescription)

1. SAMPLE the native id format from the live DB first (read-only ~/.local/share/opencode/opencode.db, e.g. recent msg_f7.../prt_ ids) and pin the exact shape in a comment: prefix, 12-hex time component (verify epoch-ms hex? confirm from samples), 14-char base62 suffix. Do not trust this brief's description blindly — match observed native ids byte-shape exactly.
2. Remint ALL message ids in intended chronology (time.created ASC, old-id tie-break), generating sequential ASCENDING ids in the native scheme. Part ids likewise (prt_ scheme, ascending within the message stream). Monotonicity invariant: every generated id must sort lexicographically ABOVE the previous one and (for realism) encode the row's time_created.
3. Rewrite every reference: message.id + JSON data.id, assistant JSON parentID (remap via old->new user map), part.id + JSON data.id, part.message_id + JSON data.messageID + data.sessionID, and any compaction marker tail_start_id / summary references in message JSON.
4. Session row: create FRESH (id, parent-less, timestamps now, costs/tokens zeroed, share_url/summary_*/revert/time_compacting/time_archived NULL, copy agent/model/permission fields only deliberately). Copy ZERO session_input rows. Keep the existing MC-state copy behavior (tags/compartments etc.) but map tag keys and compartment message-id references through the SAME new id map (this already exists — verify it flows the new ids).
5. Terminal-assistant assert: after building the plan, assert the chronologically-final visible message is an assistant with finish != "tool-calls" and no unresolved local tool; if not, trim trailing rows until it is (log what was trimmed). Also assert max(user id) < max(assistant id) lexicographically post-remint; abort loudly otherwise.
6. Add a --delete <sessionId> mode (or a small companion path) that removes a previous clone's message/part/session rows AND its MC context.db session state (use the existing clearSession machinery) — guarded: refuses to delete a session that was not created by this script (check the marker the script writes, or require --force). 
7. Tests (clone-session.test.ts): id-scheme shape test (regex + monotonic ascending), terminal-guard test (max user < max assistant), reference-rewrite completeness on a fixture with parentID + compaction marker + tool parts, and the existing independence/dry-run tests stay green.

## Execution (after tests green)

8. DELETE the poisoned clone `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO` (opencode rows + MC state) using your new deletion path. The prod serve may be running — use short transactions and busy_timeout.
9. RE-CLONE fresh from source session `ses_16497a126ffeNpTn3xACl8Rmeg` (LEBENCH). Report the new clone session id prominently. Verify post-conditions with read-only queries: id scheme regex on newest 20 rows, max-user < max-assistant, terminal assistant finish, source untouched (row counts unchanged), MC state copied (tag/compartment counts).
10. Do NOT launch any opencode process; the drive is resumed by the primary.

## Gates
bun test scripts/clone-session.test.ts + typecheck + biome on changed files. Report: pinned native id shape (with 3 sampled examples), new clone session id, post-condition query results. No em-dashes in comments.
