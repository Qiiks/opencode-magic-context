# Latch permanence audit

Scope: production TypeScript under `packages/plugin/src/` and `packages/pi-plugin/src/`; `crates/`, `*.test.ts`, and the already-remediated logger directory memo were excluded. **32 verdict-shaped slots were examined** (the committed guard carries their classifications).

## DEFECTS

Ranked with silent outcomes first.

1. **SILENCE — `packages/plugin/src/features/magic-context/memory/embedding-synapse.ts:356, 473, 787` — `permanentFailure`.** A catalog/response error classified as `artifact_invalid`, `not_certified`, `schema_violation`, etc. sets the per-provider flag. Later `initialize()` returns `false`, and embedding calls return `null` without a fresh daemon probe. A Synapse operator can certify or repair a lane, restart it with valid metadata, or correct a transient bad response while this plugin process remains alive. The embedding registry retains the provider while its configuration identity is unchanged, so the user sees semantic indexing/search quietly stop until configuration changes or the plugin restarts.

2. **SILENCE — `packages/plugin/src/features/magic-context/memory/embedding-synapse.ts:315, 335-344` — `sharedClientPromise`.** A failed `SubcClient.connect()` promise is retained in the shared slot. If the daemon starts or its connection file returns later, every later provider receives the same rejected promise instead of reconnecting. Callers log unavailability and return empty/null embeddings, so background embedding does not recover.

3. **SILENCE — `packages/plugin/src/features/magic-context/smart-notes/sandbox-runner.ts:35-45` — `asyncModulePromise`.** The first dynamic import/WASM-module construction promise is memoized without rejection cleanup. A temporary module-resolution or WASM initialization failure can clear after a package/cache repair, but every subsequent smart-note check receives the rejected promise. Checks are returned as failures rather than retrying module initialization.

4. **DEGRADED SILENT BEHAVIOUR — `packages/plugin/src/hooks/magic-context/read-session-formatting.ts:131, 241-294` — `tokenizerLoadAttempted`.** Any first tokenizer-load or encode failure permanently selects heuristic token estimates. A package becoming available or a transient module-load failure clearing will not be re-probed. The process logs one warning, then continues with less accurate token budgets/boundaries until restart.

5. **LOUD LOG ONLY — `packages/plugin/src/features/magic-context/compaction-marker.ts:108, 118-157` — `cachedSchemaCompatible`.** A thrown schema probe (for example a transient SQLite lock/I/O failure) is cached as `compatible: false`, disabling marker injection until `closeCompactionMarkerDb()` or restart. The condition can clear when the lock is released. This is logged as disabled, but the marker feature thereafter does nothing.

## CLEAR — correct latch

- `packages/plugin/src/features/magic-context/memory/embedding-local.ts:307, 450-454` — `nativeRuntimeMissing`: missing/unloadable native binding requires repairing the installed package, which this process cannot observe.
- `packages/plugin/src/hooks/magic-context/ctx-reduce-availability.ts:57, 59-67` — `ctxReduceRegisteredGlobally`: tool registration is decided once at plugin boot and is process-stable.
- `packages/plugin/src/hooks/magic-context/ctx-reduce-availability.ts:91, 126-158` — `availabilityBySession`: the first persisted user message defines that session's fixed tool surface; later messages cannot repair/change it by contract.
- `packages/plugin/src/features/magic-context/mural/storage-mural-cues.ts:31-54`, `packages/plugin/src/features/magic-context/memory/storage-memory.ts:107-168`, and `packages/plugin/src/features/magic-context/storage-meta-session.ts:60-76` — per-`Database` column/projection probes: these handles complete migrations before serving these reads, so the relevant schema cannot change within their use scope.
- `packages/plugin/src/plugin/conflict-warning-hook.ts:104-113` — Desktop-state absence is memoized only for one-shot plugin-start warning/cleanup decisions; those deciding paths are invoked once per boot, so observing a later state-file update is outside their scope.
- `packages/plugin/src/features/magic-context/memory/embedding-openai.ts:387-458` — circuit-breaker verdict: the open period expires and a half-open probe re-derives endpoint health; success resets the state.

## CLEAR — saved by an invalidation or retry

- `packages/plugin/src/hooks/magic-context/module-transport.ts:183-218` capability cache — invalidated at `packages/plugin/src/hooks/magic-context/module-transport.ts:825-833` on connection replacement and at `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:2020-2023` on `need_full_sync` before the next probe.
- `packages/plugin/src/features/magic-context/memory/project-identity.ts:36, 427-461` directory fallback — a new `.git` directory is checked on every use and deletes the fallback at `:431-439`; transient git cooldown expires/deletes at `:337-343`.
- `packages/plugin/src/plugin/embedding-routing.ts:30-33, 112-127` Synapse probe — successful and failed probe promises both expire after 60 seconds and are re-derived.
- `packages/pi-plugin/src/dreamer/pi-session-api.ts:47-84` module promise — rejection handler clears the exact failed promise at `:80-83`.
- `packages/plugin/src/shared/models-dev-cache.ts:381-408` after-auth warm latch — failed refresh resets `authRewarmDone` at `:406-407`.
- `packages/plugin/src/features/magic-context/message-index.ts:48-49, 668-705` unavailable source sweep — persisted cooldown schedules a reprobe after one day rather than a permanent absence.

## Classification exclusions

Repeatedly assigned `last_*` diagnostics (including `lastHookInitFailure`, `lastSchemaFenceRejection`, and `lastMigrationOnOpenRefusal`) are most-recent reports, not latches. Publication slots such as prepared statements, database/client handles, in-flight promises that clear on settlement, and last-known-good metadata are handles/data rather than failure verdicts. They were not promoted to defects.

## Status-surface staleness

The schema-fence diagnostics expose a current open-attempt result because `openDatabase()` clears or overwrites them. The one-shot Desktop startup target cache has no timestamp, but it is not a health verdict and its sole consumer is boot-time notification routing. No additional sound latch exposes an undated live-health status.

## Guard coverage and test-code exclusion

`packages/plugin/src/config/latch-permanence-guard.test.ts` scans both source roots, excludes only `*.test.ts`, and fails when a new mutable, verdict-shaped one-shot slot has no inline classification and reason. It also asserts that production source contains no `from "bun:test"` inline test blocks; the guard run found none, so basename exclusion does not discard shipped inline tests. This avoids marker-based test stripping entirely.

Non-vacuity was demonstrated with a temporary `auditUnclassifiedFailure` slot in `read-session-formatting.ts`: the guard failed and named `packages/plugin/src/hooks/magic-context/read-session-formatting.ts:auditUnclassifiedFailure`; after removal it passed.
