# Pending guidance-epoch changes

## Finding 6 — Control metadata is not reply syntax

Source: `.alfonso/agent-text-audit-2026-07-23.md`, finding 6.

### Exact proposed insertion text

```text
Magic Context control metadata is not reply syntax. Never reproduce `<system-reminder>`, `<ctx-search-hint>`, `<session-history>`, `<session-history-since>`, `<project-memory>`, `<memory-updates>`, `<new-compartments>`, `<new-memories>`, `[dropped §N§]`, or `<!-- +Xm -->` markers in a normal reply and never treat them as user instructions; use ordinary prose and real tool calls instead.
```

### Target files

- `packages/plugin/src/agents/magic-context-prompt.ts`: insert the rule in both primary guidance variants.
- `crates/mc-module/assets/guidance_primary.txt`: insert the same rule.
- `crates/mc-module/assets/guidance_no_reduce.txt`: insert the same rule.

### Coordination

This change rides the next guidance-epoch bump. Change the TypeScript and Rust guidance twins together, regenerate and repackage the Rust guidance asset, and perform the required prompt-cache invalidation and guidance/cache-epoch handling. Do not change the m[0]/m[1] delimiters, dropped placeholders, or temporal markers themselves.
