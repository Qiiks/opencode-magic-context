# Fix: authority state_sync seq fence rejects fresh adapter processes

Repo: this worktree (branch from `subc-migration` HEAD). Rust + TS, both small. Investigation-first: verify the mechanism before coding.

## Live evidence

Rust-mode beat on `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO`, third process lifecycle of the day: `rust transform failed; attempting LKG replay: expected_authority_seq 0 did not match durable authority_seq 1`. An earlier process's authority state_sync committed durable seq 1; the fresh TUI process's adapter state starts `lastAckedSeq: 0` (in-memory, process-lifetime) and its first sync sends expected 0, which the durable fence rejects.

The seq CAS was inherited from the shadow protocol, where a mismatch means a possibly-poisoned mirror and the answer is reset+reseed. For the AUTHORITY lane the sender is the single writer for its session; a fresh process with stale in-memory seq is a routine, legitimate event (every restart). It must self-heal without human intervention and without discarding durable module state.

## Fix (pinned design)

ADOPT-AND-RETRY-ONCE on the typed mismatch:

1. Module (crates/mc-module/src/lib.rs + mc-store as needed): the authority-lane seq-mismatch rejection must be a TYPED error carrying the durable value, e.g. `{code: "authority_seq_mismatch", durable_authority_seq: N}` (verify the exact error-shaping convention used by other typed module errors and match it). Shadow lane behavior unchanged.
2. TS adapter (module-state-sync.ts / rust-mode-transform.ts): on receiving that typed error from an authority sync, adopt the durable seq into `state.lastAckedSeq`, clear `lastAckedWatermarks` (the fresh process does not know what the durable side has acked; a forced full/watermark-rebuild sync after adoption is acceptable and simplest — reason about which and document the choice in a comment), and retry the sync ONCE within the same pass. A second mismatch in the same pass fails the pass normally (LKG/park ladder handles it) to prevent loops.
3. Confirm the fence's purpose survives: two CONCURRENT processes on the same session (split-brain) must still be detected — adoption must not let two writers silently interleave. The single-adopt-per-pass rule plus the CAS means concurrent writers keep colliding and parking, which is the correct outcome; add a test proving two interleaved senders do not both succeed silently.

## Tests (fail-first)

- Fresh-process bootstrap: durable seq N > 0, adapter seq 0 -> first sync gets typed mismatch, adopts, retries, succeeds; transform proceeds (fail-first: today the pass fails).
- Same-pass second mismatch -> pass fails, no infinite retry.
- Interleaved concurrent senders -> at least one keeps failing; no silent dual-writer.
- Shadow lane mismatch behavior unchanged (existing tests stay green).

## Gates

cargo test -p mc-module --lib + integration, clippy -D warnings; focused plugin suites (module-state-sync, rust-mode-transform) + typecheck + biome. Comments explain the authority-vs-shadow mismatch semantics without referencing this incident. No em-dashes.
