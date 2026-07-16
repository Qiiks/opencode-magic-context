# Synapse embedding lane — MC as first consumer

Status: v5 — BUILD-READY. Round-3 re-gate (bg_bde18fcb) returned REVISE with 4
mechanical spec corrections, folded below as amendments; core isolation (F3) and
query-capture (F4) designs confirmed sound. Build queues behind the U1 prod canary.

## Round-3 amendments (normative, override anything contradicting below)

A1 (F1/F2 reconciliation): routing fields live in a RAW config type only.
`embedding.provider` / `embedding.fallback_provider` are parsed into a
`ResolvedEmbeddingRouting` at bootstrap; the registry's `EmbeddingConfig` (hashed
wholesale by getRuntimeFingerprint) receives ONLY the resolved lane's
provider-specific fields, exactly as today. fallback_provider enum value is
"openai-compatible" (the existing provider name), not "openai"; valid set =
"local" | "openai-compatible" | "off".

A2 (F5): mixed-provenance migration handling is REMOVED as unreachable — under
strict allow_equivalent:false a lane never accumulates mixed provenance, so
aliases.check_index is NOT called in v1 (deferred with canonicalization to the
post-v1 equivalence decision). "canonical_model_id" is replaced by the exact
models.list field: the catalog entry's `model` id string as served. Stale-GC
interaction: with no migration minting, the only synapse ids are the active
primary/shadow ids, both GC-protected while armed (F3); no retirement class
exists in v1.

A3 (F6): the provider interface gains a DISTINCT method `embedItems(items:
{id, text, contentSha256}[]) => Promise<Map<string, Float32Array>>` implemented
by the Synapse provider; the existing positional `embed()` stays untouched for
local/openai providers. The three domain persistence paths (memory, commit,
chunk) migrate to id-keyed writes when the resolved provider exposes embedItems,
with a thin positional adapter for providers that don't. No ambiguous overload.

A4 (F7 gate pinned as a decision rule): overlap@10 per query; failures and
timeouts SCORE ZERO (not excluded); queries deduplicated by normalized text hash
within ONE (fingerprint, epoch) cohort; aggregate = P10 (10th percentile, not
bottom-decile mean). PASS = P10 ≥ 0.6 over ≥200 deduplicated queries AND
synapse-lane p95 query latency ≤ 1.5× fallback-lane p95 AND failure rate < 1%.
≥200 is a pilot floor (≈20 tail observations): the cutover decision reports the
P10 with a bootstrap 90% CI, and Ufuk sees the number + CI, not a bare
pass/fail. Small corpora (<200 queries collected) = gate not evaluable, phase 1
continues.

Original round-2 resolutions follow (F1-F7, as amended above). Key deltas over v3: fallback_provider explicit; synapse_shadow moved to a
top-level dev block; NO equivalent_to canonicalization in v1 (exact synapse:v1 identity);
domain-item batch API (id-keyed, not positional); shadow writes via a bounded
concurrency-1 backlog worker; query contract returns {vector, modelId, chunkModelId,
generation} as one captured unit; phase-1 records the rank-overlap measurement corpus.
Owner: MC. Consumers: packages/plugin (OpenCode) + packages/pi-plugin (Pi) via shared
config.

## Oracle round-2 resolutions (normative)

F1 ROUTING MATRIX (complete, no implicit block-presence routing anywhere):
- `embedding.provider: "synapse"` requires BOTH a `subc` block AND
  `embedding.fallback_provider` ("local" | "openai" | "off") with the existing
  model/endpoint fields retained for the fallback lane. Missing either → load
  warning + resolve the fallback lane (or schema default if fallback itself is
  malformed). The old "subc presence routes" language is DEAD; `subc` is transport
  config only.
- `synapse_shadow` moves OUT of EmbeddingConfig to a TOP-LEVEL dev block
  `shadow_embedding: { enabled: bool }` (same class as shadow_transform; in
  DEV_ONLY_KEYS). Matrix: synapse-provider + shadow → warn, shadow ignored
  (mirroring the primary lane into itself is meaningless); shadow without subc
  block or with provider "off" → warn + no-op; shadow with local/openai primary +
  subc present → armed.
- Project tier strips ALL THREE: `subc`, `shadow_embedding`, `embedding.fallback_provider`
  (with `embedding.provider` value-checked: project tier may not set "synapse").
  All three join the untrusted-load latch field set.

