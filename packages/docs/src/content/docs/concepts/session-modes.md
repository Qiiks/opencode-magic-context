---
title: Session modes
description: The two effective modes Magic Context runs in: primary sessions and subagents.
---

Magic Context runs in two effective modes: **primary sessions** and **subagents**. The core tagging and cleanup plumbing stays on everywhere, while heavier features are reserved for primary sessions. The agent-facing reduce surface (`ctx_reduce`, `§N§` prefixes, and reduce nudges) is gated by the session's actual tool availability: if an agent's tool allow-list denies `ctx_reduce`, Magic Context omits the visible reduce surface for that session.

## The two modes

### Primary session

The full surface. Your agent gets the historian, compartments, memory/search/note guidance, prompt adjuncts, synthetic-todowrite, auto-search hints, and automatic safety valves. When `ctx_reduce` is available in the agent's tool set, it also sees `§N§` tags and receives nudges to reduce spent tool outputs as pressure builds.

This is the recommended mode for most users. The agent actively managing spent tool outputs usually beats fully automatic cleanup because the agent knows which results it has already used.

To hide the reduce surface for a particular agent, remove or deny `ctx_reduce` in that agent's tool allow-list. The rest of Magic Context keeps running: historian compression, heuristic cleanup, compartments, memory, and search still work.

Caveman text compression (`caveman_text_compression.enabled`) is an orthogonal opt-in for primary sessions. It can run whether or not `ctx_reduce` is available, and it only rewrites old user/assistant prose; dropped tags still win.

### Subagent

Subagent sessions (council members, historian, sidekick, dreamer child sessions) get a lightweight pass:

- Tagging and heuristic cleanup run normally
- No historian, no compartment injection, no prompt-adjunct blocks (`<project-docs>`, `<user-profile>`)
- No deferred-note nudges
- Heuristic drops run on **every** execute pass (not once-per-turn like primary sessions — subagents are effectively one parent turn)
- Overflow is handled via the overflow detection path without emergency-recovery state
- No caveman text compression

Subagents are driven by a parent agent, have bounded lifetimes, and often run in parallel. Turning on the full feature set in each subagent would create redundant work and per-agent cache churn.

## Feature comparison

| Feature | Primary session | Subagent |
|---------|:---:|:---:|
| Tag tracking | ✓ | ✓ |
| `§N§` tags in message text | when `ctx_reduce` is available | when `ctx_reduce` is available |
| `ctx_reduce` tool | tool allow-list controlled | tool allow-list controlled |
| Historian and compartments | ✓ | |
| `<session-history>` injection | ✓ | |
| `<project-docs>`, `<user-profile>` | ✓ | |
| Channel 1 nudge (tool-output reminder) | when `ctx_reduce` is available | when `ctx_reduce` is available |
| Channel 2 ceiling nudge | when `ctx_reduce` is available | when `ctx_reduce` is available |
| Deferred-note nudges | ✓ | |
| Synthetic-todowrite injection | ✓ | |
| Auto-search hints | ✓ | |
| Heuristic drops at execute threshold | ✓ | ✓ |
| 85% emergency drop | ✓ | |
| 95% block and recovery | ✓ | |
| Caveman text compression | opt-in | |

## How it connects

Session modes are a lens on the full [context pipeline](/concepts/overview/). Primary sessions get the [context reduction](/concepts/context-reduction/) surface when `ctx_reduce` is available, plus the [historian](/concepts/historian/) and heuristic cleanup. Both modes benefit from cache-safe tagging and the [cache architecture](/concepts/cache-architecture/).
