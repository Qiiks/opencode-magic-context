# FM-OC-6 mutation record

Runner: `bun scripts/run-rust-fm-mutation.ts FM-OC-6`

Mutations were applied to temporary source copies and the contract assertion was run against each copy.

- Rung swap: reversed the refusal-before-LKG ordering assertion.
- Rung deletion: removed the `mc_rust_emergency_refusal before_lkg` token assertion.

Output:

```text
FM_OC_6_RUNG_SWAP: FAIL (distinct contract assertion) — expect(after[refusalIndex]).toContain("before_lkg")
FM_OC_6_RUNG_DELETION: FAIL (distinct contract assertion) — line.includes("mc_rust_emergency_refusal before_lkg")
```
