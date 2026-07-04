---
title: Overview
description: The mental model for how Magic Context manages your session's context, memory, and recall across hours, days, and weeks.
---

Magic Context runs a pipeline on every turn that keeps your session's context small, your knowledge durable, and the right information available at the right moment. This page is the one-minute tour of how the pieces fit together.

## The pipeline at a glance

| Stage | What happens | Deep dive |
|-------|-------------|-----------|
| **Tagging** | Every message, file, and tool output gets a `§N§` tag so the system can track and manage it. | [Context reduction](/concepts/context-reduction/) |
| **Agent-driven reduction** | Your agent calls `ctx_reduce` to drop spent tool outputs and stale messages. Drops are queued and applied at cache-safe moments. | [Context reduction](/concepts/context-reduction/) |
| **Background condensation** | A historian agent compresses older conversation into tiered compartments — chronological summaries with importance scores. | [Historian](/concepts/historian/) |
| **Durable knowledge** | The historian promotes durable facts (decisions, constraints, conventions) into project memory that persists across sessions. | [Memory](/concepts/memory/) |
| **Recall** | Active memories and compartment history inject automatically every turn. On demand, `ctx_search` and `ctx_expand` retrieve deeper. | [Memory](/concepts/memory/) |
| **Off-hours maintenance** | A dreamer agent runs overnight to consolidate duplicates, verify memories against code, and maintain docs. | [Dreamer](/concepts/dreamer/) |

```text
┌─────────────────────────────────────────────────────────────┐
│  Your session                                               │
│                                                             │
│  ┌──────────┐   ┌──────────────┐   ┌────────────────────┐  │
│  │ Tagging  │──▶│ ctx_reduce   │──▶│ Historian          │  │
│  │ (§N§)    │   │ (agent drops)│   │ (compartments)     │  │
│  └──────────┘   └──────────────┘   └────────┬───────────┘  │
│                                              │              │
│                                    ┌─────────▼──────────┐   │
│                                    │ Project memory     │   │
│                                    │ (durable facts)    │   │
│                                    └─────────┬──────────┘   │
│                                              │              │
│  ┌──────────┐   ┌──────────────┐   ┌────────▼───────────┐  │
│  │ Dreamer  │◀──│ ctx_search   │◀──│ Recall (auto +     │  │
│  │ (nightly)│   │ ctx_expand   │   │  on-demand)        │  │
│  └──────────┘   └──────────────┘   └────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## Why it matters

Without Magic Context, a coding agent hits a wall: the context window fills up, the host triggers compaction (a full stop to re-read everything), and durable knowledge evaporates at session end. Magic Context replaces that with a continuous background pipeline — no pauses, no forgetting.

The historian keeps the live window small by compressing old history into [tiered compartments](/concepts/historian/) that render at the right fidelity for the moment. [Project memory](/concepts/memory/) captures the knowledge worth keeping forever. The [dreamer](/concepts/dreamer/) maintains quality overnight. And the whole thing is structured so [background work never invalidates your prompt cache](/concepts/cache-architecture/).

## Two promises

1. **Your agent never stops to manage its context.** No compaction pauses, no broken flow.
2. **Your agent never forgets.** Memories persist across sessions and across harnesses — write one in OpenCode, retrieve it in Pi.

## How the modes differ

Magic Context runs in [two effective modes](/concepts/session-modes/): primary sessions and subagents. Primary sessions get the full historian, memory, and prompt surface; the visible reduce surface appears only when `ctx_reduce` is available in the agent's tool allow-list. Subagents get a lightweight pass.

## Where to go next

- [Historian](/concepts/historian/) — how compartments work, when the historian fires, what you see
- [Memory](/concepts/memory/) — the 5 categories, how memories are written and recalled
- [Dreamer](/concepts/dreamer/) — overnight maintenance tasks and scheduling
- [Context reduction](/concepts/context-reduction/) — tagging, `ctx_reduce`, nudges, and safety nets
- [Cache architecture](/concepts/cache-architecture/) — why the layout preserves prompt caching
- [Session modes](/concepts/session-modes/) — the three modes and when to choose each
