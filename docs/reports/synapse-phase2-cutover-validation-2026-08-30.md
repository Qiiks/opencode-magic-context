# Synapse phase-2 cutover validation

**Verdict: NOT-READY**  
**Snapshot:** 2026-08-30 18:49 UTC, while the v0.41.0 release pipeline and live corpus writers were active  
**Compared spaces:** OpenRouter `qwen/qwen3-embedding-8b` primary vs certified local Synapse `gte-modernbert-base-f16` (`f27ac643…`, table epoch 1)

Synapse meets the recall bar exactly and is dramatically faster at query time. The cutover is nevertheless blocked by two report-blocking prerequisite failures and one outage-diagnostics gap:

1. current-fingerprint shadow coverage is not drained (62 missing items / 83 missing vector rows across the three benchmark projects);
2. 2,045 same-key chunk pairs have different content hashes, while the shadow drain declares a compartment covered as soon as any row exists;
3. MC does not consume gen-72's typed certification-refusal reason or surface it through `/ctx-embed`.

No product or configuration changes were made. Both databases were opened with SQLite `mode=ro`; only this report, the harness extension, and an ignored result snapshot were written.

## Method

`bun scripts/ctx-search-benchmark.ts --compare-spaces --skip-p1 --out local-ignore/ctx-search-synapse-phase2/results.json`

The extension reuses the 50 redacted known answers and production search paths. For each query it embeds the query separately in each provider, then searches only rows carrying that provider's model identity. It measures:

- raw compartment chunk-vector retrieval;
- production memory hybrid retrieval with only the vector component/identity swapped;
- production commit retrieval with only the vector component/identity swapped.

FTS-only lanes were not rerun. Retrieval telemetry and counters were disabled. The result snapshot is ignored because it contains live-derived hit metadata; the committed fixture still contains IDs/ranges only.

## Prerequisite 1: shadow coverage — **RED**

Counts below use the live primary registration and the newest shadow descriptor for each scope. Memory totals are active/permanent, unexpired rows that already have a primary vector. A chunk hole requires an exact missing `(compartment_id, window_index)` row; item holes count affected compartments.

| Project | Scope | Primary rows | Current Synapse rows | Residual holes | Hole age |
|---|---|---:|---:|---:|---|
| Magic Context | memory | 520 | 520 | 0 | — |
| Magic Context | commit | 2,000 | 2,000 | 0 | — |
| Magic Context | chunk | 2,000 | 2,032 | 6 rows / 3 items | 2026-07-16 to 2026-08-17 |
| AFT | memory | 518 | 518 | 0 | — |
| AFT | commit | 2,000 | 2,000 | 0 | — |
| AFT | chunk | 1,860 | 1,874 | 26 rows / 13 items | 2026-06-28 to 2026-07-21 |
| Alfonso | memory | 484 | 480 | 4 | 2026-08-30 15:44 UTC |
| Alfonso | commit | 1,959 | 1,930 | 29 | 2026-08-30 06:37–17:26 UTC |
| Alfonso | chunk | 1,166 | 1,173 | 18 rows / 13 items | 2026-06-29 to 2026-08-30 |

A shadow count larger than the primary count is not evidence of completion. The current shadow identity retains rows for keys/windows no longer present in the current primary corpus. More importantly, exact comparisons found current-identity hash mismatches:

| Project | Same-key chunk rows with different `chunk_hash` |
|---|---:|
| Magic Context | 633 |
| AFT | 627 |
| Alfonso | 785 |
| **Total** | **2,045** |

The drain predicate explains the false-complete state. `shadowBackfillMissingBase()` treats a compartment as covered when *any* current shadow row exists; it does not compare window count, window index, or `chunk_hash`. General and `/ctx-embed` candidate selection likewise checks model-row existence before the later hash check can run. Old holes and stale-content rows therefore survive a nominally drained backfill.

### Aug 25–29 certification outage ledger

The durable ledger records the outage, but historical `partial` rows are evidence of refused/partial requests rather than a live queue depth. Across the three projects during Aug 25–29:

| Scope | `complete` requests | `partial` requests |
|---|---:|---:|
| chunk | 26 | 118 |
| commit | 59 | 282 |
| memory | 14 | 72 |

Later complete requests and current row counts prove substantial recovery. The residual exact holes above prove the drain did **not** complete. This is not only live-tail lag: 29 affected chunk items predate Aug 25, with the oldest from June 28.

## Prerequisite 2: vector-space integrity — **identity GREEN, corpus integrity RED**

All current Synapse rows selected by the benchmark use one identity in all three projects:

- model identity: `synapse:v1:d8339316…`;
- fingerprint: `f27ac643f222…`;
- table epoch: 1;
- dimensions: one observed value, 768, in memory/commit/chunk.

