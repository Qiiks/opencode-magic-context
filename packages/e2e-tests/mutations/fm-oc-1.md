# FM-OC-1 mutation record

Runner: `bun scripts/run-rust-fm-mutation.ts FM-OC-1`

Mutations were applied to temporary source copies and the contract assertion was run against each copy.

- Rung swap: `servedFrom = replayed ? "lkg" : "raw";` → `servedFrom = replayed ? "raw" : "lkg";`
- Rung deletion: removed `sessionLog(sessionId, "lkg_replay_served");`

Output:

```text
FM_OC_1_RUNG_SWAP: FAIL (distinct contract assertion) — servedFrom = replayed ? "lkg" : "raw";
FM_OC_1_RUNG_DELETION: FAIL (distinct contract assertion) — sessionLog(sessionId, "lkg_replay_served");
```
