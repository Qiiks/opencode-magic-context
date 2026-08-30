# GitHub issue #395 — bounded diagnostic logs

## Finding

Issue #395 is valid. `packages/plugin/src/shared/logger.ts` buffered entries in memory but appended every flushed batch to one `magic-context.log` forever. The temporary-directory reaper cannot reclaim a log that an active plugin keeps touching, so a long-lived OpenCode or Pi process could grow the file without limit.

Pi uses the same shared logger (`@magic-context/core/shared/logger`) for its runtime, so the change applies to both harnesses. `getMagicContextLogPath()` still resolves separate default `opencode/` and `pi/` temp subtrees.

## Fix

The shared logger now has a fixed 32 MiB structural cap for the active log and one `.1` predecessor:

1. It tracks the active path and byte count after the initial stat, rather than statting every buffered flush. It rechecks after 64 successful flushes or a path change, so a 20-flush hot-path probe performs one `statSync` call.
2. Before an append would cross 32 MiB, the logger copies at most the newest 32 MiB into `magic-context.log.1`, replaces the previous predecessor, truncates the current file in place, and appends the pending batch.
3. A single oversized buffered batch is clipped to the active-file cap, so it cannot bypass the bound. Existing oversized logs are reduced to a 32 MiB predecessor at their next rotation.
4. Both the active log and predecessor are created or tightened to owner-only `0600` permissions before diagnostic text is written. Rotation never copies text into a world-readable target.

Truncating the current inode instead of renaming it keeps `magic-context.log` continuously present. The CLI doctors and issue bundlers only read that active path, so a read racing rotation can receive the old, shortened, or new content without an `ENOENT` boundary crash. The `.1` predecessor is retained for manual inspection and is not required by existing readers.

All filesystem failures remain in `flush()`'s existing swallowed-write path: the buffered batch is discarded as before, the error is not thrown to the transform, and `swallowedWriteCount`, `lastErrorMessage`, and `lastErrorTime` are recorded. Directory creation remains non-memoized, preserving recovery after a temp-directory removal.

## Regression and mutation evidence

`packages/plugin/src/shared/logger.test.ts` runs production-mode child processes and covers:

- a file exactly at 32 MiB rotates on the next append;
- the predecessor is replaced on the next rotation, no `.2` file exists, and each persistent generation is at most 32 MiB;
- active and predecessor permissions are `0600` on POSIX;
- 20 explicit flushes cause at most one `statSync` call;
- healthy writes retain zero swallowed errors and an invalid file target increments the swallowed-write diagnostics once.

Mutation proof (issue #10588 discipline): the rotation predicate was temporarily replaced with `false /* NON-VACUITY BREAK: disable rotation */`. `bun test src/shared/logger.test.ts --timeout 30000` failed at `packages/plugin/src/shared/logger.test.ts:197`: expected the first predecessor byte `111` (`"o"` from the capped active file), received `112` (`"p"` from the stale predecessor). The predicate was restored immediately and the break marker was removed.

## Verification

- `bun test src/shared/logger.test.ts --timeout 30000` in `packages/plugin` — passed: 5 tests.
- `bun run test` in `packages/plugin` — passed: 4,239 tests across 381 files.
- `bun run typecheck` in `packages/plugin` — passed.

## Reply draft for #395

Thanks for the measured report — the diagnosis is correct. The logger kept appending to one active file, so temporary-directory cleanup could never age it out while the plugin was running. The shared OpenCode/Pi logger now enforces a fixed 32 MiB active log plus one 32 MiB `.1` predecessor, with no configuration knob. It tracks bytes in memory and only re-stats after 64 flushes or a path change, avoiding a filesystem stat on every transform-time write. Rotation keeps the current log inode in place, so doctor and issue-report readers continue to read the active path across the boundary. Both generations are owner-only (`0600`), and logger I/O failures still remain swallowed and visible in the existing diagnostics counters. Regression coverage proves the 32 MiB trigger, two-generation limit, cached hot path, permissions, and swallowed-error behavior.