Descriptors for the two prior fingerprints (`54a62ef8…` and `b7b42d96…`) remain as rotation history, but their vector-row counts are zero. Rotation-orphan vectors are therefore not silently mixed into the measured current space.

The 2,045 hash mismatches are not cross-fingerprint vector mixing: their model/fingerprint is current. They are a different integrity defect—current-space vectors for stale text—which still invalidates a clean apples-to-apples corpus comparison and must be repaired before cutover.

## Retrieval results

### Aggregate recall on all 50 gold queries

The acceptance bar is: no lane may lose more than 2 percentage points at recall@10 without a named offsetting win.

| Lane | Provider | R@1 | R@5 | R@10 | Δ R@10 |
|---|---|---:|---:|---:|---:|
| chunk-vector | qwen | 26% | 40% | 42% | — |
| chunk-vector | Synapse | 28% | 38% | 42% | 0 pp |
| memory hybrid | qwen | 18% | 20% | 20% | — |
| memory hybrid | Synapse | 20% | 20% | 20% | 0 pp |
| commit | qwen | 2% | 2% | 2% | — |
| commit | Synapse | 2% | 2% | 2% | 0 pp |

The denominator is intentionally all 50 queries for every lane, as requested. Most queries have no gold of a lane's source type, which explains the low absolute memory/commit rates. The paired matrix, rather than these aggregates, is the decision evidence.

### Paired per-query ranks

`—` means the gold was absent from the top 10. C=chunk-vector, M=memory hybrid, G=commit.

| Query | Class | C qwen | C Syn | M qwen | M Syn | G qwen | G Syn |
|---|---|---:|---:|---:|---:|---:|---:|
| conv-01 | conversation | 3 | 6 | — | — | — | — |
| conv-02 | conversation | 1 | 1 | — | — | — | — |
| conv-03 | conversation | 1 | 1 | — | — | — | — |
| conv-04 | conversation | 4 | 3 | — | — | — | — |
| conv-05 | conversation | 1 | 1 | — | — | — | — |
| conv-06 | conversation | — | — | — | — | — | — |
| conv-07 | conversation | 1 | 3 | — | — | — | — |
| conv-08 | conversation | 1 | 1 | — | — | — | — |
| conv-09 | conversation | — | — | — | — | — | — |
| conv-10 | conversation | 1 | 1 | — | — | — | — |
| conv-11 | conversation | — | — | — | — | — | — |
| conv-12 | conversation | 1 | 1 | — | — | — | — |
| conv-13 | conversation | — | — | — | — | — | — |
| conv-14 | conversation | 1 | 1 | — | — | — | — |
| conv-15 | conversation | 1 | 1 | — | — | — | — |
| conv-16 | conversation | 3 | — | — | — | — | — |
| conv-17 | conversation | 4 | 1 | — | — | — | — |
| conv-18 | conversation | 1 | 1 | — | — | — | — |
| conv-19 | conversation | 1 | 1 | — | — | — | — |
| conv-20 | conversation | 1 | 1 | — | — | — | — |
| identifier-01 | identifier | — | — | — | — | — | — |
| identifier-02 | identifier | — | — | — | — | — | — |
| identifier-03 | identifier | — | — | — | — | — | — |
| identifier-04 | identifier | — | — | — | — | — | — |
| identifier-05 | identifier | — | — | — | — | — | — |
| identifier-06 | identifier | — | — | — | — | — | — |
| identifier-07 | identifier | — | 4 | — | — | — | — |
| identifier-08 | identifier | — | — | — | — | — | — |
| identifier-09 | identifier | — | — | — | — | — | — |
| identifier-10 | identifier | 7 | — | — | — | — | — |
| fact-01 | fact/rule | — | — | 1 | 1 | — | — |
| fact-02 | fact/rule | — | — | 1 | 1 | — | — |
| fact-03 | fact/rule | — | — | 1 | 1 | — | — |
| fact-04 | fact/rule | — | — | 1 | 1 | — | — |
| fact-05 | fact/rule | — | — | 1 | 1 | — | — |
| fact-06 | fact/rule | — | — | — | — | — | — |
| fact-07 | fact/rule | — | — | 1 | 1 | — | — |
| fact-08 | fact/rule | — | — | 1 | 1 | — | — |
| fact-09 | fact/rule | — | — | 3 | 1 | — | — |
| fact-10 | fact/rule | — | — | — | — | — | — |
| hard-01 | mixed/hard | — | — | — | — | — | — |
| hard-02 | mixed/hard | 2 | 6 | — | — | — | — |
| hard-03 | mixed/hard | 1 | 1 | — | — | — | — |
| hard-04 | mixed/hard | — | — | — | — | — | — |
| hard-05 | mixed/hard | 5 | — | 1 | 1 | — | — |
| hard-06 | mixed/hard | — | 1 | — | — | — | — |
| hard-07 | mixed/hard | — | 2 | 1 | 1 | — | — |
| hard-08 | mixed/hard | 3 | 5 | — | — | — | — |
| hard-09 | mixed/hard | — | — | — | — | — | — |
| hard-10 | mixed/hard | — | — | — | — | 1 | 1 |