F2 IDENTITY-HASH SAFETY: no routing flag lives inside EmbeddingConfig
(getRuntimeFingerprint hashes the whole normalized config). Provider switches in
resolution code become EXHAUSTIVE (a non-exhaustive branch must fail loud, never
interpret "synapse" as local/off). Golden tests pin byte-identical
providerIdentity/runtimeFingerprint for existing local/openai/off configs before
and after the schema change.

F3 SHADOW ISOLATION (registry + resource bounds): registerProjectEmbedding is
upsert-per-(project, scope, model_id) but the PRIMARY registration is singular —
shadow NEVER calls it. Shadow gets its own descriptor + generation
(shadow_embedding_registrations or a scope discriminator) and a process-wide
CONCURRENCY-1 backlog worker: primary commit enqueues (project, scope, item ids)
O(1); the worker re-reads payloads at dequeue, caps items/page-bytes/time per
tick, yields between pages, never holds a SQLite transaction across I/O
(hydration-storm lesson). Stale-GC protects BOTH primary and shadow ids while the
flag is armed.

F4 QUERY CONTRACT: embedTextForProject's return widens to {vector, modelId,
chunkModelId, generation} captured ATOMICALLY at embed time; every search wrapper
(auto-search-runner, auto-search-pi, ctx-search tools) threads the captured ids
into unifiedSearch — no later registry snapshot. Compartment search uses the
captured chunkModelId (distinct derived id; threading modelId alone is
insufficient). Generation validated at use.

F5 IDENTITY v1 (no canonicalization): equivalent_to canonicalization is REMOVED
from v1 (it contradicted the separate shadow row space + allow_equivalent:false).
Lane identity = `synapse:v1:<hash(canonical_model_id, fingerprint)>` exactly;
module_generation and table_epoch are EXCLUDED from the id. Persist alongside the
registration: fingerprint (verbatim), table_epoch, dims, provenance JSON. Every
request sends required_fingerprint + required_epoch + allow_equivalent:false +
accept_declared:false. Dims from models.list validated against response dims.
The registration descriptor carries max_input_tokens=8192 BEFORE chunkModelId is
computed (chunk-window folds into chunk identity). aliases.check_index runs at
write-commit when provenance is mixed; migration_required mints a future-only id,
existing rows stay readable (revocation forward-only). Canonicalization onto the
bundled-MiniLM identity is EXPLICITLY deferred to a post-v1 decision.

F6 BATCH API (id-keyed, durable-job ready): EmbeddingProvider gains a
domain-item form: items = [{id, text, contentSha256}] with ids
`memory:<id>` / `commit:<sha>` / `chunk:<compartment>:<window>`; results are
Map<id, vector> — positional arrays are dead on this lane. The batch ledger
persists {manifest (ordered ids + content hashes), request_key, job_id, cursor}
per submission; request_key derives from every server-bound digest field (op,
model id, constraints, ordered item ids, content hashes) so a legitimate retry is
same-key-same-digest by construction and idempotency_conflict is invariant-fatal
(fail loud, never remint). Pages stage by item id; compartment window-groups
commit atomically only when complete. module_restarted → resubmit same
key+digest, resume committed pages. Dependency pinned EXACTLY 0.4.1.

F7 TTL/PROBE + MEASUREMENT OPERABILITY: probes are keyed+coalesced by
(normalized connection file, model), guarded by desired-config + availability
generations; Pi's bootstrap early-return on unchanged config fingerprint gets a
TTL check BEFORE the early return. Phase-1 measurement substrate (without it the
phase-2 gate is unmeasurable): after each authoritative search, enqueue a bounded
offline evaluation that computes rankings in BOTH row spaces over the same corpus
and stores {stable top-K ids + scores per space, model fingerprints + epochs,
filters, corpus hash, coverage, failures, latency}. Gate definition pinned: K=10,
overlap = |topK_a ∩ topK_b| / K per query, aggregate = worst decile over ≥200
real queries, missing vectors/timeouts COUNT AS FAILURES (never excluded),
retention 30 days, query text stored hashed + local-only.

## Goal and rollout phases

End state: `embedding.provider: "synapse"` routes ALL MC embedding traffic
(search-time query embeds, publish-time chunk embeds, /ctx-embed drains, dreamer
trickles, memory/commit embeds) through the subc daemon's Synapse module, target
model gte-modernbert-base f16 (8192-token context — covers our chunk sizing that
MiniLM truncates at 512; one-time re-embed accepted).

PHASE 1 — SHADOW (ships first, dev-flag gated): the current provider stays
authoritative for ALL reads and writes. Additionally, the background embed paths
(chunk publish, /ctx-embed drain, memory/commit writes — NEVER the search hot
path) dual-write the same content through Synapse into a SEPARATE model-id row
space (v49 coexistence native). Divergence detector armed; measurement collected.
Query-time stays on the current leg for the whole phase.

