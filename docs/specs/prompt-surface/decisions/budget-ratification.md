# Budget policy ratification

- **recordId:** `prompt-surface-budget-r1`
- **artifactId:** `prompt-surface-budget`
- **artifactRevisionOrDigest:** `sha256:77082b5ff1546f45160b08e896e6c1f434654ba4f52b0806bd20b0b24cd67213`
- **decision:** `PENDING-RATIFICATION — proposed ACCEPT for floor(0.50 × mutable-prose baseline)`
- **authorizedDecisionOwner:** `Ufuk`
- **ratificationTimestamp:** `PENDING-RATIFICATION`
- **scope:** `The primary built-in full guidance variant primary-full-reduce-memory-on and its active ctx_* descriptions; serialized parameter schemas are reported separately; adjuncts and USER overrides are excluded.`
- **status:** `PENDING-RATIFICATION`

## Proposed policy and evidence

- **Tokenizer package:** `ai-tokenizer`
- **Encoding:** `claude`
- **Version:** `1.0.6`
- **Counting method:** `new Tokenizer(claudeEncoding).count(rawText)`
- **Primary feature call:** `buildMagicContextSection(null, 20, true, true, true, false, false, undefined, true)`
- **Active tools:** `ctx_reduce`, `ctx_expand`, `ctx_note`, `ctx_memory`, `ctx_search`
- **Mutable-prose baseline:** `3650` tokens (`2003` guidance + `318 + 395 + 391 + 234 + 309` descriptions)
- **Integer light ceiling:** `1825` tokens (`floor(0.50 × 3650)`)
- **Fixture:** `docs/specs/prompt-surface/budget-fixture.json`
- **Deterministic gate:** `bun packages/plugin/scripts/measure-agent-surface.ts --assert`

The fixture recomputes the baseline from the current built-in source. A source mismatch is a gate failure; it cannot be repaired by silently changing the measured value. Light prose is not yet present, so the gate reports no light counts until a ratified S3 light manifest is supplied.
