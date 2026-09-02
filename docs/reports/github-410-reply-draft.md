# Reply draft for GitHub issue #410

Thanks for the report — the diagnosis was correct. Pi had registered
`ctx_memory` unconditionally and relied only on its call-time refusal, so a
memory-off project exposed a tool the model could not successfully use.

Pi supports changing its active tool set at runtime, so the main extension now
registers the definition once and enables or removes `ctx_memory` at each
session start from the resolved project config. That includes Pi reloads, and
the project-config cache is refreshed first, so changing `memory.enabled` takes
effect in the next session without restarting Pi. The existing call-time refusal
remains as a safety belt for a stale active tool during an in-session directory
or config change. The Pi system prompt continues to remove `ctx_memory`
guidance whenever memory is disabled.

`ctx_note` is a sensible follow-up, but it needs a dedicated `notes.enabled`
design rather than a tool-only switch: notes also drive note nudges, automatic
note search, and m1 note deltas. We have kept that broader contract out of this
fix so it can be specified coherently.