PHASE 2 — CUTOVER (separate later flip, needs Ufuk + the acceptance gate):
`provider: "synapse"` becomes the resolved lane for query + write. ACCEPTANCE
GATE: Synapse's worst-decile rank-overlap measured on MC's REAL queries against
both row spaces (not eval-corpus NDCG alone), per the D-005 contract.

Both phases: without the gate config, behavior is byte-for-byte today's. The
`embedding.*` sub-fields for the current provider are never rewritten; they
remain the fallback lane definition.

## Config gate (AFT convention)

```jsonc
// ~/.config/cortexkit/magic-context.jsonc (user tier ONLY)
{
  // PHASE 2 end state: explicit provider enum beside "openai" | "local".
  "embedding": { "provider": "synapse" },
  // PHASE 1 shadow (dev-flag class, like shadow_transform; default false;
  // excluded from public config docs via DEV_ONLY_KEYS):
  // "embedding": { "provider": "local", "synapse_shadow": true },
  "subc": { "connection_file": "~/.local/share/cortexkit/run/subc-connection.json" }
}
```

- `provider: "synapse"` WITHOUT a `subc` block is a config error: warn at load and
  fall back to the schema default provider resolution (never silently guess a
  socket path). The enum is user-tier by the existing rule (project tier strips
  `embedding` destinations already).
- `synapse_shadow: true` is valid with any current provider; it arms the phase-1
  dual-write only when the subc block is present and the lane is READY (three-state
  below). Shadow failures NEVER fail the primary write (fail-open mirror, same
  doctrine as shadow_transform).
- Naming: the enum is "synapse" (the capability module), not "subc" (the
  transport) — a future module served over the same daemon must not overload it.

- Zod: optional `subc: { connection_file: string }` top-level block. `~` expansion via
  the existing path helpers. No other fields v1. `subc` must NOT live inside
  `EmbeddingConfig` (registry fingerprints hash the embedding config; the gate must not
  perturb existing identity hashes).
- SECURITY: `subc` is stripped from PROJECT-tier configs in project-security.ts
  (same class as `sqlite.*`) — a cloned repo must not point the plugin at an arbitrary
  socket. Both loaders call stripUnsafeProjectConfigFields before merge, so top-level
  stripping covers OpenCode and Pi. Test required.
- UNTRUSTED-LOAD LATCH: `subc`/`subc.connection_file` must be added to the
  embedding-affecting field set in embedding-bootstrap-helpers.ts — a malformed subc
  block recovering to defaults must mark the load untrusted, or a schema-recovery pass
  could silently register the fallback lane and later GC Synapse vectors.
- Pi parity free via shared config file; the Pi plugin reads the same block.

## Transport: @cortexkit/subc-client@^0.4.1 (blessed, published, wire v2)

Pin the npm package (0.4.1+ carries the wire-v2 21-byte header + endpoint-side
epoch validation + the 1 MiB frame-cap and idle-read-timeout fixes proven on the
shadow-transform lane). Do NOT vendor or copy AFT's attach code. The lib provides
connection-file read + HMAC handshake, catalog.list, routeOpen with managed route
cache, call() with NotSent/OutcomeUnknown classification, subscribe(), managed
reconnect (auth-transient retry, GOODBYE eviction, unknown_channel evict-reopen-retry,
30s route.open budget), and env-derived consumer identity.

- BindIdentity: harness = "opencode" | "pi" (the actual host), project_root = resolved
  project path, session = the MC session id. Coherent-caller pin from SUBC.
