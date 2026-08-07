# FM-OC-5 mutation record

Runner: `bun scripts/run-rust-fm-mutation.ts FM-OC-5`

Mutations were applied to temporary source copies and the contract assertion was run against each copy.

- Rung swap: changed the fault action from `stopModule()` to `continueModule()` before the outage pass.
- Rung deletion: removed the lineage-scoped `assertLoudModuleFailure` assertion.

Output:

```text
FM_OC_5_RUNG_SWAP: FAIL (distinct contract assertion) — h.subc.stopModule();
            await h.sendPrompt
FM_OC_5_RUNG_DELETION: FAIL (distinct contract assertion) — assertLoudModuleFailure(h, sessionId);
```
