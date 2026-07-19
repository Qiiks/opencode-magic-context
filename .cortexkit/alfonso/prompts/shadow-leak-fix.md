# Shadow-sender memory leak: bound + coalesce the work queue (re-enable gate)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. The shadow transform lane (packages/plugin/src/hooks/magic-context/shadow-sender.ts) caused a sustained ~15GB RSS climb + high CPU in `opencode serve` and had to be disabled. It must be provably leak-free before re-enabling, because the shadow soak is the parity gate for the Rust-module cutover. Verify the mechanism at source before fixing.

## Observed + suspected mechanism (verify, then fix what you find)
- Symptom: RSS climbing over an hour to 24GB with shadow enabled on the largest session (138k+ ordinals, hundreds of compartments); dropped ~15GB immediately on disable+restart. Accumulation, not a one-shot read (the ordinal-resolution full-read storm was already fixed — resolveOrdinalsForShadow now id-only + provider-cache, commit a23d4772 / 3aa74cd7).
- PRIME SUSPECT (verify at source): the shadow work queue in shadow-sender.ts. The coalescer (`pushWork`, ~:1060-1078) collapses ONLY `kind === "pass"` items; seed / `state_sync` / `shadow_reset` work items are NOT coalesced. The drain loop (~:1083) awaits subc round-trips per item. If the subc round-trip STALLS (transport was flaky), the drain wedges while every subsequent pass keeps enqueuing large full-sync (`buildStateSyncPayload force:true`) blobs that never collapse — on the largest session each blob is big → GBs. Also confirm the queue itself has NO absolute size bound and that a stalled/failed send does not retain payloads indefinitely.

## Fix (implement what the source actually shows; the below is the intended shape)
1. COALESCE state_sync work items too: at most ONE pending full `state_sync`/seed per session in the queue — a newer full-sync supersedes an older un-drained one (same collapse `kind==="pass"` already gets). A full state_sync is idempotent-latest-wins, so keeping only the newest is correct.
2. ABSOLUTE QUEUE BOUND per session: cap the queue length; on overflow drop OLDEST non-essential items (shadow is best-effort dev instrumentation — dropping a shadow pass is fine, it just means that pass isn't byte-compared). Emit a single counter, not per-drop logs.
3. BACKPRESSURE on a stalled drain: if a send is in-flight/stalled, do NOT let the queue grow unbounded behind it — either a bounded-wait with drop, or a single-slot "latest wins" for full-syncs. A subc round-trip that times out must free its payload (no retained reference) and must not block the queue forever — bounded timeout + drop-and-count.
4. HARD MEMORY CEILING check: verify no path retains the full raw-message array or per-pass annotated snapshots beyond the single in-flight send. The annotated input array must be released once its send resolves/fails.

## Constraints
- Shadow is dev-instrumentation, user-tier-only, default-off (project-security strips it), gated behind `shadow_transform.enabled` + a discoverable subc daemon. Dropping shadow work under pressure is ALWAYS acceptable — correctness of the REAL transform must never depend on shadow. Confirm the shadow path is fully try/caught so nothing here can throw into the real transform (memory: the capture-clone must be inside the shadow try/catch).
- Do NOT change the real transform bytes or timing. This is purely the shadow enqueue/drain lifecycle.
- Keep the byte-compare semantics intact for the passes that DO get compared (a dropped pass is skipped, not falsely reported as matching).

## Tests
- queue coalesces multiple pending state_sync into one (assert queue depth after N enqueues).
- queue respects the absolute bound (enqueue > cap → oldest dropped, newest kept, counter incremented).
- a stalled/timed-out send frees its payload and does not wedge the queue (mock a slow subc send; assert later enqueues still bounded + earlier payload released).
- shadow send throwing does NOT propagate into the real transform path.
- a non-dropped pass still produces a byte-comparison (semantics preserved).

## Gates
packages/plugin: bun test src/hooks/magic-context/shadow-sender*, typecheck, lint, check_comments. Comments explain WHY (best-effort dev lane; bound+coalesce prevents the full-sync pileup on stalled drain). Report the source-confirmed leak mechanism + per-fix status + test evidence. Do NOT re-enable the config flag — Ufuk re-enables after review.