- Consumer identity (SUBC source-exact, client.ts): DO NOT send `consumerIdentity` —
  a harness-plugin process has neither SUBC_MODULE_ID nor SUBC_LAUNCH_NONCE, so
  route.open goes out without the field and the daemon stamps the bind Principal as
  DIRECT (first-party, HMAC key-possession-authenticated). That is the designed state
  for plugin consumers (same as AFT's plugin lane), not a degraded one. NEVER fabricate
  a module_id/nonce pair (daemon validates nonces against its spawn table and rejects).
  Construct shape: `new SubcClient({connectionFile})` → authenticate →
  `routeOpen(target, {harness, projectRoot, session})` with no consumerIdentity key.
- Route lifecycle: let the lib's cache+retry manage; do not hand-roll idle teardown.
- One client per process (module-global, lazy, like the exit-abort registry pattern);
  connection_file changes require restart (documented, not hot-reloaded).

## Synapse ops used (wire-contract-v1.md, synapse repo 2d4aed6)

- `embed.query` — search hot path. Single text, `deadline_ms` (use the existing
  search-time budget), typed rejection. Envelope (fingerprint, table_epoch, provenance,
  equivalent_to, module_generation) rides the response.
- `embed.batch` — publish/drain/trickle paths. Send full chunk sets; respect
  `recommended_batch` advisory from `models.list` when present; admission is the
  enforcement. The per-endpoint batch-size knob is NOT ported to this lane.
- `models.list` — lane discovery + advisory. `aliases.check_index` where the client
  needs alias resolution.
- v1.1 batch semantics adopted: over-budget `embed.batch` returns `{job_id}`;
  poll `embed.result` with cursor paging and KEY WRITES BY ITEM ID (page order is
  length-sorted, never positional). Send `request_key` on every batch so
  resubmission after crash/restart resumes committed pages idempotently
  (`idempotency_conflict` on digest drift = bug, fail loud). `module_generation`
  change mid-conversation ⇒ re-poll jobs (prior-generation jobs are terminal
  `module_restarted`).
- `accept_declared` stays FALSE (default): v1 targets the local ort lane
  (gte-modernbert f16 Metal); MC does not accept declared-assurance remote
  identities without a separate decision.
- NOT used v1: rerank.score, microllm.oneshot (separate A/B lane later).

## Three-state availability (never two)

Resolved ONLY in the bootstrap/registration seam (ensureProjectRegisteredFromOpenCodeDirectory
after trusted config load, before registerProjectEmbedding; mirrored in Pi's bootstrap) —
NEVER inside a provider call. Cached in-process with a short TTL re-check (e.g. 60s),
and the TTL probe is PROMISE-COALESCED per process (concurrent sessions share one
in-flight probe, no stampede). Lane changes re-register the project (existing
generation guards make in-flight embeds against the old registration reject cleanly):

1. ABSENT — no `subc` block, or connection file missing/unreadable, or daemon
   unreachable → fallback lane. Transient-typed; re-check on TTL.
2. PRESENT-UNCERTIFIED — daemon reachable but Synapse returns `probe_required` /
   `not_certified` for the lane's model → fallback lane FOR NOW; optionally invoke
   `probe.start` once per process (fire-and-forget) so the lane self-heals.
3. READY — certified → Synapse lane.

Embedding must NEVER hard-fail because the daemon is down (standalone degradation is
a founding constraint). Fallback = the user's `embedding.*` lane exactly as today.

NO PROVIDER-INTERNAL FALLBACK: the Synapse provider never calls the fallback provider
from inside embed() — vectors produced by a different lane must never be saved or
searched under the Synapse registration's modelId. On mid-session daemon death the
provider returns null (search degrades to lexical for that query); the next TTL
re-check re-registers the project onto the fallback lane.

QUERY/MODEL PINNING (API change, both harnesses): embedTextForProject already returns
{ vector, modelId, generation } but the OpenCode/Pi wrappers discard modelId
(auto-search-runner.ts, auto-search-pi.ts, tools/ctx-search.ts). Thread { vector,
modelId } through embedQuery into unifiedSearch so a query vector is only compared
against vectors of the model that produced it, even across a concurrent lane flip.

## Error typing (maps onto the existing resilient pipeline)

- Transient (retry path, existing backoff/circuit-break): `queue_full`,
  `model_loading` (honor `retry_after_ms`), transport NotSent, timeouts.
- EVERY subc call (embed.query, embed.batch, models.list, probe.start) carries an
  explicit timeoutMs (the client lib exposes timeoutMs, not AbortSignal): search-path
  calls get the existing small search budget; batch calls get a warm-path budget.
- OutcomeUnknown: SAFE TO RETRY for embeddings — embeds are idempotent and re-embedding
  is harmless; this is the one place at-most-once discipline deliberately relaxes
  (state this in code comments; SUBC pin).
- Permanent (fall back + warn once): `artifact_invalid`, `substitution_rejected`,
  `not_certified` (until a probe fixes it), schema violations.
- Timeouts: per-call `timeoutMs` sized to the WARM path (client lib default 30s is not
  the contract; embed batches pass an explicit generous budget; cold model-load is
  Synapse's job-shaped load op, not our timeout problem).

## Vector-space identity (v49 coexistence — no wipe class)

- IDENTITY IS ESTABLISHED BEFORE REGISTRATION, never derived from embed responses:
  the registry computes providerIdentity/runtimeFingerprint/chunkModelId at
  registration time and every save path uses the registration's captured modelId, so
  the canonical identity comes from `models.list` + alias certification during the
  availability check. Response envelopes are VALIDATION ONLY (reject on mismatch).
- The lane's model identity = a `synapse:` scheme id from (model, fingerprint) as
  served by models.list — registered exactly like any provider identity. Old vectors
  coexist; lazy GC; `/ctx-embed` re-drains.
- PHASE-1 SHADOW ROW SPACE: shadow writes register the `synapse:` identity as an
  ADDITIONAL identity (never touching the primary registration); vectors land under
  the synapse modelId rows only. Unified search ignores them until phase 2 (query
  pinning already guarantees this: query vectors carry the primary lane's modelId).
  The rank-overlap measurement reads both spaces offline, not through live search.
- `equivalent_to` CANONICALIZES AT REGISTRATION: if the alias table certifies the
  served model as same-space with an existing identity (e.g. bundled MiniLM), register
  under the EXISTING identity — not a parallel `synapse:` id — so cache keys
  (projectPath, modelId) stay stable and stale-GC never sees a phantom lane flip.
- STRICT substitution rejection: if the envelope fingerprint of a response does not
  match the lane's registered fingerprint AND is not related via `equivalent_to`
  (alias table), REJECT the vectors (do not save) and warn — same class as the
  existing embeddingModelsMatch guard.
- `content_sha256` echo per batch item = DIVERGENCE DETECTOR ONLY: mismatch vs our own
  hash → reject vector + log loud (chunker/tokenizer drift signal), never adopt the
  provider hash as key.
- `equivalent_to` may certify same-space with the bundled local MiniLM identity where
  Synapse serves the same model — when present, map to the EXISTING identity so
  existing vectors stay live (no forced re-embed on lane adoption).

## Where it lives

- New: `packages/plugin/src/features/magic-context/memory/embedding-synapse.ts`
  (provider implementing the same embed interface as embedding-openai/embedding-local)
  + `packages/plugin/src/shared/subc-client.ts` (module-global client lifecycle).
- Provider resolution: in the existing embedding-bootstrap seam — `subc` block present
  → try Synapse provider (three-state) → else fall through to today's resolution
  order untouched.
- Pi: same provider via shared core import; Pi's config loader already reads the
  shared file. Pi-side client lifecycle mirrors (one per process).
- CLI doctor: new check — `subc` block present → report daemon reachability +
  Synapse certification state + lane model/fingerprint (redacted paths).
- Dashboard: NO v1 changes (config editor renders unknown keys as-is; embedding
  Test Connection keeps testing the fallback lane it knows).

## Explicitly out of scope v1

- Transform/cache-core paths (unrelated lane; memory 7755 constraint untouched).
- rerank/microllm ops; dreamer A/B (separate lane).
- Hot-reload of the subc block; multi-daemon.
- Any change to existing embedding providers or their configs.

## Tests

- Config: gate parses; `~` expansion; project-tier strip (security); absent block =
  zero behavior change (byte-identical provider resolution — regression); malformed
  subc block marks the load untrusted (GC latch).
- Three-state: absent / uncertified / ready transitions incl. TTL re-check and
  probe.start fired once; fallback provider used on 1 and 2; TTL probe coalescing
  under concurrent callers; lane flip re-registers (generation guard rejects stale
  in-flight embeds); no provider-internal fallback (Synapse provider returns null on
  daemon death, nothing saved under the Synapse modelId).
- Query pinning: unifiedSearch compares the query vector only against the modelId that
  produced it across a mid-search lane flip; equivalent_to registration maps to the
  existing identity (no re-embed, cache keys stable).
- Error typing: transient retries honor retry_after_ms; permanent falls back + warns
  once; OutcomeUnknown retries (idempotence comment asserted by test name).
- Identity: fingerprint mismatch rejects saves; equivalent_to maps to existing
  identity; content_sha256 mismatch rejects + logs.
- Client lifecycle: one client per process; reconnect delegated to lib (no
  hand-rolled retry around call() beyond classification).
- Pi: gate + provider resolution parity tests.
- Provider enum: `provider: "synapse"` without subc block warns + falls back
  (never a crash, never a silent socket guess); with block + READY resolves the
  Synapse provider; schema regen (build-schema.ts) carries the new enum value;
  `synapse_shadow` excluded from public config docs (DEV_ONLY_KEYS).
- Shadow phase: primary write unaffected when shadow throws (fail-open mirror);
  shadow vectors land ONLY under the synapse modelId; live search results
  byte-identical with shadow on vs off (regression); batch job resume by item id
  after simulated restart (request_key idempotence).
