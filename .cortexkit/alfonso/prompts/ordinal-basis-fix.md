# Fix: 1-based ordinal assumptions strand ordinal 0 (fold refuses forever)

Branch subc-migration (HEAD 77ce1a8c), crates/mc-module + touched call sites. Live failure on the rig: llm-runner stamps message ordinals 0-BASED (message 0 = first message; llmr-core stamp_ordinals "0, 1, 2…"). The main session's first message is a USER turn at ordinal 0 (mid "m0"). Three places in mc-module assume the TypeScript 1-based convention, so ordinal 0 is silently excluded from every historian chunk; no compartment can ever cover it, and the m0-fold's leading-coverage-gap guard then correctly refuses on every pass:

"leading coverage gap: live item m0#0 (ordinal 0) sits before the first compartment start (ordinal 1)"

The guard is RIGHT (dropping a live user message from the tail must fail loud). The basis assumption is the bug. The module must be ORDINAL-BASIS-AGNOSTIC: never assume the first ordinal is 1 (or 0); derive floors from the live array and the store.

## Sites to fix

1. crates/mc-module/src/historian_chunk.rs assemble_historian_firing (~line 463):
   today: chunk_start = compartments.map(end_message).max().unwrap_or(0)+1 .max(1)
   fix: if compartments is empty → chunk_start = the MINIMUM ordinal among eligible live messages (non-synthetic, role != "system", ordinal < eligible_end). If none exist → NoFire(EmptyChunk). If compartments exist → max_end + 1 (drop the .max(1) floor entirely).

2. crates/mc-module/src/boundary.rs — BoundaryContext.last_compartment_end_ordinal: u64 becomes Option<u64> (None = no compartments yet; Some(e) = last published end, e may legitimately be 0 for a 0-based first compartment covering only ordinal 0 — the u64-with-0-sentinel is EXACTLY the seq-0 sentinel bug class we fixed before, do not reintroduce it).
   - resolve_protected_tail_boundary offset (~line 445): offset = match { Some(e) => e+1, None => min live message_ordinal (the messages slice is in scope; if empty, keep the existing raw_message_count==0 early return) }.
   - check_compartment_trigger offset (~line 638): same derivation.
   - fence/semantic-snap call sites that take last_compartment_end_ordinal: pass Some/None through; inside, a None floor means "no publication floor" — use the min-live-ordinal-derived offset consistently.
   - prior_boundary_ordinal / migration_floor_active defaults: leave semantics; only the compartment-end field changes type.
   - Update BoundaryContext::default() and ALL test constructions (mechanical: 0 → None where tests meant "no compartments", Some(n) where they meant a real floor).

3. crates/mc-module/src/lib.rs maybe_spawn_historian_fire (~line 491): last_compartment_end_ordinal = load_compartments().map(max end) — change to Option: None when the list is empty, Some(max) otherwise. Thread into BoundaryContext.

4. crates/mc-module/src/historian_validate.rs validate_stored_compartments (~line 535): `let mut expected_start = 1;` hardcodes the 1-anchor. A store-pure function cannot know the session's ordinal basis. Fix: anchor to the FIRST stored compartment's start_message (check only inter-compartment contiguity + range validity here). The leading anchor (does coverage start at the session's true first message?) is owned by the fold's live-aware leading-gap check in transform.rs, which has the live array — add/adjust a comment there stating this ownership split. Also check validate_historian_output's chunk-vs-prior check ("Historian chunk starts at raw message X but existing compartments end at Y") still holds — it compares chunk.start_index to last.end_message+1, which is basis-agnostic already: keep.

5. Search for any OTHER `.max(1)` / hardcoded-1 ordinal floors in crates/mc-module (boundary.rs chunked_message_estimate has `let start_ordinal = start_ordinal.max(1)` (~line 577) and a `estimate whole-session` helper calling with start=1 (~line 560) — fix both to accept the true floor; callers pass the derived offset).

## What must NOT change
- The fold's leading-coverage-gap guard in transform.rs stays EXACTLY as strict (role!="system" exemption only). Do not weaken it — it just caught a real bug.
- The chunk builder's system-role skip and the trigger's boundary_messages system filter stay.
- The existing chunk-builder differential golden (1-based TS fixtures) must still pass UNCHANGED — the fix is basis-agnostic, not a 0-based cutover. Do NOT regenerate goldens.
- TS plugin untouched.

## Tests (all new, alongside existing)
a. ZERO-BASED SESSION E2E (the rig scenario): handler-level test with messages at ordinals 0..N (m0 = user at ordinal 0, no system message), store empty → trigger fires → assembled chunk_start == 0 (chunk includes ordinal 0) → scripted producer returns compartments covering 0..k → publish succeeds → NEXT pass HARD-folds and mints the boundary (no leading-gap error). This is the full ARC shape on 0-based ordinals; reuse the autonomous-cycle test harness.
b. Zero-based WITH system lead: m0 = system at ordinal 0, m1 = user at ordinal 1 → chunk_start == 1, fold succeeds (system exempted by the guard).
c. One-based session (TS shape): messages 1..N → chunk_start == 1 → everything works as today (regression).
d. Compartment ending at ordinal 0 (Some(0) floor): next chunk starts at 1; trigger offset = 1; no re-consumption of 0.
e. validate_stored_compartments: first compartment starting at 0 accepted; starting at 5 with a second at 9 rejected (interior gap); overlap still rejected.
f. boundary: trigger fires on a 0-based session where ALL content sits at ordinals 0..5 (previously the 1-floor silently dropped ordinal 0 from the estimate).

## Gates
cargo test -p mc-module -p mc-core -p mc-store --features mc-store/test-support; cargo test -p mc-module --test real_daemon; cargo test -p mc-module --lib (goldens intact); cargo clippy --workspace --all-targets -- -D warnings; cargo fmt --check; check_comments. Commit message: name the 0-based/1-based basis mismatch, the ordinal-0 stranding mechanism, and the Option-over-sentinel choice.
