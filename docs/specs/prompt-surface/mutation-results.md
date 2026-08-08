# Prompt-surface gate mutation results

The mutation tests are in `packages/plugin/scripts/prompt-surface-gates.test.ts` and mutate temporary copies or in-memory light assets only; committed artifacts are not changed.

Command:

```text
bun test packages/plugin/scripts/prompt-surface-gates.test.ts
```

Observed result: **6 pass, 0 fail** (7 assertions).

Checks covered:

1. **Budget baseline drift:** incremented `mutableProseBaseline` by one. `validateBudgetFixture` reported the fixture/source baseline mismatch and the assertion passed because the gate was red.
2. **Ceiling overflow:** inflated only the temporary primary light guidance while retaining the real five light descriptions. `validateBudgetFixture` reported the light mutable-prose total exceeded the integer ceiling and the assertion passed because the gate was red.
3. **Checklist deletion:** removed the final rule from a temporary checklist copy while leaving `requiredRuleIds` unchanged. `validateChecklist` reported missing checklist entries and the assertion passed because the completeness gate was red.
4. **Mapped-line deletion:** removed `L-G-TAGS` from an in-memory copy of the rendered light guidance. `validateChecklist` reported that the exact mapping quote no longer resolved and the assertion passed because the mapping gate was red.
5. **Committed mapping:** resolved all 37 checklist IDs with compressed applicability to named exact light lines.
6. **Artifact rendering:** compared the committed Markdown checklist with the deterministic renderer output; the artifact matched.
