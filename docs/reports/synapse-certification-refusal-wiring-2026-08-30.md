# SYNAPSE certification-refusal wiring validation

**Scope:** Magic Context gate 3 consumer-side diagnostics only. No storage, routing, or fence behavior changed.

## Contract pin

The classified vocabulary is a checked-in snapshot of the `StableError` constructors in SYNAPSE's drift-guarded Errors surface:

- `synapse@c43cd33` — `crates/synapse-core/src/error_contract.rs` (Errors)
- Enumeration mechanically supplied by the operator from that source because this worktree is fenced from the SYNAPSE checkout.

`SYNAPSE_ERROR_VOCABULARY` is the sole copy in MC. Its test parses the pinned Errors-form snippet and compares every literal to that snapshot. Certification actions are exact for `not_certified`, `probe_required`, and `migration_required`; an unknown `detail` beneath coarse `certification_refused` remains a certification refusal, preserving the literal as a newer-than-snapshot contract value rather than misclassifying it as transport.

## Parse-layer pin

The implementation targets `@cortexkit/subc-client` 0.10.0's additive `SubcCallError.detail` getter (subconscious master `572e247d`). For isolated verification, the supplied worktree-only `vendor/cortexkit-subc-client-0.10.0.tgz` (SHA-256 `c838b67735a4454909b68396…`) provided that surface. The committed package manifest and lock were restored to the repository's canonical resolution as requested; normal integration refresh supplies the real 0.10.0 dependency.

The provider reads typed reasons only from `detail`; it does not treat coarse `code` as a duplicated typed reason. The downgrade-shape test proves a pre-0.10 error without `detail` cannot crash and renders the wildcard certification-refusal class.

## User-visible outcome

`SynapseEmbeddingProvider.getLastFailureReason()` now records classified failures. The existing session backfill path already carries that evidence into a stalled outcome, and `/ctx-embed` now uses `formatEmbedFailureSummary()`. A first `not_certified` refusal therefore names `not_certified` and directs the user to recertify SYNAPSE or configure `embedding.fallback_provider`, rather than saying the provider returned no result.

## Verification

- `bun test src/features/magic-context/memory/embedding-synapse.test.ts src/hooks/magic-context/format-embed-failure.test.ts` — passed (17 tests).
- `bun run typecheck` in `packages/plugin` — passed against the temporary 0.10.0 detail surface.
- The formatter test is mutation-directional: replacing the classified summary with the generic stalled message removes the cause/recovery assertions; mapping a typed certification refusal to `transport_error` breaks its exact class assertion.
