# FM-OC-4 mutation record

Runner: `bun scripts/run-rust-fm-mutation.ts FM-OC-4`

Mutations were applied to temporary source copies and the contract assertion was run against each copy.

- Rung swap: inverted the emergency-fail-closed branch.
- Rung deletion: removed the `mc_rust_emergency_refusal before_lkg` diagnostic.

Output:

```text
FM_OC_4_RUNG_SWAP: FAIL (distinct contract assertion) — if (emergencyFailClosed) {
FM_OC_4_RUNG_DELETION: FAIL (distinct contract assertion) — mc_rust_emergency_refusal before_lkg
```
