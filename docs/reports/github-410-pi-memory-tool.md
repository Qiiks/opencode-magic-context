# GitHub #410: Pi `ctx_memory` visibility

## Decision

Pi's extension API supports runtime tool activation through `getActiveTools()`
and `setActiveTools(toolNames)`. Magic Context therefore follows the runtime
path: it registers the `ctx_memory` definition once, then synchronizes the
active tool set on every `session_start` from the resolved project config. A
memory-off session removes `ctx_memory`; a memory-on session restores it. The
same handler invalidates the current directory's cached project dependencies
before resolving config, so Pi reloads and the next session observe a
`memory.enabled` flip without a process restart.

Pi can switch directories while an existing session remains active. The tool's
existing call-time `getProjectEmbeddingSnapshot(projectIdentity)` check remains
unchanged as the belt: if a `/cd` or config change leaves a stale enabled tool
until the next session/reload, it still returns the existing memory-disabled
refusal instead of touching storage.

## Prompt behavior and coverage

The Pi system-prompt builder already passes `memoryEnabled !== false` into the
shared guidance builder. Its memory-off output retains `ctx_search` guidance
but removes the `ctx_memory` block and its proactive-save instructions. The
regression tests cover a memory-off active set, restoration to the active set
when memory is re-enabled for the following session/reload, and absence of
`ctx_memory` guidance while memory is off.

## `ctx_note` design note

A future `notes.enabled` project setting should control the model-facing
`ctx_note` tool and its related note behavior as one coherent feature rather
than merely hiding the tool. In addition to registration/activation and system
prompt guidance, it would need to gate note nudges, automatic search of notes,
and m1 note deltas; otherwise the model could still receive note-driven prompts
or injected note changes after being told that notes are disabled. This change
does not add that setting so its scope and user-facing semantics can be decided
separately.
