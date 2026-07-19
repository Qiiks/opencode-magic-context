# Task: mc-module — neuter the nothing-survives truncate into pending-raw-loud (Layer-2 rewrite safety)

Repo: this worktree (branch off subc-migration). Crates: `crates/mc-module`, `crates/mc-store`. This brief is self-contained — do not look for external plan files.

## Background (why)

The #423 re-cut arm in `crates/mc-module/src/transform.rs` handles "the incoming array no longer matches the held lineage". It has two branches:
- SURVIVING-PREFIX: the incoming array is a prefix of the held conversation (revert, store-marker restart) → truncate compartments to the shared point. This branch has positive evidence and KEEPS its behavior unchanged.
- NOTHING-SURVIVES (`surviving_revert_prefix_seq == -1`-class, the truncate-all + epoch-bump + bootstrap path, around transform.rs:1452-1460): fires on boundary-absence alone. Upstream (the ai-proxy layer) now separates conversations with a composite session key — CC Task subagents via agent_id, /clear and native-compact via a monotonic epoch component minted on lineage switches — so a legitimately-new conversation ALWAYS arrives on a FRESH key with no prior MC state and takes the normal bootstrap path. Therefore any boundary-absent array arriving on a key WITH existing compartments is either an upstream detection miss or foreign/misattributed traffic — and truncating on it destroys a real lineage. The destructive branch must be ELIMINATED, not gated: the invariant is that a missed detection degrades to raw pass-through with a loud alarm, never to a wrong truncation.

## The change

### 1. Neuter nothing-survives (transform.rs)

When the incoming array is boundary-absent AND shares no prefix with the held lineage AND the session HAS existing compartments/durable lineage state:
- Do NOT truncate. Do NOT bump revert_epoch (only a committed truncate owns that bump; publish CAS rejects on mismatch — bumping on arm would poison concurrent publishes).
- Serve raw pass-through (`TransformResponse::passthrough`, transform.rs ~298-315) — never-compact degrade.
- Arm a durable `pending_rewrite` state in ModuleMeta: `{ armed_at_ms, absent_shape_fingerprint, absent_request_count (diagnostic only), last_present_at_ms }`. ARM-ONCE semantics: one CAS commit when arming; subsequent pending passes are pure pass-through with NO durable writes (defer-pass row stability; see mc-store commit discipline around lib.rs:1203-1209 "call ONLY when durable state changed").
- LOUD diagnostics: a `last_failure`-style durable detail naming the state and expected causes (upstream lineage-switch detection miss, or foreign traffic on this key), plus log lines. Terminal: NO commit predicate, nothing ever auto-truncates out of pending.

A fresh key with NO prior MC state keeps taking the normal bootstrap path — that distinction (bootstrap-vs-pending) is exactly "does this session have prior durable lineage state".

### 2. Pending-pass isolation (mechanical pins — each is load-bearing)

While `pending_rewrite` is armed, a pass whose array is still absent-shape must:
- BYPASS `enforce_block_identity` (runs right after load, transform.rs ~536-538) — a foreign array must not produce IdentityDrift state.
- BYPASS `apply_ingress_meta` (transform.rs ~1068-1076) — do not persist foreign block-identities or usage into the session's meta.
- BYPASS classifier/scheduler durable side-effects — a pending pass must not set `reconcile_pending` (normal defer can set it at transform.rs ~935-941, which would route a later pass to Hard via cortexkit-cache-core lib.rs:128-130).
- SUPPRESS historian work: the handler's Emergency95 arms (`crates/mc-module/src/lib.rs` ~1125-1282, `prepare_historian_fire`) must not fire for a pending session — add an explicit pending guard producing a `no_fire=pending_rewrite`-style diagnostic. A ≥95% absent-shape request stays raw + loud (the client holds the raw array; overflow is its native problem — the honest degrade).

### 3. Recovery and alarms

- RECOVERY: a pass whose array IS boundary-present (extends the held lineage) while pending is armed:
  - If it arrives while the absent-shape traffic pattern indicated a lineage switch that upstream has since handled (i.e., pending armed, then present traffic resumes and no absent traffic conflicts), clear `pending_rewrite` and resume normal operation (one CAS commit).
  - BUT if it INTERLEAVES with continuing absent-shape traffic (present arrives while pending, then absent again), that's two conversations multiplexed on one key = upstream keying breach → keep the lineage, set a durable `ambiguous` flag, fail LOUD. Implement the trip as: any boundary-present arrival while pending clears pending but increments a durable trip counter; re-arming within the same session bumps it; past a small threshold (e.g. 3 arm/clear cycles) set `ambiguous` + loud diagnostic. (This is the rate-breaker/interleave alarm; in normal operation with upstream composite keys it should NEVER fire.)
- All alarm states keep serving: present-shape traffic gets normal transform service; absent-shape gets pass-through.

### 4. Canary test (coupling enforcement — REQUIRED)

MC's boundary/absence basis fingerprints `flatten_block` bytes (includes block-level provider_extras); the upstream switch-detection matches role+kind only. They agree because the submit-strip removes churning fields (cache_control) before MC sees the array. Write a canary test: construct an array whose blocks carry a synthetic block-level `provider_extras` field, run it through the same ingress path a real pass uses (post-submit-strip semantics), and assert the pending arm does NOT arm when the array is otherwise a legitimate extension — i.e., a provider_extras-only difference must never register as boundary-absence. Comment the test with the two-sided invariant: any per-turn-churning field must be absent from both fingerprint bases; a change to either basis must change both together.

## Tests (all non-vacuous; use existing transform test fixtures/store fixtures as patterns)

1. Nothing-survives neutered: session with compartments + boundary-absent share-nothing array → pass-through response, compartments INTACT, pending armed (durable), diagnostic present. Repeat pass: byte-stable output, NO additional CAS write (assert row_version stable).
2. Surviving-prefix unchanged: existing prefix-shrink truncate tests keep passing untouched.
3. Fresh-key bootstrap unaffected: no prior state + boundary-absent → normal bootstrap (existing behavior), NOT pending.
4. Recovery: pending armed → boundary-present extension arrives → pending cleared, normal service, compartments intact, no truncate ever happened.
5. Interleave/rate alarm: repeated arm/clear cycles → ambiguous flag + loud diagnostic, lineage intact.
6. Emergency-on-pending: ≥95% usage absent-shape request → raw pass-through, `prepare_historian_fire` NOT invoked (assert via the existing test seams for historian spawn), no fold.
7. Ingress-meta isolation: pending pass leaves block-identity map, usage meta, and reconcile_pending untouched (compare ModuleMeta before/after).
8. Crash/restart: pending_rewrite persists in ModuleMeta; a restarted handler resumes pending semantics (no reset, no truncate).
9. Canary (section 4).
10. Composite-key hygiene: two session keys differing only by a suffix component scope independently (two frozen sets, two lineages); a composite key never triggers the `mc-historian:` prefix exemption.

## Gates

- `cargo test -p mc-module -p mc-store`, `cargo clippy -p mc-module -p mc-store -- -D warnings`, `cargo fmt --check`, `check_comments`.
- Comments explain invariants (eliminated-not-gated, arm-once, bump-on-truncate-only), never reference this task, plan versions, or review rounds.
- Do NOT touch: manifest(), dispatch_value routing, memory_tool.rs, selection/reduction code, the surviving-prefix truncate logic itself.

## Style pins

- SQLite binds: spread positional args, never array-form. No 0-as-sentinel for ordinals/sequences — Option<T>.
- Commit with trailer: `Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>`
