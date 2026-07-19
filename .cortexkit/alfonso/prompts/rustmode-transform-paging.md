# Fix: authority `transform` requests have no module-side page reassembly

Repo: this worktree (branch from `subc-migration` HEAD). Rust (crates/mc-module) primarily; small TS test touch allowed.

## Live evidence (verify at source before coding)

Rust-mode beat on `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO` (216 messages, body > MODULE_PAGE_MAX_BYTES): the TS adapter pages the transform body via `buildPagedModuleTransformPayloads` (packages/plugin/src/hooks/magic-context/module-wire.ts:211) — incomplete pages carry only the page envelope (`transform_page_id/generation/index/total/complete/digest`) + array fields; scalar fields (render_config, serializer_profile, usage, pass_inputs...) ride only the final `transform_page_complete: true` page. Module-side, `dispatch_value` routes `"transform"` directly to `handle_transform_value` (lib.rs:4997), which serde-parses the page as a full `TransformRequest` and fails with `missing field render_config`. Page reassembly exists ONLY on the shadow lane (`handle_shadow_transform_value` detects page fields at lib.rs:6250-6266 and routes to `handle_shadow_transform_page_value`).

## Fix

Generalize transform page reassembly to the authority lane:

1. In `dispatch_value`'s `"transform"` arm: if any of the six page-envelope fields is present, route to a page-reassembly path (all-or-none envelope validation exactly like the shadow path). Reuse/generalize the existing shadow transform page coordinator (`handle_shadow_transform_page_value` + its phase machinery) rather than duplicating it: parameterize by lane the same way the recent state_sync generalization did (see `StateSyncLane` and `handle_state_sync_value` — follow that pattern and its session-binding accessor `state_sync_binding`, NOT `shadow_binding`, so real-session bindings are accepted).
2. On assembly completion, the assembled body must flow into `handle_transform_value` unchanged (scalars from the complete page + concatenated arrays in original order). Digest/generation/all-or-none violations discard the partial assembly and return the same typed errors as the shadow path.
3. Buffer bounds: reuse the shadow page coordinator's existing per-session byte caps and discard-on-overflow behavior; authority pages must count against the same bounded budget (no unbounded growth from a misbehaving sender).
4. Concurrency: one in-flight paged transform per session; a new transform_page_id while one is assembling discards the old (newer-wins, same as shadow). Confirm what the shadow path does and mirror it; report if it differs.
5. The kill switch must NOT gate authority paged transforms (only shadow-namespaced sessions), consistent with the state_sync lane split.

## Tests (fail-first)

- Authority paged transform end-to-end: build a >1-page body TS-style (use the same field split as buildPagedModuleTransformPayloads: envelope+arrays on pages, scalars on the final complete page), send pages in order on a real-session binding, assert the assembled transform executes and returns the normal response (fail-first: today page 1 returns bad_request missing field).
- Out-of-order / missing page / digest mismatch → typed rejection + assembly discarded.
- Shadow-namespaced paged transform still works and still gates on the kill switch.
- Interleaved sessions: two sessions assembling concurrently do not cross-contaminate.

## Gates

cargo test -p mc-module (lib + integration), clippy -D warnings, changed-file rustfmt. If you touch TS tests, focused suites + typecheck. Comments explain the lane-shared coordinator invariant; never reference this incident or plan files. No em-dashes. Report files, test names, and any divergence found between shadow and authority paging semantics.
