# FM-OC-2 mutation record

Runner: `bun scripts/run-rust-fm-mutation.ts FM-OC-2`

Mutations were applied to temporary source copies and the contract assertion was run against each copy.

- Rung swap: changed the parked probe guard from `|| state.parked` to `&& state.parked`.
- Rung deletion: removed the `mc_rust_park_transition` log emission.

Output:

```text
FM_OC_2_RUNG_SWAP: FAIL (distinct contract assertion) — if (state.consecutiveFailures < RUST_FAILURE_PARK_THRESHOLD || state.parked) return;
FM_OC_2_RUNG_DELETION: FAIL (distinct contract assertion) — mc_rust_park_transition
```
