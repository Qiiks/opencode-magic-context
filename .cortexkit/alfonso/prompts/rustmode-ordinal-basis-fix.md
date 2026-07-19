# Fix: compartment ordinal basis conflict blocks rust-mode/state-sync on marker-bearing sessions

Repo: this worktree (branch from `subc-migration` HEAD). TypeScript only.

## Diagnosed failure (live evidence, do not re-derive the diagnosis, verify it)

Rust-mode beat 1 on the cloned session `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO` failed every pass with `module state sync mismatch` (thrown at module-state-sync.ts:815, payload "mismatch" from `serializeCompartment`).

Root cause, verified against the live DBs:
- Stored compartment boundary ordinals (`compartments.start_message` / `end_message`) were computed by the TS pipeline on a basis that INCLUDES MC's synthetic compaction-summary rows in opencode.db. Verified: newest compartment end ordinal stored as 11719 for `msg_2P5mSoAYNofR0BcS7rJTPinkvKy3`, but the canonical order EXCLUDING summary rows puts that id at 11718 (the session has exactly 1 summary row).
- `resolveOrdinalsForModule` (rust-mode-transform.ts / module-wire.ts) resolves wire-message ordinals on the summary-EXCLUDING canonical basis (`readRawSessionMessages*` filters summary rows symmetrically).
- Both write into the SHARED `idOrdinalMemo`. When the same message id (a compaction boundary user message is typically also on the wire) gets ordinal N from the resolver and declared ordinal N+1 from `serializeCompartment`'s stored value, `ordinalForMessageId` (module-state-sync.ts:344-360) returns "mismatch" and the whole sync throws.
- Every compartmentalized session has at least one marker summary row, so this also explains a chunk of the shadow soak's persistent `seed-boundary mismatch` / reseed classes: the shadow lane hits the same conflict and fail-opens; rust mode hits it as an authority failure (LKG replay/raw).

## The fix

Make `serializeCompartment` STOP trusting the stored `start_message`/`end_message` ordinals and derive boundary ordinals on the canonical (summary-excluding) basis, so one basis rules everywhere:

1. In module-state-sync.ts, add a canonical ordinal resolver for a message id: `canonicalOrdinalForMessageId(sessionId, messageId)` = COUNT of non-summary messages at-or-before the target in `(time_created ASC, id ASC)` order. Implement as a single indexed opencode.db query per UNIQUE id, memoized in the existing `idOrdinalMemo` (same generation discipline). The existing `readRawSessionMessagePartsById` already point-reads the row; extend that module (read-session-db.ts or wherever the by-id reader lives) with the count query rather than hand-rolling SQL in the sync file. Seeds resolve ~2 ids per compartment once (few hundred point counts, one-time); steady state resolves 0-2 new ids per pass.
2. `serializeCompartment` uses the canonical resolver for start/end ordinals. The stored `compartment.startMessage/endMessage` values remain untouched in the DB (TS mode consumes them on its own basis) but are no longer sent to the module. Keep the null path (missing raw row -> "unresolved") exactly as now. The "mismatch" arm now only fires if the memo holds a DIFFERENT canonical value for the same id within a generation, which after this change indicates genuine drift (kept as a fail-loud).
3. Check `flatBlockIdForRawMessage` and the seed-boundary derivation for any other consumer of the stored ordinals and align them to the canonical resolver if found. Report what you find either way.
4. Audit the shadow sender leg: it uses the same `serializeCompartment` via module-state-sync (post-U5 refactor), so the fix covers both lanes; confirm no second copy of the stored-ordinal read survives in shadow-sender.ts.

## Tests (fail-first)

- Regression: session fixture with a mid-history summary row (summary=true assistant message in the raw store), compartments whose stored ordinals were computed on the includes-summary basis (i.e. stored = canonical+1 for post-summary boundaries), and a wire array containing the boundary id. Assert: pre-fix behavior produced "mismatch" (write the test against the OLD code path first and flip it), post-fix the sync payload serializes with canonical ordinals and no throw.
- Memo-coherence: resolver-assigned wire ordinal and compartment boundary ordinal for the same id agree; a genuinely conflicting canonical value still returns "mismatch".
- No-summary session: ordinals unchanged by the fix (stored == canonical when no summary rows exist).

## Gates

Focused: module-state-sync, rust-mode-transform, shadow-sender suites. Then full `bun test` in packages/plugin (attribute any failure against base before claiming pre-existing). Typecheck + biome. Comments explain the two-bases invariant without referencing this incident or plan files. No em-dashes.
