# Fix: shadow-mode soak dies at cold-start (seed protocol + quarantine hygiene)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Two sides: TS sender (packages/plugin/src/hooks/magic-context/shadow-sender.ts + transform wiring) and Rust module (crates/mc-module/src/transform.rs shadow arms + shadow meta).

## Evidence (live soak data, 2026-07-11 morning)

2,900 rows in shadow_divergences decompose into ONE root cause — the cold-start bootstrap:
- ALL 51 decision-mismatch rows are pass_seq=1, zero at pass>=2. Shape is always: TS decision = defer (warm production session, cached m0/m1 state) vs RS decision = HARD/first_render, boundary Absent (fresh shadow store row). That asymmetry is DESIGNED — a fresh shadow lineage must first-render while the real lane is warm mid-flight.
- The 1 trim-mismatch row: TS advanced its compaction marker on a warm session; shadow store had no compartments yet (predicate boundary_identity, durable boundary ""). Correct validation, missing seed data.
- 2,848 "quarantined" rows are per-pass spam: pass-1 mismatch quarantines the lineage, then EVERY subsequent pass appends another quarantined row (one session has 291, all recording pass_seq 1) and is never compared again.
- Only 1 shadow compartment was ever synced: quarantine kills the lineage before compartments ride state_sync.

Net: the byte-compare has judged ZERO warm passes. The soak dies at bootstrap on every session.

## The fix

### A. Sender: seed protocol after every shadow_reset
After a shadow_reset acks (fresh generation), and BEFORE the first shadow_transform of that generation, send a complete seed state_sync: ALL compartments for the session (tier columns included), memory state, marker state — whatever the state_sync op already carries, but complete rather than delta. Mark the first shadow_transform of a fresh generation with a seed flag on the wire body (e.g. seed_pass: true) so the module knows this pass calibrates rather than judges. Keep FIFO ordering guarantees (reset -> seed sync -> transform) within the per-session queue.

### B. Module: seed pass calibrates, does not judge
On a seed-flagged pass (or equivalently the first transform after generation bump): run the shadow transform normally (the shadow lineage takes its own first HARD — expected), commit shadow state, but DO NOT byte-compare or write divergence rows for that pass. Comparison verdicts start from the NEXT pass, when both lanes are warm.

### C. Quarantine hygiene
- A quarantined lineage writes ONE terminal divergence row (the row that caused it) + maintains a counter (quarantined_pass_count) on shadow meta — subsequent passes while quarantined must NOT append new rows.
- shadow_reset UN-quarantines (fresh generation starts clean, seed protocol reruns). The sender should detect quarantine (from the transform response) and schedule a reset+reseed once (not a hot loop — one retry per session per process run is enough; if it re-quarantines after a clean reseed, leave it quarantined: that's then a REAL divergence to investigate).
- Existing 2,900 rows: add a one-off cleanup that deletes rows with class='quarantined' (they carry no information; the terminal rows and real classes stay).

### D. Keep honest
The pass-1 defer-vs-HARD asymmetry is designed, but do NOT special-case away real first-pass divergences forever: after seeding, the SECOND pass on a fresh generation compares fully — including boundary/trim state. If seeding worked, TS and RS boundary state should agree there; if they don't, that's a genuine finding the comparator must report.

## Tests (non-vacuous, must actively fail if the mechanism is wrong)
1. Seed pass: fresh generation -> seed sync -> first transform commits shadow HARD, writes ZERO divergence rows.
2. Warm compare: the pass AFTER seeding, with identical inputs, produces zero divergences AND a deliberately injected byte flip (mutate one frozen unit in the shadow store between passes) produces exactly one real divergence row — proving the comparator actually compares warm passes.
3. Quarantine: after a real divergence, repeated passes add ZERO new rows (counter increments); shadow_reset un-quarantines and the seed protocol reruns.
4. Sender ordering: reset -> full seed sync (compartments included) -> seed transform, FIFO per session, verified against the wire-shape fixture (extend the cross-language serde fixture if the wire body gains the seed flag — regenerate, don't hand-edit).
5. Compartment seeding: a session with N compartments in context.db lands N shadow compartments in mc_compartments under shadow:<sid> after seed.

## Gates
- cargo test -p mc-module (+ mc-store if touched), clippy --all-targets, fmt.
- cd packages/plugin && bun test, typecheck, lint.
- Regenerate the shadow wire fixture if the wire shape changed (bun packages/plugin/scripts/generate-shadow-wire-fixture.ts) and keep the Rust fixture test green.
- check_comments clean; comments explain the invariant (seed pass calibrates because cold-start asymmetry is designed; quarantine is terminal-once), never this incident or row counts.
