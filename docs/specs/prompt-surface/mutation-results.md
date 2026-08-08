# Prompt-surface gate mutation results

The mutation tests are in `packages/plugin/scripts/prompt-surface-gates.test.ts` and mutate temporary copies only; committed artifacts are not changed.

Command:

```text
bun test packages/plugin/scripts/prompt-surface-gates.test.ts
```

Observed result: **4 pass, 0 fail** (4 assertions).

Mutations covered:

1. **Budget baseline drift:** incremented `mutableProseBaseline` by one. `validateBudgetFixture` reported the fixture/source baseline mismatch and the assertion passed because the gate was red.
2. **Ceiling overflow:** supplied a temporary primary light manifest with oversized guidance and descriptions. `validateBudgetFixture` reported the light mutable-prose total exceeded the integer ceiling and the assertion passed because the gate was red.
3. **Checklist deletion:** removed the final rule from a temporary checklist copy while leaving `requiredRuleIds` unchanged. `validateChecklist` reported missing checklist entries and the assertion passed because the completeness gate was red.
4. **Artifact rendering:** compared the committed Markdown checklist with the deterministic renderer output; the artifact matched.

The temporary light manifest is not a light prose artifact. S3 remains responsible for authoring light only after the pending Ufuk ratifications.