There were six top-10 outcome disagreements, balanced 3–3: qwen-only (`conv-16`, `identifier-10`, `hard-05`) and Synapse-only (`identifier-07`, `hard-06`, `hard-07`).

### Qualitative top-five inspection

- **`conv-16`, qwen win:** qwen ranked the Electron embedding device-selection gold third. Synapse's top five stayed in the same embedding/Electron neighborhood but omitted the exact episode.
- **`hard-06`, Synapse win:** Synapse ranked the Rust-mode SUBC cutover gold first. Qwen's top five were adjacent Rust/SUBC and Pi-subagent episodes but missed the exact cutover.
- **`hard-07`, Synapse chunk win with hybrid parity:** Synapse ranked the shadow byte-comparison episode second while qwen missed it in the chunk top ten. Both memory hybrids independently ranked the paired shadow-ordinal memory first.

These are plausible semantic differences, not obvious wrong-space artifacts. Because coverage/corpus integrity is red, they should be rerun after repair before treating the balance as final.

## Query-embedding latency

Each provider embedded all 50 live queries successfully. “Cold” is the first measured query per project (three samples); “warm” is the other 47. Discovery was completed during routing, matching normal plugin bootstrap rather than charging `models.list` to every search.

| Provider | Total n | Cold p50 | Cold p95 | Warm n | Warm p50 | Warm p95 |
|---|---:|---:|---:|---:|---:|---:|
| qwen / OpenRouter | 50 | 4,255 ms | 8,292 ms | 47 | 1,387 ms | 8,355 ms |
| Synapse / local subc RPC | 50 | 42 ms | 47 ms | 47 | 28 ms | 35 ms |

This is the principal measured Synapse win. Query embedding is on the user-facing search path; warm median falls by about 1.36 seconds and warm p95 by about 8.32 seconds.

## Full-context hypothesis

| Corpus scope (three projects) | Items | Estimated >8,192 tokens | Maximum |
|---|---:|---:|---:|
| compartment raw transcripts | 4,303 | 399 (9.3%) | 45,054 |
| memories | 5,363 | 0 | 1,102 |
| commits | 5,959 | 0 | 1,221 |

Five query/gold instances point at an over-cap compartment (`conv-01`, `conv-05`, `conv-10`, `conv-13`, `hard-08`; `conv-10` and `hard-08` share one item). At recall@10, qwen and Synapse outcomes were identical on all five: four hit in both and one missed in both. Ranks differed for `conv-01` (3 vs 6) and `hard-08` (3 vs 5), both favoring qwen.

The claimed “qwen truncated, Synapse complete” advantage is **not present in MC's current integration**. Both resolved configs advertise an 8,192-token cap, and both compartment spaces use the same 90%-budget windower (7,372 estimated tokens per window). Long transcripts are split before either provider sees them. Synapse showed no unique retrieval win on an over-cap gold.

## Cutover mechanics review

### What the primary flip changes

1. `resolveEmbeddingRouting()` probes `models.list` before registration. A healthy Synapse lane becomes a resolved config pinned to model, fingerprint, table epoch, dimensions when known, and connection file.
2. `registerProjectEmbedding()` changes the active memory/commit model ID and chunk model ID from qwen identities to `synapse:v1:<model+fingerprint>` identities. Existing shadow rows become the immediately searchable primary corpus; no row copy is required.
3. Query embeddings carry the exact memory/chunk IDs. Searches load only rows with those IDs. Generation checks reject a query captured across a registration change.
4. The configured `fallback_provider` is a **bootstrap/re-registration selection**, not a per-request runtime fallback. The fallback's own identity is registered, so fallback vectors cannot be written under a Synapse identity.
5. Primer vectors are not part of shadow dual-write/backfill. After a flip, existing primer vectors retain the qwen model ID; primer FTS still works, but the semantic component is absent until those primer embeddings are regenerated. No rotation-aware primer backfill was found.

### What a four-day certification outage looks like with Synapse primary

