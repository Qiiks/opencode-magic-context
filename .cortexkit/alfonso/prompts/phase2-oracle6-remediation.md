# Phase-2 remediation round 6 (self-Oracle BLOCK on round 5: 1 High + 1 Medium)

Repo: subc-migration, current HEAD (round-5 merge). Work in crates/mc-module + crates/mc-store. Gates: `cargo test -p mc-module --lib`, `cargo test -p mc-store`, `cargo clippy -p mc-module --all-targets -- -D warnings`, `cargo fmt --check`. FAIL-FIRST tests for both fixes. Comments explain WHY, no Oracle references, no em-dashes.

## HIGH: reattach fence binds the wrong generation

Defect: the transform request A that triggers a reattach records its own snapshot generation (lib.rs ~4287-4291), but maybe_spawn_reattach does NOT receive it; the reattach path instead calls transform_snapshots.observe() later (lib.rs ~2281-2289) and fences on whatever entry is current THEN, while building the chunk from A's parsed request/projection (lib.rs ~2295-2321). If transform B begins between A's capture and the observe() call (reachable after the await at ~4366 + second prepare at ~4375, and under any multithreaded embedding), the fence captures B's generation, matches_observation accepts B, and A's stale compartments/transcript/facts/floor publish against B's rows.

Fix (exact, per review):
1. Thread the INITIATING transform's snapshot generation (the u64 recorded at ~4287-4291) through prepare_historian_fire and maybe_spawn_reattach into the reattach path.
2. ReattachSnapshotPublicationFence stores that exact `generation: u64` (replace the TransformSnapshotObservation field). At publication, under the snapshots lock, accept ONLY that generation present as InFlight OR Ready; absence or any other generation rejects with FenceRejected. Remove the Absent arm from this path entirely (the handler-driven reattach is always initiated by a transform that has already called begin(), so Absent means superseded, not fresh). Delete TransformSnapshotObservation and observe()/matches_observation if nothing else uses them; keep a simple generation_present_in_flight_or_ready(session_id, generation) predicate on the cache.
3. Tests:
   a. Pre-observation supersession (fail-first proves current code publishes wrongly): use the existing between_transform_and_prepare test hook to begin() transform B between A's generation capture and reattach spawn; drive the reattach to publication; assert FenceRejected path taken, historian Idle, failure_backoff_at_ms None, ALL additive tables + publication floor unchanged.
   b. Same-generation InFlight -> Ready control: A's generation captured while InFlight, finish_ready() with the SAME generation before publication; publication must be ACCEPTED (preserves the intended acceptance case).

## MEDIUM: H1 race test does not kill a load-then-commit mutant

Defect: the current interleave hook performs the competing cache-state bump BEFORE abandon_historian_run_if_matching is invoked, so a mutant that re-implements it as plain load() + predicate-check + commit() loads the already-bumped row and still passes. The store-level test has the same shape, and the integration test never asserts its hook ran.

Fix (exact, per review):
1. Add a store-local #[cfg(test)] callback inside abandon_historian_run_if_matching invoked AFTER the predicate read and BEFORE the UPDATE, inside the fenced transaction (same pattern as existing test hooks; keep it Option<Box<dyn FnMut()>> behind a Mutex on the store or a thread_local).
2. From the test's callback, open a SECOND raw rusqlite connection to the same database with busy_timeout=0 and attempt a write (e.g. UPDATE mc_cache_state SET row_version = row_version). Under the real BEGIN IMMEDIATE implementation this must fail SQLITE_BUSY (assert it does). A load-then-commit mutant holds no write lock at that point, the write succeeds, and the test fails.
3. Assert the callback actually ran (shared flag).
4. Keep the existing race test but fix its ordering so the bump happens through the new in-transaction callback (or via a second connection attempt), and exercise BOTH abandon variants (no-cooldown FenceRejected route and cooldown CAS/fingerprint route) through the common primitive.

## Deliverables

Single commit. All gates + real_daemon if quick. Report per-item: fix summary, test names, fail-first evidence (the pre-fix wrong-publish for the High; the surviving mutant for the Medium), gate outputs.
