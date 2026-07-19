# Marker lifecycle: Oracle remediation (2 BLOCKER + 1 HIGH + 1 LOW) — structural reconciler, not more patches

Branch from `subc-migration` HEAD (8fac08a1). Cache-core (rule #4975). An adversarial Oracle audited the deferred-marker lifecycle after the two flip fixes and found the remaining holes below, each with a concrete two-pass byte-flip. Fix them with ONE structural change plus two targeted ones, not four point patches.

## The structural insight (implement this shape)

The plugin doctrine is replay-everything: every persistent mutation re-applies deterministically on EVERY pass (ARCHITECTURE.md, mc:protected section). The marker representation violates this today: injection/removal happen only on the `applied` arm of the drain, so every other pass shape (loser process, already-current, crash recovery, retry) serves whatever its stale array happens to hold. Replace the applied-arm-only mutation with an UNCONDITIONAL MARKER REPRESENTATION RECONCILER that runs in postprocess on every pass for marker-bearing sessions:

reconcileMarkerRepresentation(messages, persistedMarkerState):
1. Remove every message in the served array whose info.summary === true AND whose id is NOT the persisted summaryMessageId (covers advance losers, duplicate rows, stale arrays).
2. Ensure the persisted summary representation is present EXACTLY ONCE at the CANONICAL POSITION (see BLOCKER 1 below), with the identical bytes the apply pass would serve (tag from the persisted content-id reuse path, ctxReduceCallable gating as today).
3. If persisted marker state is absent: remove any summary-flagged message from the served array.

This single function, called unconditionally (both on drain passes after apply and on non-drain passes), converges every process and every failure arm to one representation. It must be deterministic and byte-stable (idempotent: running it twice changes nothing), and must run at the SAME postprocess position the current inject runs (after tagging/heuristics/caveman, before final normalization) so the cleared-surface analysis from the audit stays valid.

## BLOCKER 1 — canonical position is wrong (role-anchored instead of position-anchored)

transform-postprocess-phase.ts:152-159 inserts before the first non-summary ASSISTANT. When the retained tail begins with a real user message U, the drain serves [m0,m1,U,summary,A] but the next pass serves [m0,m1,summary,U,A] (the summary row precedes the whole retained tail in the DB projection). Wire role sequence flips = guaranteed bust.

Fix: the canonical position is the START of the retained tail — immediately after the synthetic prefix (m0/m1 and any other synthetic-flagged head messages), BEFORE the first real tail message regardless of its role. Determine the prefix boundary structurally (synthetic flag / the known m0-m1 ids), not by role scanning. VERIFY against the real next-pass projection: reconstruct from opencode.db what filterCompacted + trim actually yields after an advance (summary first, then tail) and match it exactly. Add fixtures: retained tail starting with a USER message (both directions of the lifecycle), and the joined-assistant shape (summary + adjacent assistant with tool_use) asserted through the merge serializer.

## BLOCKER 2 — concurrent drains serve stale loser arrays

No ownership claim before applyDeferredCompactionMarker mutates the DB; already-current and absent-pending arms never reconcile the caller's array (transform-postprocess-phase.ts:1360, compaction-marker-manager.ts:230-242). Loser process serves S0 then S1 next pass.

Fix: the unconditional reconciler covers this by construction (loser reads persisted state = S1, reconciles its array to S1 before serving). Add a two-array test: simulate two callers around one pending marker; the loser's postprocess output must be byte-identical to the winner's next-pass rebuild.

## HIGH — post-insert crash window mints duplicate markers (non-idempotent retry)

compaction-marker.ts:393-423 commits OpenCode rows; state persist afterwards can fail (compaction-marker-manager.ts:303-307, caught retryable at :319-334); marker IDs carry random suffixes (compaction-marker.ts:49-58) so the retry inserts a SECOND summary row (S2) alongside S1.

Fix, two layers: (a) make marker/summary row IDs DETERMINISTIC — derived from (sessionId, boundary end message id) so a retry after a post-insert crash finds the existing rows (insert becomes upsert/no-op) and never mints S2; verify the id format remains compatible with OpenCode's projection (they are ordinary message/part rows; keep the required prefixes and length shape — see clone-fix lessons: OpenCode compares message ids LEXICOGRAPHICALLY, so the deterministic id must still sort correctly relative to neighbors; derive the time-prefix from the boundary row's timestamp, deterministic suffix from a hash). (b) recovery reconciliation: on drain, delete any OTHER summary rows for this session's marker lineage found adjacent to the boundary (defense against rows minted by older binaries), inside the same transaction discipline as the current delete.

## LOW — removed summary tag rows linger active

Old summary deletion (compaction-marker.ts:453-456) never drops its tag row; unreachable-but-active tags pollute counts/age-windows/caveman ranking. Fix: when the reconciler or the advance path removes a summary whose id differs from persisted, mark its tag dropped through the existing drop machinery (idempotent).

## Tests (fail-first for each finding)

- Leading-retained-user two-pass byte equality (first-apply AND advance) through serializeAnthropicWireWithAdjacentAssistantMerge.
- Joined-assistant seam shape (summary + assistant-with-tool_use adjacency).
- Two-process loser reconciliation (BLOCKER 2 scenario byte-for-byte).
- Post-insert crash retry: simulate state-persist failure after insert; retry must not create a second summary row; next serve byte-identical to a clean apply.
- Reconciler idempotence: double invocation changes nothing; absent-marker sessions untouched byte-for-byte (guard against regressing sessions with no marker).
- Old-tag drop on advance.
- Keep ALL existing marker fixtures green.

## Gates

Full plugin suite + focused marker/postprocess/cache-replay suites; typecheck; biome. Report: the reconciler's position derivation explained against the real projection (with a reconstructed row table), each finding mapped to its test, fail-first proof per finding. Comments explain invariants (canonical-position law, replay-everything doctrine), never audit history. No em-dashes.