- **Already-running, Synapse-registered process:** the first rejected `embed.query` is retried up to three times, then returns no vector. There is no in-process switch to `fallback_provider`. Message/memory/commit FTS continues; chunk-vector retrieval disappears; hybrid vector components disappear. Background batches return partial/empty maps and their drains stall.
- **Within the 60-second discovery cache:** a re-registration can reuse the previously healthy `models.list` promise and remain on Synapse even though admission has begun refusing work.
- **Fresh discovery (new process, project bootstrap, or `/ctx-embed start` after cache expiry):** if `models.list` truth marks the lane not certified, routing selects the configured fallback. With qwen fallback, the existing qwen corpus is immediately usable. Local fallback creates another vector identity and needs its own backfill; `off` disables semantics.
- **Recovery:** the next fresh registration selects Synapse again. Primary drains then fill rows missing under the Synapse identity. There is no continuous health poll that automatically flips an already-running registration at the first refusal or immediately flips it back on recovery.
- **User-visible behavior today:** `/ctx-embed start` stops after repeated failed batches and says only that the provider “returned no result” and to retry. `/ctx-embed` status reports provider/model/coverage, not certification state or refusal cause.

### Gen-72 health/refusal surface consumption

SYNAPSE's side of the former health gate is shipped, but MC does not consume it:

- no MC call reads `health()` certification fields or `admission.status` counters;
- `@cortexkit/subc-client` 0.4.1 parses Error frames as `{code,message}` and drops a separate typed `reason` field;
- `embedding-synapse.ts` classifies only a top-level `code`/Error code and does not inspect a nested or separate refusal reason;
- `SynapseEmbeddingProvider` does not implement `getLastFailureReason()`;
- `embedding-failure.ts` and the OpenAI-compatible provider have classified diagnostics, and a formatter exists, but the live hook does not call it—its stalled branch hard-codes the generic “provider returned no result” message.

Therefore MC cannot reliably go loud from the first typed certification refusal. If gen-72 duplicates the typed reason into `code`, the internal log may classify `not_certified`; the user still receives the generic `/ctx-embed` text.

### Fingerprint rotation and mixed-space windows

SYNAPSE's current commitment is favorable: machine-profile rotations do not rotate fingerprints (`f27ac643…` held through Aug 25–29); only deliberate numerics changes rotate, with a pre-announced migration window and alias/equivalence rows; `models.list` remains authoritative.

MC behavior is stricter but more expensive:

- the d833/f27 shadow registration backfills active memories that already have primary rows, commits that already have primary rows, and compartments that already have primary rows;
- it does not cover permanent memories in the missing-set query, primary holes, primers, or stale same-model chunk hashes;
- when Synapse is primary and its fingerprint changes, the model ID changes. MC does not consume Synapse alias/equivalence rows, so it performs a full logical re-embed of every supported current item, merely bounded per sweep (500 commits, 200 chunks, small memory batches), rather than treating equivalent fingerprints as migrated;
- the same active-only memory query means permanent memories can remain stranded after primary rotation.

Cross-fingerprint vector mixing is prevented by construction as long as the daemon reports truthful metadata: response fingerprint/epoch validation, fingerprint-derived model IDs, exact-ID SQL filtering, generation checks, and pre-registration fallback selection all fail closed. What is **not** prevented is stale-content mixing inside one fingerprint/model ID, as the 2,045 hash mismatches demonstrate.

## Residual gates

1. **Repair and rerun coverage (blocking).** Make shadow/current chunk reconciliation key- and hash-complete, repair the 62 missing items / 83 rows and 2,045 hash mismatches, prove zero holes for all three projects, then rerun the same paired benchmark.
2. **Keep the recall bar.** The current run passes: no lane loses at recall@10. The repaired-corpus rerun must also lose no more than 2 pp in any lane without a named offsetting win.
3. **Wire gen-72 refusal diagnostics (blocking).** Upgrade/extend the subc error parser to retain typed `reason`; map Synapse refusal classes into `EmbeddingFailure`; expose `getLastFailureReason()`; and use the classified formatter in the live `/ctx-embed` path. First certification refusal must become a user-visible, non-generic diagnostic.
4. **Define runtime fallback expectations.** Either document fallback as bootstrap-only, or implement a bounded first-refusal transition/re-probe policy. A four-day outage in an already-running process currently loses semantic retrieval without switching to qwen.
5. **Close rotation coverage.** Include permanent memories, add a deliberate primer re-embed path, and decide whether MC will consume announced fingerprint equivalence or intentionally full-reembed on every numerics rotation.
6. **Add a hash-complete invariant.** A “drained” status must be non-vacuously proven against row count, window keys, and `chunk_hash`, not just model-row existence.

Once gates 1, 3, 4, and 5 are closed, Synapse's measured recall parity and latency advantage support a cutover-ready verdict.
