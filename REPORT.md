# ASTRO rust-mode m0 wire invariant incident report

## Mechanism

The codec hypothesis was confirmed.

A test-first reproduction decodes a boundary-bearing OpenCode input containing normal messages and one persisted synthetic user nudge, then encodes a fresh m0/m1 prefix followed by the retained input. Before the fix, `fresh_boundary_prefix_does_not_borrow_persisted_synthetic_meta` failed because m0 did not have a sidecar match and `encode_opencode_impl` selected the first input synthetic message positionally. `encode_with_meta` then started from the nudge envelope; its block metadata could not match the fresh m0 block, so the newly rendered part lacked `synthetic: true`, while the retained native envelope also exposed the nudge identity to m0.

The positional fallback had only one call site. Git history showed it was introduced with native OpenCode serving, but current decode behavior always gives every decoded message, including synthetic input, a retained `harness_id`. Such input messages therefore rebind through `meta_for_ck` by exact mid. There was no test or production caller that required a synthetic input message to lose its mid and rebind by position. The fallback was removed, so sidecar message metadata now binds by identity only; fresh module-authored messages take `encode_new_message`, which scopes synthetic user output to the active session and marks every part synthetic.

The existing no-persisted-synthetic native-serving golden remains unchanged and passes. The broader OpenCode and Pi codec goldens also pass unchanged.

## Pi cross-check

`crates/mc-module/src/codec/pi.rs` has no positional synthetic metadata lookup. `encode_pi` already uses `meta_for_ck` followed by `encode_new_message`, so no symmetric Pi code change was needed.

## Unknown-limit emergency fail-closed evidence

The second hypothesis was also confirmed. An overflow event with `reportedLimit=unknown` persists `needs_emergency_recovery` and the `provider_overflow` origin but leaves `detectedContextLimit` at zero. The Rust adapter gate required either a resolved trusted numeric limit or a positive detected provider limit, so this state could not arm `EmergencyFailClosedError` after adapter validation failed.

The gate now accepts a second, non-numeric provider proof only when both facts exist:

1. durable recovery is armed with origin `provider_overflow`; and
2. a process-local reconfirmation marker proves another provider overflow arrived while that durable latch was already armed.

A durable unknown-limit latch by itself still does not abort, and the first unknown-limit arm alone does not set the reconfirmation marker. `proactive_model_shrink` remains excluded by the persisted origin check. This does not promote estimator-only evidence to provider proof.

Regression coverage proves both sides: a repeated unknown-limit provider rejection aborts instead of serving raw, while the first unknown-limit overflow arm keeps the existing fallback behavior.

## Self-heal and committed state

No store surgery is required for ASTRO.

The rejected native output did not replace the host-owned raw message array. The module commit can advance durable core, coverage, overlays, and served fingerprints, but output construction on every later pass creates new synthetic CK messages from the durable `m0` and `m1` frozen units before rendering the retained tail. With the positional codec binding removed, those fresh CK messages encode through `encode_new_message` regardless of prior discarded native output.

Coverage does not require the rejected wire to have been accepted: the durable boundary is validated against a live raw block identity (or an already validated declared trim), and the raw input still contains the untrimmed covered messages and boundary anchor. The next pass can therefore validate the live boundary and apply the same durable trim again. Existing shape lanes also rebuild a missing m1 and reject/reconcile an actually absent or inconsistent boundary instead of silently advancing it. The observed discarded-serve window therefore did not create state that outruns the next correctly encoded wire.

## Files changed

- `crates/mc-module/src/codec/mod.rs` — persisted-synthetic boundary regression.
- `crates/mc-module/src/codec/opencode.rs` — identity-only sidecar binding for synthetic messages.
- `crates/mc-module/src/codec/sidecar.rs` — removed the unused positional synthetic lookup.
- `packages/plugin/src/features/magic-context/storage-meta-persisted.ts` — process-local repeated-provider-overflow evidence and lifecycle cleanup.
- `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts` — repeated provider-event proof for unknown reported limits.
- `packages/plugin/src/hooks/magic-context/rust-mode-transform.test.ts` — repeated-event abort and first-event non-abort regressions; recovery-registry test isolation.
- `REPORT.md` — this report.

No tool schemas, guidance text, database migrations, package manifests, or lockfiles changed.

## Verification

- Test-first reproduction: failed before the codec fix at the all-parts-synthetic assertion; passed after the fix.
- `cargo fmt --all -- --check` — passed.
- `cargo test -p mc-module -p mc-store` — passed (mc-module 684 passed / 3 ignored, real-daemon test passed, mc-store 104 passed).
- `bun test` in `packages/plugin` — passed (3225 passed, 0 failed).
- `bunx tsc --noEmit` in `packages/plugin` — passed.
- `bun run test:rust-e2e` in `packages/e2e-tests` — passed (10 passed, 4 explicitly gated/skipped, 0 failed across 9 files).
- `bun run typecheck` in `packages/plugin` — plugin source typecheck passed, but the pre-existing scripts typecheck failed in unrelated files (`bench-synapse-vs-local.ts`, `generate-mural-font.ts`, `test-mural-render.ts`, and `test-synapse-embed.ts`).
- `bun install --frozen-lockfile` was used to hydrate the isolated worktree; it changed no tracked dependency files.
