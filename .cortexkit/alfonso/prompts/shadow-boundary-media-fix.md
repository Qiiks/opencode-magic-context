# Shadow soak round 3: seed-time boundary adoption + Media block pass-through

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Rust side: crates/mc-module (transform.rs, ck_wire.rs, shadow arms, mc-store if schema needed). TS side: packages/plugin/src/hooks/magic-context/shadow-sender.ts. The shadow soak went live today and its first hour of real traffic surfaced exactly two structural defects. The mission context matters: shadow mode exists to prove the Rust module byte-matches the OpenCode plugin transform so the plugin can eventually become a thin client. Every defect here is a parity gap to CLOSE, not to special-case away.

## Defect 1 (design hole): seed sync never adopts a boundary identity

Live evidence (shadow_divergences rows 5124/5125, session ses_12f72c654ffe94...): a compacted production session's fresh shadow generation seeded all 72 compartments correctly (verified 1:1 with context.db), but the shadow lineage's durable boundary is "" (never minted). The TS lane declares its real compaction marker (declared boundary "msg_...#2"). DeclaredTrimValidated's boundary_identity predicate correctly rejects ("declared boundary X did not match durable boundary ''"), the lineage quarantines at pass_seq=1, and because the incoming wire is already TS-trimmed (boundary-absent share-nothing shape), the module rides pending_rewrite passthrough forever. Every compacted session — the sessions that matter most for parity — quarantines at pass 1.

### Fix: adopt the boundary during seed
When a seed state_sync carries compartments AND the sender supplies the TS lane's current marker identity (it already sends marker state — verify what's on the wire today; extend the seed payload if needed), the seed transaction must also install the boundary identity into the shadow lineage's durable state (boundary_id in core state / ModuleMeta coverage bookkeeping) so the first shadow_transform validates declared-trim against a REAL boundary.

VALIDATION, not trust: the adopted boundary must be consistent with the seeded compartments — the declared flat-id's mid must equal the highest-sequence seeded compartment's end_message mid (index checked parseable; if the declared index can't be verified against seeded data, adopt with the end-block form the publisher would mint). If the declared boundary does NOT match the seeded compartments (stale marker, mid-publish race), REJECT the seed with a typed error → sender resets and reseeds once (existing retry machinery). Never adopt an unverifiable boundary — a wrong adopted boundary would corrupt every subsequent compare.

Also set coverage bookkeeping consistently (coverage_ordinal etc.) from the seeded compartments so the first pass's leading-edge/coverage checks hold. Check what the publish path sets after a real fold and mirror the same fields — the seed should leave the lineage in the same durable shape a real fold would have.

### Sender side
Ensure the seed payload carries the TS marker/boundary identity explicitly (flat block id vocabulary, mid#index). If resolveDeclaredTrimForShadow already computes it per-pass, reuse that derivation at seed time. Regenerate the cross-language wire fixture (bun packages/plugin/scripts/generate-shadow-wire-fixture.ts) if the payload shape changes.

## Defect 2: Media blocks reject the whole shadow pass

Live evidence: "shadow: send failed (ignored): shadow_transform_failed: ck wire: unsupported CK block media at msg_...#1" — a pasted screenshot in the user message kills every shadow pass for that session. Opaque blocks were made first-class pass-through earlier (projected verbatim, never interpreted, selection classifies never-reducible); Media was left rejected because the CC leg never carries it. OpenCode traffic DOES (pasted images).

### Fix: Media becomes first-class pass-through exactly like Opaque
In crates/mc-module/src/ck_wire.rs (ensure_supported / ensure_output_supported and the projection): project Media blocks verbatim (never interpreted, byte-preserved through build_output), selection treats them as never-reducible (they're handled by the TS image-strip lane which the shadow input already reflects — by the time MC sees the wire, TS's processed-image strip has already replaced old images; live current images must ride through untouched). Mirror whatever block-identity/fingerprint treatment Opaque got so the boundary-absence and identity-drift bases stay consistent (memory: identity excludes per-turn-churning fields; Media payload bytes are NOT per-turn-churning — a media block's bytes are stable for a given message).

Check the chunk builder too: if a Media block can reach the historian chunk path (it can, in the compactable head), it must serialize into the chunk text deterministically (a short placeholder like the TS lane's TC: representation — look at how the TS historian chunk represents image parts and mirror that), never crash assembly.

## Tests (non-vacuous)
1. Seed-with-boundary: seed a lineage with N compartments + declared boundary matching the last compartment → first shadow_transform validates declared-trim (predicate passes), NO quarantine, second pass compares fully. Must fail on current code.
2. Seed-with-stale-boundary: declared boundary that does NOT match seeded compartments → typed seed rejection, sender-side single reseed retry path exercised.
3. Media pass-through: a wire with a Media block in a live message → pass succeeds, block projected verbatim byte-identical across two passes, selection never targets it.
4. Media in compactable head → historian chunk builds with the deterministic placeholder, no crash, coverage validation intact.
5. Regression: Opaque behavior unchanged; the golden/fixture suite regenerated where the wire shape changed (never hand-edited).

## Gates
cargo test -p mc-module -p mc-store, clippy --all-targets -D warnings, fmt. packages/plugin: bun test, typecheck, lint (sender changes). Cross-language wire fixture regenerated + green on both sides. check_comments clean (invariants only — e.g. "a seed must leave the lineage in the shape a real fold would have"; no incident references). Report: which durable fields the seed now sets, with the publish-path parity table.
