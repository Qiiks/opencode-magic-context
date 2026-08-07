# FM-OC-3 mutation record

Runner: `bun scripts/run-rust-fm-mutation.ts FM-OC-3`

Mutations were applied to temporary source copies and the contract assertion was run against each copy.

- Rung swap: changed the parked probe's retry cadence conjunction to a disjunction.
- Rung deletion: removed `state.parked = false;`, the successful-pass unpark rung.

Output:

```text
FM_OC_3_RUNG_SWAP: FAIL (distinct contract assertion) — !emergencyFailClosed &&
                passUsageSnapshot.percentage < RUST_PARK_PROBE_PRESSURE_BYPASS_PCT &&
                state.passCount % RUST_PARK_RETRY_INTERVAL !== 0
FM_OC_3_RUNG_DELETION: FAIL (distinct contract assertion) — state.parked = false;
```
