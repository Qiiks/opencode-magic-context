## Finding 1: Non-empty compartment sync is rejected by Rust
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts:564-615`; `crates/mc-module/src/lib.rs:267-295`
- **Confidence**: high
- **Issue**: TS serializes each compartment as nested `start`/`end` objects, while Rust requires top-level `start_message` and `end_message` integers. Any state sync containing a compartment fails serde before a transform can be compared.
- **Evidence**: TS emits `start: { flat_id, bare_message_id, absolute_ordinal }` and the equivalent `end` (`shadow-sender.ts:584-604`). Rust requires non-default `start_message: i64` and `end_message: i64` (`lib.rs:267-270`). `invalid_params` is neither a peer reject nor connection failure, so the sender logs the error and repeatedly attempts the unacknowledged sync rather than resetting/recovering (`shadow-sender.ts:887-904`).
- **Suggested Fix**: Emit the Rust contract exactly—top-level ordinal and ID fields—or change Rust to explicitly deserialize the nested contract. Add an end-to-end TS-built, flat-wire serde test with at least one compartment.

## Finding 2: Shadow capture is synchronous, unbounded, and partly outside fail-open protection
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/transform.ts:459-462`; `shadow-sender.ts:226-228, 361-408, 483-512`
- **Confidence**: high
- **Issue**: The observe-only lane adds synchronous full-message cloning and DB work to the live transform path. Worse, the initial clone and declared-trim lookup occur before the later `try/catch`, so an exception can reject the live transform rather than merely lose shadow coverage.
- **Evidence**: `cloneForShadow(messages)` and `resolveDeclaredTrimForShadow()` execute before the shadow enqueue guard (`transform.ts:459-462`; the guard begins at `2186`). The JSON clone is an unbounded serialize/parse (`transform.ts:177-179`; `shadow-sender.ts:226-228`). Enqueue then clones input again (`shadow-sender.ts:380`) and clones output again (`:288`), while resolving ordinals via a full raw-session read (`:375-378`). No queue cap limits this pre-queue work.
- **Suggested Fix**: Enclose all shadow preparation in fail-open handling; impose byte/message/time caps and skip oversized passes; eliminate duplicate clones; and move raw-session/ordinal preparation off the transform return path or reuse already-computed transform metadata.

## Finding 3: Recompaction silently leaves stale compartments in the shadow store
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-recomp.ts:110-123`; `shadow-sender.ts:646-647, 696-706`; `crates/mc-module/src/lib.rs:184-200`; `crates/mc-store/src/lib.rs:3991-4036`
- **Confidence**: high
- **Issue**: Recomp deletes and recreates all live compartments, often reusing old sequence numbers. The sender only transmits sequences above its prior high-water mark, so it sends none of the recreated rows. It emits an `m0_mutations` record intended to signal this change, but Rust’s state-sync wire does not define or consume that field. The shadow store therefore retains obsolete compartments.
- **Evidence**: Recomp explicitly `DELETE`s all compartments, reinserts staging rows, and records `recomp_boundary_change` (`compartment-runner-recomp.ts:110-123`). Sender skips every compartment with `sequence <= acked.compartment_sequence` (`shadow-sender.ts:646-647`) and sends `m0_mutations` (`:696-706`). Rust accepts only compartments, memories, and memory mutations (`lib.rs:184-200`) and only upserts supplied compartments—there is no deletion/replacement operation (`mc-store/src/lib.rs:3991-4036`).
- **Suggested Fix**: On any m0/recomp revision, reset the shadow lineage and force a full snapshot, or add an atomic authoritative-replace state-sync mode that deletes absent shadow compartments. Do not acknowledge the m0 watermark until Rust has applied the revision.

## Summary
**3 high findings. Verdict: HOLD.** The most important reason is Finding 2: a production shadow soak is not observe-only when synchronous clone/DB work—and potential exceptions—runs before the live transform completes.

False-positive filtering performed: `packages/pi-plugin/PARITY.md` documents Pi/OpenCode host differences only; the smart-notes PARITY file concerns compiled SSRF checks, neither covers this lane. Existing tests do not close these gaps: `shadow-sender.test.ts` checks a state-sync payload with no compartment, while Rust’s shadow test hand-constructs Rust-shaped `start_message`/`end_message` fields (`crates/mc-module/src/lib.rs:7067-7089`). I also found the shadow-binding and plain-transform isolation guards present, and the ordinal full-read/by-ID paths use the same ordering and summary filtering, so I did not report those as defects.